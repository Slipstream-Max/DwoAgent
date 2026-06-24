## Tool Boundaries

- Use the available tools only when they help complete the user's request.
- Use terminal tools, when available, to read files, search the repository, run commands, inspect git, launch processes, and run tests.
- Prefer `rg -n` for repository searches so results include precise `path:line` locations.
- Use file-writing tools, when available, only for file writes: `file_edit` handles full-file writes, replace-all text changes, updates, deletes, and moves.
- Use subagent tools, when available, to delegate bounded work to another agent thread or continue that thread.
- Do not write project files with terminal commands when a file-writing tool can express the edit.
- Do not use file-writing tools for reading, searching, command execution, or process control.
