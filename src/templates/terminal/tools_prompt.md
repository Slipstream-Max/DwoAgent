## Terminal Tools

### Rules

- Keep commands focused and outputs concise.
- Use the returned `run_id` to inspect or stop long-running commands.
- Use `list_terminals` when you need to recall running terminal ids.
- Use `terminal_wait` when you want to keep waiting for completion.
- Use `terminal_checkout` when you only want the current snapshot.

### Workflow

1. Start with `terminal_exec(command, env, timeout, lines, mode, startwith)`.
- Required:
  - `command`: command line to run.
- Optional:
  - `env`: object of environment variables.
  - `timeout`: seconds to wait before returning, default `30`.
  - `lines`: maximum output lines to return, default `200`.
  - `mode`: output slice mode while still running, one of `head`, `tail`, `startwith`; default `tail`.
  - `startwith`: 1-based line number used when `mode=startwith`, default `1`.
- It waits up to `timeout` seconds.
- If command finishes in time, it returns final output immediately.
- If still running, it returns `run_id` and a snapshot.

2. List running processes with `list_terminals()`.

3. Wait for a running process with `terminal_wait(run_id, timeout, lines, mode, startwith)`.
- Required:
  - `run_id`: ID returned by `terminal_exec`.
- Optional:
  - `timeout`: seconds to wait before returning, default `30`.
  - `lines`: maximum output lines to return, default `200`.
  - `mode`: output slice mode while still running, one of `head`, `tail`, `startwith`; default `tail`.
  - `startwith`: 1-based line number used when `mode=startwith`, default `1`.
- It waits up to `timeout` seconds.
- If command finishes during wait, it returns final output.
- If still running, it returns the latest snapshot.

4. Check a running process with `terminal_checkout(run_id, lines, mode, startwith)`.
- Required:
  - `run_id`: ID returned by `terminal_exec`.
- Optional:
  - `lines`: maximum output lines to return, default `200`.
  - `mode`: output slice mode while still running, one of `head`, `tail`, `startwith`; default `tail`.
  - `startwith`: 1-based line number used when `mode=startwith`, default `1`.
- This is a peek call.
- It returns the latest snapshot without waiting.
- If the command has finished, it returns final output.

5. Stop process with `terminal_kill(run_id, lines)`.
- Required:
  - `run_id`: ID returned by `terminal_exec`.
- Optional:
  - `lines`: maximum output lines to return, default `200`.
- Returns final tail output after kill.

### Output Slicing

- `head`: earliest lines.
- `tail`: latest lines.
- `startwith`: from line N for `lines` rows.
- For finished states (`completed_success`, `completed_error`, `killed`), output is always returned by `tail(lines)`.

### Lifecycle

- Running jobs stay in active queue.
- Finished or killed jobs stay queryable briefly, then expire.
