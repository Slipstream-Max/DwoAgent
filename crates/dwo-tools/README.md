# dwo-tools

Provider-neutral tool execution for the dwoagent rewrite. This crate is not yet
wired into the legacy agent runtime.

## Ownership

```text
AgentService
|- Arc<ToolPolicyEngine>
|- Arc<FileEditManager>     shared by every session; FIFO mutation lock
`- SessionAgent
   `- ToolManager           one per loaded session
      |- TerminalManager    one registry per session
      |- Arc<FileEditManager>
      `- Arc<ToolPolicyEngine>
```

`TerminalManager::list` and `shutdown_all` are internal service APIs. The model
only sees `terminal` and `file_edit`; there is no `wait` or terminal-list tool.

## Call path

```text
raw calls
  -> ParsedToolCall::parse
  -> ToolManager::execute_batch
  -> ToolPolicyEngine::authorize(intent, SessionMode) (exactly once)
  -> optional confirmation callback
  -> TerminalManager | shared FileEditManager
  -> ordered ToolResult list
```

Terminal calls in one batch execute concurrently. A batch may contain at most
one `file_edit`; multiple file-edit calls are rejected without applying any of
them. One patch may still contain operations for multiple files. The shared
`FileEditManager` mutex serializes accepted edits across sessions. Results
retain input order. Invalid arguments and unknown names return per-call errors
and do not abort the batch. Tool-call ID normalization and deduplication belong
to the LLM client boundary.

Duplicate file-edit rejections use the stable `multiple_file_edit_calls` error
code. The context layer uses this code to redact rejected patch bodies while
preserving the assistant tool-call and tool-result protocol pair.

## Terminal actions

- `run`: starts a PTY or closed-stdin pipe and waits for exit or `yield_ms`.
- `input`: writes to a PTY, then waits for output, exit, or `yield_ms`.
- `input` with empty `data`: event-driven poll without writing.
- `kill`: terminates the process tree and drains trailing output.

Output stays as bytes internally. Each terminal retains at most 1 MiB. Model
results retain the head and tail with an omission marker and are capped at
20,000 UTF-8 bytes, a conservative upper bound for a 10,000-token budget.

## Policy

Global terminal deny rules take precedence. `full_access` otherwise allows
terminal and file operations. `confirm` automatically allows configured or
simple read-only commands and confirms everything else. `watch` only permits
simple read-only commands and rejects file edits. `run`, `input`, `kill`, and
`file_edit` each pass through the same single authorization point.

`SessionMode` is the one persisted mode type. There is no second agent-level or
tool-level mode enum. A tool batch captures its mode when it is dispatched;
changes apply to the next batch.
