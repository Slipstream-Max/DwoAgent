## Tool Boundaries

- Use the available tools only when they help complete the user's request.
- Use terminal tools, when available, to read files, search the repository, run commands, inspect git, launch processes, and run tests.
- Use `file_edit`, when available, only for file writes: add, update, delete, or move files.
- Use subagent tools, when available, to delegate bounded work to another agent thread or continue that thread.
- Use CodeMode, when available, for short sandboxed computation and MCP tool/prompt/resource calls from code.
- Do not write project files with terminal commands or CodeMode when `file_edit` can express the edit.
- Do not use `file_edit` for reading, searching, command execution, or process control.
