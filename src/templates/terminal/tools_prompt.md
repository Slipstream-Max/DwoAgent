## Terminal Tools

### Rules

- Keep commands focused and outputs concise.
- Use the returned `id` to inspect or stop long-running commands.
- Use `list_terminals` when you need to recall running terminal ids.
- Use `terminal_wait` when you want to wait before checking terminal state again.
- Use `terminal_checkout` when you only want the current snapshot.

### Workflow

1. Start with `terminal_exec(command, env, timeout)`.
- Required:
  - `command`: command line to run.
- Optional:
  - `env`: object of environment variables.
  - `timeout`: seconds to wait before returning, default `30`.
- It waits up to `timeout` seconds.
- If command finishes in time, it returns output immediately.
- If still running, it returns `id` and a snapshot.

2. List running processes with `list_terminals()`.

3. Wait before checking again with `terminal_wait(time)`.
- Required:
  - `time`: seconds to wait.
- It returns after the requested duration.

4. Check a running process with `terminal_checkout(id, tail_line_num)`.
- Required:
  - `id`: ID returned by `terminal_exec`.
- Optional:
  - `tail_line_num`: number of latest output lines to return, default `200`.
- This is a peek call.
- It returns the latest snapshot without waiting.
- If the command has finished, it returns output.

5. Stop process with `terminal_kill(id)`.
- Required:
  - `id`: ID returned by `terminal_exec`.
- Returns final output after kill.

### Lifecycle

- Running jobs stay in active queue.
- Finished or killed jobs stay queryable briefly, then expire.
