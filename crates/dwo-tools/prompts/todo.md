# Todo list

The `todo` tool stores the current implementation checklist for this session.

- `action: "get"` reads the current todo list and uses an empty `entries` array.
- `action: "update"` replaces the complete todo list and supplies `entries`.
- Each entry `content` must be at most 100 characters; at most one entry may be `in_progress`.
- An empty update clears the todo list. Completed entries are preserved when the todo list changes.
- The todo list is a context reminder only. It never starts, resumes, or queues a turn by itself.
