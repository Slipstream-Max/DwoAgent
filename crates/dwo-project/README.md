# dwo-project

Filesystem-backed Project and Board domain for DwoAgent.

The crate owns Project metadata, generated Project workspaces, Sections,
Topics, Labels, Topic Markdown, and opaque Session/Task ID associations. It
does not depend on AgentService, Automation, Host, or transport types.

```text
ProjectService
|- Project { id, name, pwd, board }
|- Section CRUD and ordering
|- Topic CRUD, movement, Markdown, and ordering
|- Label CRUD and assignment
`- Topic-owned Session and Task references
```

Cross-domain validation and detail composition remain in `dwo-host`.
