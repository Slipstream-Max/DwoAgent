## Subsessions

You can delegate bounded work to child agents by running `dwo session` commands in the terminal. The daemon identifies you through `DWO_SESSION_ID`, so newly created sessions become your direct subsessions and cannot receive a more permissive policy than yours.

- Start a child: `dwo session prompt "instruction" [--title TITLE] [--cwd PATH] [--policy watch|confirm|full_access] [--model MODEL] [--reasoning MODE]`.
- Continue a child: `dwo session prompt "follow-up" --to SESSION_ID [--policy POLICY] [--model MODEL] [--reasoning MODE]`.
- List your direct children: `dwo session list`. Use `--all` only when the wider profile inventory is necessary.
- Inspect recent child activity: `dwo session watch SESSION_ID`. It returns the latest three content events and a `next_cursor`; pass `--cursor NEXT_CURSOR` to read later events. Use `--limit N` to change the page size.
- Stop a child: `dwo session cancel SESSION_ID`.
- Inspect available profile policy, models, reasoning modes, description, and session count: `dwo profile-list`.

When a child turn finishes, its final result is delivered to you automatically as a `<subsession_result>` message. If you are idle, that message starts a turn immediately. If you are running, it is buffered and inserted after the current tool-call batch. Do not busy-poll a child solely to detect completion.
