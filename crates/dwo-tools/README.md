# dwo-tools

Provider-neutral tool execution used by `dwo-agent-service`. Each loaded
session owns a `ToolManager`, while policy and file-edit coordination are
shared by the service.

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
only sees `terminal`, `read_file`, and `file_edit`; there is no `wait` or
terminal-list tool. `read_file` pages UTF-8 text at 500 lines and injects
supported image content only when the selected model accepts image input.

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

A single `terminal` tool covers every operation; there is no `action` enum and
no required field:

- no `terminal_id`: starts a new interactive PTY and runs `command`.
- `terminal_id` with `command`: sends the command as input (trailing newline).
- `terminal_id` without `command`: event-driven poll for incremental output.
- `terminal_id` with `kill: true`: terminates the process tree and drains
  trailing output before returning.
- New terminals always start in the session workspace. Terminal tool input has
  no `cwd` field.
- `timeout_ms` bounds a new terminal's total runtime (default 10 min);
  `yield_ms` bounds each wait for output (default 60 s). Unknown `terminal_id`
  values are reported as errors and never recreated.

Output stays as bytes internally. Each terminal retains at most 1 MiB. Model
results retain the head and tail with an omission marker and are capped at
20,000 UTF-8 bytes, a conservative upper bound for a 10,000-token budget.

## Policy

Global terminal deny rules take precedence. `full_access` otherwise allows
terminal and file operations. `confirm` automatically allows configured or
simple read-only commands and confirms everything else. `watch` only permits
simple read-only commands and rejects file edits. Every terminal intent
(run, input, kill) and `file_edit` passes through the same single
authorization point.

`SessionMode` is the one persisted mode type. There is no second agent-level or
tool-level mode enum. A tool batch captures its mode when it is dispatched;
changes apply to the next batch.
