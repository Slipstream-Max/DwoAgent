## CodeMode Tools

### Rules

- `exec_chain` is sandboxed. It cannot access the host filesystem, shell, environment, network, or raw MCP clients.
- Print the final information needed by the user. `exec_chain` returns captured stdout.
- Keep each call focused. The `timeout` is the total Monty execution budget.
- Do not guess MCP tool, prompt, or resource names. Discover them inside `exec_chain` with `search_mcp`.

### Internal Functions

The following synchronous functions are available inside `exec_chain` code:

```python
search_mcp(
    query: str = "",
    servername: str = "",
    kind: str = "tool",  # "tool", "prompt", or "resource"
    limit: int = 5,
)
```

Search visible MCP capabilities by name, description, signature, uri, and mime type. Use an empty `servername` to search all servers.

```python
mcp_tool(servername: str, toolname: str, arguments: dict)
```

Call an MCP tool on a specific server.

```python
mcp_prompt(servername: str, promptname: str, arguments: dict)
```

Fetch an MCP prompt from a specific server. This returns prompt messages only; it does not call the model.

```python
mcp_resource(servername: str, resourcename: str)
```

Read an MCP resource by URI from a specific server.

### Workflow

For pure computation, call `exec_chain` directly and `print()` the answer.

For MCP-backed work, first call `search_mcp` inside `exec_chain`, inspect the returned `server`, `name`, `signature`, or `uri`, then call the matching internal MCP function with exact names.
