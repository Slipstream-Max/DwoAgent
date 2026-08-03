## Subsessions

Use a child session when a bounded task can run independently or needs a separate context. Do not create one for a trivial step that is faster to do directly. Run `dwo session` commands in the terminal. The daemon identifies you through `DWO_SESSION_ID`, so newly created sessions become your direct children and cannot receive a more permissive policy than yours.

- Start a child: `dwo session prompt "instruction" [--title TITLE] [--cwd PATH] [--policy watch|confirm|full_access] [--model MODEL] [--reasoning MODE]`.
- Continue a child: `dwo session prompt "follow-up" --to SESSION_ID [--policy POLICY] [--model MODEL] [--reasoning MODE]`.
- List your direct children: `dwo session list`. Use `--all` only when the wider profile inventory is necessary.
- Check whether a child is running: `dwo session status SESSION_ID`. Use its phase and active turn; `idle` means no turn is currently running. The last answer is only a 100-character preview.
- Inspect recent child content: `dwo session watch SESSION_ID`. It returns the latest three content events and a `next_cursor`; pass `--cursor NEXT_CURSOR` to read later events. Use `--limit N` to change the page size. `watch` reads content and is not the source of current running state.
- Cancel the active turn: `dwo session cancel SESSION_ID`.
- Delete an unneeded child and its transcript: `dwo session delete SESSION_ID`. Only delete when the session and its history are no longer needed.
- Inspect available profile policy, models, reasoning modes, description, and session count: `dwo profile-list`.

When a child turn finishes, its final result is delivered automatically as an internal `<subsession_result>` message, not as a user prompt. If you are idle, that message starts a turn immediately. If you are running, it is buffered and inserted after the current model response or tool-call batch. Do not busy-poll a child solely to detect completion.
