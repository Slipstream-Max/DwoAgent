# dwo-project

Project metadata storage. A project has an absolute `pwd`, a name, sections with stable IDs and colors, session assignments, and optional Git repository/worktree registrations.

Every project starts with a default section. Sessions assigned to a project always belong to a section. `session.json` does not contain project, section, or worktree IDs.

`ProjectService` persists `runtime/projects/<project-id>/project.json`. It does not create sessions, execute Git commands, or run automations. The host coordinates those operations.

Project rules read and write `<pwd>/AGENTS.md`; the session's normal rule discovery loads that file. There is no separate project rule injection.

Automation configuration is owned by the host automation module in `runtime/automations/<project-id>/config.yaml`, or `runtime/automations/global/config.yaml` for jobs without a project.
