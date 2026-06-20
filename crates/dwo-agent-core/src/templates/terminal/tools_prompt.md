## Terminal Tools

### Rules

- Keep commands focused and outputs concise.
- Use the returned `name` to inspect or stop a terminal session.
- Use `list_terminals` when you need to recall terminal names.
- Use `terminal_checkout` when you only want the current output snapshot.
- Prefer `rg -n` for text search so each hit includes `path:line:text`.
- Use `rg --files` to find files by name before opening them.

### Search

- Exact text: `rg -n --fixed-strings "needle" path`
- Regex text: `rg -n "pattern" path`
- Case-sensitive search: `rg -n --case-sensitive "Name" path`
- Files by glob: `rg --files -g "*.rs"`
- Show nearby context when needed: `rg -n -C 2 "pattern" path`

### Workflow

1. Start with `terminal_exec(command, terminal_name?, env?, timeout?)`.
- Required:
  - `command`: command line to run.
- Optional:
  - `terminal_name`: human-readable name; if omitted, one is assigned, such as `powershell-1` or `sh-1`.
  - `env`: object of environment variables.
  - `timeout`: seconds to wait before returning, default `30`.
- If the command finishes in time, it returns full output immediately.
- If it is still running after timeout, it returns the terminal name and timeout status.

2. List terminal sessions with `list_terminals()`.

3. Check output with `terminal_checkout(terminal_name, lines?)`.
- Required:
  - `terminal_name`: name returned by `terminal_exec` or `list_terminals`.
- Optional:
  - `lines`: number of latest output lines to return, default `200`.

4. Stop a process with `terminal_kill(terminal_name, lines?)`.
- Required:
  - `terminal_name`: name returned by `terminal_exec` or `list_terminals`.
- Optional:
  - `lines`: number of latest output lines to return after killing, default `200`.

### Lifecycle

- Running and recently finished jobs stay queryable briefly, then expire.
