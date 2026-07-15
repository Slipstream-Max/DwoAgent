# dwo-context

Owns the model-visible context for the rewrite. It does not call a model.

```text
ContextManager
|- model history (index 0 is the current system message)
|- SystemPromptBlock source metadata + watcher baseline
|- complete client transcript
|- cumulative usage + last turn input-token size
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

Rules also include the `AGENTS.md` at the session's initial `cwd`. Only the
profile and initial-cwd rule paths are watched. Changes to system prompt,
rules, skills, MCP config, or environment append `EnvWatcher` messages at
model-step boundaries. They do not mutate the existing system prompt.

Compaction is split into preparation and replacement phases. The model client
owns the non-streaming summary request:

```text
ContextManager::plan_compaction(CompactionPlanner)
  -> compact tool-call arguments while preserving call/result pairs
  -> split history before the latest three real user turns
  -> remove reasoning/internal messages from the older summary view
  -> allocate one shared 20K UTF-8 user-message budget across history and latest turns
  -> ModelClient summarizes the view
ContextManager::apply_compaction(summary)
  -> rebuild current SystemPromptBlock
  -> replace model history with system + historical users + summary + filtered latest turns
  -> keep transcript and usage unchanged
  -> reset watcher baseline
```

Tool exchanges retain their standard assistant `tool_calls` plus paired tool
result messages. File-edit patches are replaced with bounded omission markers.
Terminal commands remain unchanged, while terminal result `output` values are
replaced with the shared content-omission marker; status and other small result
fields remain.
The older summary view then removes reasoning, watcher, permission, config, and
runtime messages.

The latest three turns retain assistant reasoning and text after watcher and
runtime messages are removed. Images in these turns are preserved and do not
consume the UTF-8 text budget. Historical images are removed before both the
summary request and historical-user retention. Recent real user messages
consume the shared 20K budget first, newest to oldest; only the remaining bytes
can retain historical user messages before the summary. When there is no older
history, argument-only filtering does not call the summary model.

Live tool calls and results are stored unchanged. Terminal output is already
capped at 20K UTF-8 bytes by the tool before it reaches model context; the
compaction view applies the tool-specific omission rules above.
