# dwo-project

Filesystem-backed Project and Board domain for DwoAgent.

The crate owns Project metadata, Sections, Topics, Labels, Topic Markdown, and
opaque Session/Task ID associations. A shared Project has one required `pwd`
and optional repository/worktrees; an independent Project has none of those.
Session workspace bindings belong to SessionService, not ProjectService. The
Project directory contains metadata rather than working files. This crate does
not depend on SessionService, Automation, Host, or transport types.

```text
ProjectService
|- Project { id, name, kind, optional pwd/repository/worktrees, board }
|- Section CRUD and ordering
|- Topic CRUD, movement, Markdown, and ordering
|- Label CRUD and assignment
`- Topic-owned Session and Task references
```

Cross-domain validation and detail composition remain in `dwo-host`.
