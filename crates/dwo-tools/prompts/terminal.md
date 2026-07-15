---
name: terminal
description: Run commands, interact with terminal processes, poll their output, and stop them.
---

# Use Cases

Use `terminal` to inspect the workspace, run builds and tests, execute programs, start long-running processes, or interact with a process that requires stdin.

# Atomic Operations

## `run`

Start a new command.

- `command` is required and is executed through PowerShell on Windows or `sh` on Unix.
- `cwd` optionally selects the working directory for this command. Relative paths are resolved from the session workspace.
- `tty: true` starts an interactive PTY and keeps stdin available.
- `tty: false` closes stdin. Use it for non-interactive commands.
- `yield_ms` controls how long the call waits for the command to exit before returning `running`. It does not terminate the command.
- `timeout_ms` optionally sets a total runtime limit. Reaching it terminates the process tree.

If the command is still alive when `yield_ms` elapses, the result contains a `terminal_id` for later operations.

## `input`

Operate an existing terminal.

- `terminal_id` identifies the terminal returned by `run`.
- Non-empty `data` is written to PTY stdin, then the call waits for output, process exit, or `yield_ms`.
- Empty `data` writes nothing and waits for unread output, process exit, or `yield_ms`.
- Input is only available when the terminal was started with `tty: true`.

## `kill`

Terminate an existing terminal by `terminal_id`. The process tree is terminated and trailing output is drained before the result is returned.

# Results

- `running`: the process is still alive.
- `completed`: the process exited successfully.
- `error`: the process exited unsuccessfully or the terminal operation failed.
- `cancelled`: the process was terminated.

Results include only output not already returned to the model. `exit_code` is present after process exit when the platform provides it.

# Notes

- Different terminals can run concurrently. Calls targeting the same `terminal_id` are serialized.
- A directory change inside one command does not affect later `run` calls. Use `cwd` for a stable working directory.
- Prefer non-TTY execution unless the process needs input or terminal behavior.
- Keep `yield_ms` short enough to remain responsive. Continue a running process with `input` rather than starting the same command again.
- Output is decoded as lossy UTF-8 at the model boundary. Large unread output is capped with its beginning and end preserved.
