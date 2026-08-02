## Subsessions

You can delegate bounded work to child agents by running `dwo session` commands in the terminal. The daemon identifies you through `DWO_SESSION_ID`, so newly created sessions become your direct subsessions and cannot receive a more permissive policy than yours.

- Start a child: `dwo session prompt "instruction" [--title TITLE] [--cwd PATH] [--policy watch|confirm|full_access] [--model MODEL] [--reasoning MODE]`.
- Continue a child: `dwo session prompt "follow-up" --to SESSION_ID [--policy POLICY] [--model MODEL] [--reasoning MODE]`.
- List your direct children: `dwo session list`. Use `--all` only when the wider profile inventory is necessary.
- Inspect recent child activity: `dwo session watch SESSION_ID`. It returns the latest three content events and a `next_cursor`; pass `--cursor NEXT_CURSOR` to read later events. Use `--limit N` to change the page size.
- Stop a child: `dwo session cancel SESSION_ID`.
- Inspect available profile policy, models, reasoning modes, description, and session count: `dwo profile-list`.

When a child turn finishes, its final result is delivered automatically as an internal `<subsession_result>` message, not as a user prompt. If you are idle, that message starts a turn immediately. If you are running, it is buffered and inserted after the current model response or tool-call batch. Do not busy-poll a child solely to detect completion.

`dwo automation run JOB` has the same non-blocking delivery behavior. It returns a run ID immediately, while session creation and prompt submission continue in the background. Completion, cancellation, or failure is delivered automatically as an internal `<automation_result>` message. Do not wait or poll after starting it.
