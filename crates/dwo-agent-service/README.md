# dwo-agent-service

Standalone session runtime for the dwoagent rewrite. The `dwo-agent` binary
hosts it behind local IPC and adapts ACP and Weixin clients to this API.

## Agent profile

The rewrite profile has one strict `profile.yaml` and fixed resource paths:

```text
<agent-profile>/
|- profile.yaml
`- resource/
   |- prompts/System.md
   |- prompts/AGENTS.md
   |- models/<family>.yaml
   |- skills/<skill>/SKILL.md
   `- mcp.json
```

```yaml
policyMode: confirm
model:
  default:
    model: deepseek/deepseek-v4-pro
    reasoning: High
  compactionTriggerRatio: 0.5
  providers:
    deepseek:
      apiKeyEnv: DEEPSEEK_API_KEY
```

There are no tool switches or provider protocols. Official family names supply
their URL and complete model list. Custom providers declare one API root and a
display-name map whose entries select an upstream `modelId` and Model List
`profile`. User Model Lists live under `resource/models/`.

`load_profile(path)` canonicalizes one profile root, loads `profile.yaml`,
resolves it against the built-in model catalog, and validates the fixed prompt
resources. `profile.full.yaml` documents every supported profile field.

## Persistent shape

```text
sessions/YYYY/MM/DD/<session-id>/
|- session.json              identity, cwd, title, mode, timestamps, LLM settings
|- model_context.json       system prompt, model messages, current usage, compaction state
`- client_transcript.jsonl  append-only client-visible event stream for replay
```

Runtime phases (`Idle`, `Running`, `WaitingPermission`, `Cancelling`, and
`Closing`) are actor state and are never persisted as authoritative session
state. If a persisted title is empty, list/load repairs it from the first user
prompt in the transcript, normalized and capped at 10 Unicode characters. A
session with no history applies the same rule when its next prompt arrives.

## Runtime ownership

```text
AgentService
|- SessionRepository
|- ModelClient
|- optional AgentProfile root
|- Arc<FileEditManager>
|- Arc<ToolPolicyEngine>
`- loaded: SessionId -> Arc<SessionAgent>
   `- SessionAgent actor
      |- SessionRecord / ContextManager ownership
      |- one active PromptTurn
      |- one session-local ToolManager / TerminalManager
      `- sequence-numbered session event stream
```

Loading an active ID returns the existing actor. Repository `list` does not load
sessions. Different session actors run concurrently. `prompt` starts immediately
when idle. During an active turn, user and internal messages enter one FIFO and
are appended after the current model response or tool-call batch. User messages
keep the turn running; watcher-style internal messages can be appended without
waking another model step. Explicit cancellation clears queued user messages,
preserves internal messages, and is the only prompt path that interrupts a turn.

The filesystem repository serializes save/load/delete only within the same
`SessionId`. Records for different sessions do not share a filesystem write
lock.

`close` cancels and unloads a session but retains its record for a later load.
`delete` closes the actor first, removes the repository record, and prevents a
concurrent load from recreating the actor while deletion is in progress.

## Session configuration

`SessionAgent::set_config` and `AgentService::set_config` accept one strongly
typed `SessionConfigUpdate`:

```text
Mode(SessionMode)
Model(String)
Reasoning(Option<String>)
```

The service persists a valid update before publishing `ConfigChanged`. A mode
change applies to the next tool batch. Model and reasoning changes apply to the
next model request. Work already dispatched keeps its captured configuration.

Switching from an image-capable model to a text-only model is a context
migration. When the session is idle, the service first asks the current (or
last successfully used) image-capable model to summarize every image-bearing
turn, rebuilds `model_context.json` without image blocks, and only then persists
the target model. The client transcript remains unchanged for replay. A failed
summary leaves both model and context unchanged. A downgrade is rejected while
an image turn is active, and a text-only model rejects new image prompts before
they are persisted.

## Turn call path

```text
SessionAgent::prompt(origin, user_message)
  -> persist user checkpoint
  -> AgentLoop with session SystemPromptBlock
     -> scan mutable profile/environment state and append watcher changes
     -> compare last turn input usage with the selected model compact trigger
     -> compact old history with the last successful model when the trigger is reached
     -> ModelClient::stream_turn
     -> on transient model failure, persist partial output and retry the same step up to five times
     -> on ContextLengthExceeded, compact and retry that request once
     -> stream assistant deltas
     -> without tool calls, drain queued user/internal messages after the response
     -> with tool calls, ToolManager::execute_batch using current SessionMode snapshot
     -> drain queued user/internal messages after the tool-call batch
     -> persist the complete response/tool/message checkpoint
     -> repeat model/tool steps
  -> TurnCompleted | TurnCancelled | TurnFailed
  -> persist the latest plan as a non-waking PlanWatcher for the next explicit turn
```

`ModelClient` is implemented by the provider-configured HTTP client in
`dwo-model-client`. A scripted implementation lives under `tests/support` for
deterministic tests. User turns use streaming requests; compaction summaries
use the non-streaming `ModelClient::summarize` path.

Compaction estimates the complete model request and keeps approximately the
newest 20K tokens. It can split a turn: the user question is retained with the
newest agent suffix while the removed prefix is summarized. The summary input
is unfiltered; only retained tool content is reduced, while tool calls and
results remain paired. A shared 5K token budget covers raw user messages in the
front section and reserve. A model switch
uses the last successfully used model for that summary, preserves the session's
current reasoning mode, and records the target as `last_model` only after its
first successful turn request.

Image capability downgrades use a stricter plan: the summary request retains
all image blocks so the source model can describe them, while the rebuilt model
context retains no images at all. Switching back to an image-capable model does
not restore compacted images; the original blocks remain available only in the
append-only client transcript.

Only an explicitly classified `ContextLengthExceeded` response enters reactive
recovery. Authentication, rate-limit, network, and other invalid-request
errors do not masquerade as context errors. Transient network, provider,
protocol, and stream-interruption errors use the shared model retry policy.
Retries keep the same turn active, absorb pending user/runtime messages before
the next request, and preserve partial assistant content in both transcript and
model context. A permanently failed or retry-exhausted turn remains idle.

## Execution plans

Each session may persist one current `ExecutionPlan`. The model-facing `plan`
tool supports only `get` and `update`; an empty update clears the plan, and a
plan whose entries are all `completed` or `cancelled` is removed from current
session state after its terminal update is published.

Plans never start, resume, or queue turns. When an agent turn reaches any
terminal outcome, the actor places the latest unfinished plan in the normal
pending buffer as a non-waking `PlanWatcher`. The idle drain replaces the
previous watcher in model context and persists it. A later user prompt or
explicit `/resume` naturally includes that watcher. Reloading a session only
restores the plan and watcher; it does not call the model. Manual compaction
does not create a plan watcher. Context compaction carries an existing
`PlanWatcher` separately from summary history, preserving its position before
the next user message so later plan updates or clears cannot leave stale plan
text inside a compaction summary.

Context usage is recomputed from system prompt, messages, reasoning, images,
tool calls/results, and tool schemas after each checkpoint. Provider input and
output usage is optional transport metadata and is not accumulated. The model
trigger uses `contextWindowTokens - maxOutputTokens`, multiplied by the profile
`compactionTriggerRatio`; changing models immediately publishes the estimate against
the target model's context-window size.

## Observation and control

`attach(endpoint)` returns a snapshot plus a filtered live receiver. The actor subscribes
before capturing snapshot sequence `N`; the returned stream only forwards
events with `seq > N`, so replay/snapshot and live delivery do not have a gap.
The snapshot includes the persisted record, current phase, active turn, partial
assistant output, active tools, pending permission, and sequence watermark.

Any endpoint may cancel the current turn or resolve a pending permission; the
first permission response wins. User prompts are broadcast to observers but
filtered from the matching origin endpoint to avoid client-side echo. Close
waits for cancellation checkpoints before unloading the actor.

Internal messages are persisted as `MessageKind::Runtime` context messages and
do not emit `UserPromptSubmitted`. A waking internal message starts an idle
session immediately. Cancellation preserves queued internal messages but
suppresses their wake behavior after the cancelled turn finishes.
