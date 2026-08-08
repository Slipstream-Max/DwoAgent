---
name: terminal
description: Run commands, interact with or poll terminals, and kill them. Every terminal is interactive; commands are sent as input with a trailing newline.
---

# Use Cases

Use `terminal` to inspect the workspace, run builds and tests, execute programs, start long-running processes, or interact with a process that requires input.

# Parameters

- `command`: Command to run in a new terminal, or input to send to the terminal identified by `terminal_id` (include a trailing newline). Omit to poll for new output.
- `terminal_id`: ID of an existing terminal returned by a previous call. Omit to create a new terminal. An unknown ID is an error and is never recreated.
- `kill`: Set `true` to terminate the terminal identified by `terminal_id`; trailing output is drained before returning.
- `cwd`: Working directory for a new terminal. Relative paths are resolved from the session workspace. Ignored when `terminal_id` is given.
- `yield_ms`: How long to wait for output before returning (ms). The command keeps running; a `running` result includes a `terminal_id` to continue. Default 60000.
- `timeout_ms`: Total runtime limit for a new terminal (ms); reaching it terminates the process tree. Ignored when `terminal_id` is given. Default 600000.

# Call Shapes

- `{"command": "cargo build"}` — new terminal, run the command.
- `{"command": "cargo build", "yield_ms": 3000}` — return after at most 3s.
- `{"terminal_id": "term-1"}` — poll for incremental output.
- `{"terminal_id": "term-1", "command": "ls\n"}` — send input to the existing terminal.
- `{"terminal_id": "term-1", "kill": true}` — terminate the terminal.

# Results

- `running`: the process is still alive; `terminal_id` lets you continue.
- `completed`: the process exited successfully.
- `error`: the process exited unsuccessfully or the operation failed (for example, an unknown `terminal_id`).
- `cancelled`: the process was terminated.

Results include only output not already returned to the model. `exit_code` is present after process exit when the platform provides it.

# Notes

- Different terminals run concurrently. Calls targeting the same `terminal_id` are serialized.
- A directory change inside one command does not affect later new-terminal calls. Use `cwd` for a stable working directory.
- Every terminal accepts input (PTY). Commands that wait on stdin (like `cat` or `read`) hang until you send input or `kill` them.
- Keep `yield_ms` short enough to stay responsive. Continue a running process by polling its `terminal_id` rather than starting the same command again.
- Output is decoded as lossy UTF-8 at the model boundary. Large unread output is capped with its beginning and end preserved.
