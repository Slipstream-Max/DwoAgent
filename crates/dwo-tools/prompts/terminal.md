---
name: terminal
description: Run commands, interact with or poll terminals, and kill them. Every terminal is interactive; commands are sent as input with a trailing newline.
---

# Use Cases

Use `terminal` to inspect the workspace, run builds and tests, execute programs, start long-running processes, or interact with a process that requires input.

# Parameters

The terminal always uses the session workspace. There is no `cwd` parameter.

- `command`: Command for a new terminal, or input for an existing `terminal_id` (include a trailing newline). Empty or omitted means poll when `terminal_id` is present.
- `terminal_id`: Existing terminal ID. Empty or omitted means create a new terminal from `command`.
- `kill`: `true` kills `terminal_id`; `false` means run, input, or poll.
- `yield_ms`: Maximum wait for this call. Default 60000.
- `timeout_ms`: Total lifetime of a new terminal. Default 600000; ignored for existing terminals.

# Call Shapes

- `{"command": "cargo build"}` — new terminal, run the command.
- `{"command": "cargo build", "yield_ms": 3000}` — return after at most 3s.
- `{"terminal_id": "term-1"}` — poll for incremental output.
- `{"terminal_id": "term-1", "command": "ls\n"}` — send input to the existing terminal.
- `{"terminal_id": "term-1", "kill": true}` — terminate the terminal.

When the endpoint supports sparse arguments, send only fields used by the selected shape: never send an empty `terminal_id`, `kill: false`, or unchanged defaults. Some endpoints fill every schema field; those completed values are accepted, but `cwd` must never be sent because it is not part of the schema.

# Results

- `running`: the process is still alive; `terminal_id` lets you continue.
- `completed`: the process exited successfully.
- `error`: the process exited unsuccessfully or the operation failed (for example, an unknown `terminal_id`).
- `cancelled`: the process was terminated.

Results include only output not already returned to the model. `exit_code` is present after process exit when the platform provides it.

# Notes

- Different terminals run concurrently. Calls targeting the same `terminal_id` are serialized.
- A directory change inside one command does not affect later new-terminal calls. Every new terminal starts in the session workspace.
- Every terminal accepts input (PTY). Commands that wait on stdin (like `cat` or `read`) hang until you send input or `kill` them.
- Keep `yield_ms` short enough to stay responsive. Continue a running process by polling its `terminal_id` rather than starting the same command again.
- Output is decoded as lossy UTF-8 at the model boundary. Large unread output is capped with its beginning and end preserved.

# Lifecycle

A terminal is bound to the process started by its command — a channel for that command, not a persistent console.

- While the process is alive and interactive (a persistent shell, a REPL, a command waiting on stdin, or a long-running task), you can keep sending input and polling it.
- One-shot commands (`cargo test`, `git status`) exit on their own. Once the process exits, the terminal is locked: further input fails with `terminal is not running`; results report a status (`completed`/`error`/`cancelled`) plus `exit_code`; after a short retention period (~5 minutes) the terminal is removed and later calls report `terminal not found`.
- Never reuse a terminal to start a new command after its process has exited — create a new terminal instead.
- For continuous use, start a persistent shell (e.g. `powershell`) and keep feeding it commands.
