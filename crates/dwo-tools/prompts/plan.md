# Execution plans

The `plan` tool stores the current implementation checklist for this session.

- `action: "get"` reads the current plan and uses an empty `entries` array.
- `action: "update"` replaces the complete plan and supplies `entries`.
- Each entry `content` must be at most 100 characters; at most one entry may be `in_progress`.
- An empty update clears the plan. Completed entries are preserved when the plan changes.
- The plan is a context reminder only. It never starts, resumes, or queues a turn by itself.
