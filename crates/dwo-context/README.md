# dwo-context

Owns the model-visible context for the rewrite. It does not call a model.

```text
ContextManager
|- item-first model history (index 0 is the current system message)
|- provider instance for which history is normalized
|- SystemPromptBlock source metadata + watcher baseline
|- current token estimate for the complete model request
`- compaction state
```

`SystemPromptBuilder` reads these fixed profile resources:

```text
<agent-profile>/resource/
|- prompts/System.md
|- prompts/AGENTS.md
|- skills/<skill>/SKILL.md
`- mcp.json
```

Rules also include `AGENTS.md` files at the session's initial `cwd` and any
persisted extra `RuleSource`, such as a Board Topic's Knowledge file. Every
rule snapshot carries the source file path, the `pwd` where its instructions
apply, and its content. All configured rule paths are watched. Changes to the
system prompt, rules, skills, MCP config, or environment append `EnvWatcher`
messages at model-step boundaries. They do not mutate the existing system
prompt.

Responses output is stored item-first. Reasoning, assistant messages, local
function calls, and hosted calls occupy separate ordered history entries rather
than one aggregated assistant message. Reasoning and hosted calls carry their
provider-instance owner; visible messages and local function call/output pairs
are provider-neutral. Switching providers permanently removes provider-owned
items from model history. Switching to a model without image input permanently
removes image blocks. The client transcript is independent and remains intact.

Compaction is split into preparation and replacement phases. The model client
owns the non-streaming summary request:

```text
ContextManager::scheduled_compaction(trigger_tokens, tools)
  -> refresh the complete message, reasoning, image, tool-call, result, and schema estimate
  -> decide whether the model-specific trigger has been reached
  -> return no plan when compaction is not needed
  -> otherwise build the default CompactionPlan
  -> reserve approximately 20K newest context tokens, cutting inside a turn when needed
  -> keep the split turn's user question with the retained agent suffix
  -> pass the complete summary prefix to the summary model without history filtering
  -> allocate a shared 5K raw-user-token budget across front users and the reserve
  -> filter only tool content in the retained reserve while preserving call/result pairs
  -> ModelClient summarizes the view
ContextManager::apply_compaction(summary)
  -> rebuild current SystemPromptBlock
  -> replace model history with system + front users + summary + filtered reserve
  -> immediately re-estimate the complete replacement context
  -> reset watcher baseline
```

Tool exchanges retain item-first `function_call` entries plus paired output
messages. Legacy aggregated assistant calls are normalized on load. File-edit
patches are replaced with bounded omission markers.
Terminal commands remain unchanged, while terminal result `output` values are
replaced with the shared content-omission marker; status and other small result
fields remain. This filtering is applied only to the retained reserve; the
summary input is the unfiltered model-visible history. Images and reasoning are
preserved by normal compaction. Text is estimated in tokens rather than UTF-8
bytes, with a conservative non-ASCII estimate and a fixed image estimate.

The initial system message is excluded while finding the split and is rebuilt
from the current profile when replacement is applied. The final model context
is always `system + front user messages + summary + reserve`. A split turn is
represented in the summary as its user question plus the removed agent prefix,
and in the reserve as the same user question plus the newest agent suffix.

Live tool calls and results are stored unchanged. Current usage is recomputed
from the complete context and tool schemas after each message checkpoint; model
provider input/output usage is not accumulated or used as the session context
counter. The model trigger is based on `contextWindowTokens - maxOutputTokens`
and the profile `compactionTriggerRatio`, with no additional fixed headroom.

Handoff compaction uses the same plan and replacement path, except the summary
comes from the model-provided `handoff_text` instead of a summary request.
`ContextManager::remove_tool_call` first drops the handoff tool-call exchange
from model history; `apply_compaction` then rebuilds the context with the
handoff summary and the caller appends a `<handoff_continuation>` runtime
message so the same turn continues. The client transcript is untouched.
