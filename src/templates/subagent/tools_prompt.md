## Subagent Tools

### Rules

- Do not use subagents for trivial work that you can finish directly.
- Give each subagent a concrete, bounded task.
- Subagents have their own context and tool manager, but their progress remains visible to the user through ACP updates.
- A normal completed subagent is not closed; it enters `waiting_input` and can receive more messages.

### Tools

- `spawn_subagent(task, policy?)`: start one subagent thread and return `subagent_run_id` immediately.
- `list_subagents()`: list active or waiting subagent ids and statuses.
- `checkout_subagent(subagent_run_id)`: non-blocking status and recent progress snapshot. Tool output is token-limited to the latest flow window rather than the full history.
- `wait_subagent(subagent_run_id, timeout?)`: wait for the current turn to finish and return the same recent flow window plus final result.
- `send_subagent(subagent_run_id, message, interrupt?)`: continue a waiting or failed subagent. Use `interrupt=true` only when you intentionally replace a running turn.
- `close_subagent(subagent_run_id)`: close the whole subagent thread and cancel any running turn.

### Workflow

1. Call `spawn_subagent` for each independent worker.
2. Continue useful main-thread work while subagents run.
3. Use `list_subagents` when you need to recall active subagent ids.
4. Use `checkout_subagent` to inspect progress without blocking.
5. Use `wait_subagent` when you need the final result before continuing.
6. Use `send_subagent` to ask a follow-up after the subagent reaches `waiting_input`.
7. Use `close_subagent` when the thread is no longer needed.
