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
   |- skills/<skill>/SKILL.md
   `- mcp.json
```

```yaml
name: coder
description: coding agent
policyMode: confirm
model:
  defaultModelId: deepseek-v4-pro
  providers:
    deepseek:
      type: deepseek
      baseUrl: null
      apiKeyEnv: DEEPSEEK_API_KEY
  models:
    - modelName: deepseek-v4-pro
      provider: deepseek
      modelId: deepseek-v4-pro
      compactThreshold: 0.5
```

There are no tool switches. Provider credentials and an optional complete
`baseUrl` override are declared once per provider instance. Model entries may
override context/output limits, compact threshold, and default reasoning mode;
transport settings and reasoning request parameters remain catalog-owned.

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
state.

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
when idle; when a turn is active it atomically cancels that turn and starts the
queued replacement after cancellation finishes.

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
     -> on ContextLengthExceeded, compact and retry that request once
     -> stream assistant deltas
     -> without tool calls, persist the complete assistant message
     -> with tool calls, ToolManager::execute_batch using current SessionMode snapshot
     -> persist assistant tool calls and all paired results in one checkpoint
     -> repeat model/tool steps
  -> TurnCompleted | TurnCancelled | TurnFailed
```

`ModelClient` is implemented by the provider-configured HTTP client in
`dwo-model-client`. A scripted implementation lives under `tests/support` for
deterministic tests. User turns use streaming requests; compaction summaries
use the non-streaming `ModelClient::summarize` path.

Compaction keeps the latest three user turns after filtering watcher/runtime
messages. Tool calls and results remain paired in their provider-compatible
shape; large call arguments are replaced with bounded omission markers while
stored results remain unchanged. Older history is summarized. A model switch
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
errors do not masquerade as context errors.

Every normal provider response must include usage. Its reported total becomes
the current context-token snapshot; values are never accumulated across turns.
Compaction or an image downgrade clears the snapshot to zero until the next
normal model response. Changing models immediately publishes that current value
against the target model's context-window size.

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
