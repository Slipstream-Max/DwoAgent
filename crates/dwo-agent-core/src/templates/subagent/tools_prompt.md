## Subagent Tools

### Rules

- Do not use subagents for trivial work that you can finish directly.
- Give each subagent a concrete, bounded task.
- Subagents have their own context and tool manager, but their progress remains visible to the user through ACP updates.
- A completed subagent remains available for follow-up messages until it is closed.

### Tools

- `spawn_subagent(task, subagent_name?, policy?)`: start one subagent thread and return its name immediately.
- `list_subagents()`: list known subagent names and statuses.
- `checkout_subagent(subagent_name, message_num?)`: read the latest structured session slice or final result.
- `send_subagent(subagent_name, message, interrupt?)`: send a follow-up message; returns only ok or error.
- `close_subagent(subagent_name)`: close the subagent thread and cancel any running turn.

### Workflow

1. Call `spawn_subagent` for each independent worker.
2. Continue useful main-thread work while subagents run.
3. Use `list_subagents` when you need to recall subagent names.
4. Use `checkout_subagent` to inspect progress or retrieve the final result.
5. Use `send_subagent` only after you want to continue an existing subagent.
6. Use `close_subagent` when the thread is no longer needed.
