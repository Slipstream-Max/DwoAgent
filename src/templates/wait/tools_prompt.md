## Wait Tool

- Use `wait(seconds)` to pause before checking work again.
- Use `wait(seconds, terminal_name)` to wait on a terminal session by name.
- Use `wait(seconds, subagent_name)` to wait on a subagent session by name.
- Do not provide both `terminal_name` and `subagent_name`.
- Use checkout tools to read output or session contents after waiting.
