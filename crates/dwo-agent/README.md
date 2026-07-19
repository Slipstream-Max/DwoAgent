# dwo

`dwo` is the long-running dwoagent host and its control CLI. One daemon owns
the profile, sessions, channel state, model clients, and tool runtimes. CLI and
ACP processes connect to that daemon over local IPC instead of creating their
own `AgentService`.

```text
src/
  main.rs              process entry point
  cli/mod.rs           command parsing and client-side command handlers
  host/mod.rs          daemon composition root and RPC dispatch
  local/
    ipc.rs             local IPC server, request client, and event subscription
    acp.rs             ACP stdio adapter backed by local IPC
  channels/
    manager.rs         channel login, configuration, state, and secrets
    gateway.rs         long-running channel runtimes and session event routing
```

Only `dwo serve` constructs the `Host` and its `AgentService`. CLI and ACP
commands are local clients; channel runtimes live inside the daemon and call
the shared service directly.

```text
dwo install [--start]
dwo uninstall [--purge]
dwo serve --config-path <profile.yaml>
dwo daemon start|stop|status

dwo session list
dwo session new [name] [--cwd <path>]
dwo session delete <id>
dwo session prompt <id> <message>
dwo session cancel <id>
dwo session watch <id>
dwo session model <id> <model>
dwo session reasoning <id> [reasoning]
dwo session approve|deny <id> <permission-id>

dwo channel list
dwo channel weixin status
dwo channel weixin bind
dwo channel weixin unbind
dwo channel weixin send-message <message>
dwo channel weixin send-file <path>
dwo mcp search <query>
dwo mcp call <server.tool> --args '<json>'
dwo mcp auth <server>
dwo mcp auth <server> --logout
dwo automation list [--json]
dwo automation status [--json]
dwo automation run <job> [--json]
dwo acp
```

Windows uses a named pipe and an on-login scheduled task whose generated VBS
launcher keeps the daemon window hidden. macOS uses a Unix domain socket and a
per-user launchd agent. `serve` itself stays in the foreground; the
operating-system service manager owns background lifecycle.

The default profile is `~/.dwoagent`:

```text
profile.yaml
resource/prompts/System.md
resource/prompts/AGENTS.md
resource/skills/
resource/mcp.json
runtime/sessions/YYYY/MM/DD/<session-id>.json
runtime/workspaces/<session-id>/
runtime/attachments/weixin/YYYY/MM/DD/<session-id>/
runtime/mcp/catalog.json
runtime/mcp/oauth/
runtime/logs/
channels/weixin/runtime.yaml
channels/weixin/secret.yaml
```

Weixin user settings live in `profile.yaml` and are validated before the host
starts:

```yaml
channels:
  weixin:
    enabled: true
    replayTurns: 5
    markdownFilter: true
    mediaInput: true
```

`runtime.yaml` stores the selected session, `syncBuf`, and SDK context tokens.
`secret.yaml` stores the QR-login credentials. Both files are daemon-owned;
there is no per-channel generated configuration file.

`replayTurns` is limited to 10. After `/use`, each replayed turn combines the
user prompt and every non-empty assistant response into one message; tool
results are omitted. `/status` reports only current session state.

`status` reports whether the channel is configured and bound, the bound user
ID, and the selected session. `connected` means
that persisted credentials exist and validate; it is not a live network
health check. `send-message` and `send-file` always target the bound user and use
that user's current context token.

Inbound Weixin images and files are downloaded under the selected session's
dated `runtime/attachments/weixin/` directory and submitted as a structured
resource link containing the local path, MIME type, name, and size. A
media-only message is a valid prompt. Sessions created without an explicit
cwd use `runtime/workspaces/<session-id>` instead of the daemon process cwd.

The Weixin slash commands include `/new [name] [--cwd <path>]` and `/policy
[full_access|confirm|watch]`. Assistant responses are buffered for the whole
turn, joined in commit order, and split only when the combined text exceeds
4,000 characters. Tool calls are sent immediately only when confirmation is
required, together with the permission request ID.

When Weixin is enabled and bound, the context builder adds a concise channel
capability block to the system prompt. Binding and unbinding changes are
reported to existing sessions by the environment watcher. Credentials remain
owned by the daemon and are never included in model context.

MCP servers are configured in `resource/mcp.json`. Static HTTP headers and
stdio environment variables are resolved from that file, including `${ENV}`
references. Only servers declaring `auth.type: oauth` use the interactive
`dwo mcp auth` flow. The daemon initializes configured servers concurrently at
startup and stores the resulting catalog under `runtime/mcp/`; each successful
connection stays managed by the daemon. New or changed servers are initialized
the same way by the config watcher. Failed or unauthenticated servers remain in
the catalog with their status and error. MCP schemas are never registered as
model tools.

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "."]
    },
    "github": {
      "type": "streamableHttp",
      "url": "https://example.test/mcp",
      "headers": {"Authorization": "Bearer ${GITHUB_TOKEN}"}
    },
    "notion": {
      "type": "streamableHttp",
      "url": "https://example.test/mcp",
      "auth": {"type": "oauth"}
    }
  }
}
```

Weixin binding uses the real `weixin-agent` QR flow. Bound channels reconnect
inside the daemon, persist sync state, route slash commands through the shared
`AgentService`, and support answer-only or full tool-progress streaming.

The ACP command is a stdio bridge to the daemon IPC endpoint. It shares the
same sessions and events as CLI and Weixin clients. Loading a session keeps a
live observer attached, so idle ACP clients continue to receive prompts, tool
events, and permission requests from other clients.

External prompts use interrupt semantics: an active turn is cancelled, the
host waits for its terminal event, and then starts the replacement turn. The
origin endpoint does not receive its own prompt notification; every other
observer does.

## Automation

The daemon watches the `automation` section in `profile.yaml`. Jobs use a
standard five-field cron expression and either create a fresh session for every
run or target a fixed session. A fixed-session run intentionally uses the same
prompt semantics as an interactive interruption: an active turn is cancelled,
then the automation prompt starts.

Automation is unattended. Tool confirmation requests are denied automatically
instead of waiting forever. Automation does not create a separate state or
history directory; execution is persisted only through the target session.

```yaml
automation:
  enabled: true
  jobs:
    - name: daily-report
      schedule:
        cron: "0 9 * * *"
        timezone: Asia/Shanghai
      session:
        mode: new
        cwd: .
      prompt: Summarize the current project status.
```
