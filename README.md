# Dwo Agent (赤铎)

Native Rust implementation of a modular, multi-turn AI agent runtime with
native tool execution, subagent delegation, and streaming ACP transport.

## What is Dwo Agent?

Dwo Agent runs a persistent AI agent that can:

- Execute tools natively — file editing, terminal commands, subagent delegation
- Serve across multiple channels simultaneously — stdio (ACP), WebSocket, WeChat, Feishu
- Schedule automated jobs with configurable session policies
- Enforce tool-use policies (confirm / full_access / watch)
- Present a local TUI dashboard for monitoring and session inspection

Everything is configured through one `agent.yaml` plus `resources/` prompt files.

## Quick Start

```bash
# Build
cargo build

# Run an embedded agent over stdio (closes when stdin ends)
cargo run -- acp embedded --agent-folder examples/dwo-agent
```

> **Prerequisite for Feishu:** The Feishu channel depends on a WebSocket library
> that requires `protoc`. If you see `Could not find protoc`, install the
> protobuf compiler or set the `PROTOC` environment variable.

## Configuration

Agent behaviour is driven by YAML files under the agent folder. See
[docs/agent-profile-structure-config.md](docs/agent-profile-structure-config.md)
for the full specification.

| Path | Purpose |
|---|---|
| `agent.yaml` | Agent identity, model, policy, channels, automation, tool toggles |
| `runtime/sessions/` | Runtime ordinary sessions: conversations, context, attachments, artifacts |
| `runtime/channel_state/` | Runtime channel routing state: bridge binding, cursors, context tokens |
| `runtime/channel_secret/` | Runtime local channel credentials, daemon manifests, confirmation audit |
| `resources/prompt/system.md` | Required system prompt |
| `resources/prompt/AGENTS.md` | Optional profile-level rule prompt |
| `resources/skills/` | Optional agent-profile skills exposed through `<available_skills>` |
| `resources/mcp.json` | Optional MCP config marker; adds an `<mcp>` context block with mcporter install/use guidance |

Tool toggles in `agent.yaml` (`file_edit`, `terminal`, `subagent`) accept
`enable` or `disable`. These values are **snapshotted at session creation**;
later changes to `agent.yaml` only affect new sessions.

`max_running_turn` is optional. When omitted the agent loop runs until the model
stops, the session is cancelled, or an error occurs. Set a positive integer to
cap turns.

## Channels

### Embedded ACP (stdio)

Runs an ACP session in-process. The process exits when the client closes stdin.

```bash
cargo run -- acp embedded --agent-folder examples/dwo-agent
```

### Stdio Bridge (long-lived)

Enable under `channels.stdio` in `agent.yaml`:

```yaml
channels:
  stdio:
    enabled: true
    auth: true
```

```bash
# First-time login (stores token in runtime/channel_secret/stdio/auth.yaml)
cargo run -- channel login stdio --agent-folder examples/dwo-agent

# Start the service
cargo run -- serve --agent-folder examples/dwo-agent

# Connect from Zed or any ACP stdio client
cargo run -- acp connect --agent-folder examples/dwo-agent
```

`acp connect` talks to the already-running service via a local IPC endpoint
(written by `serve` to `runtime/channel_secret/stdio/daemon.yaml`). Disconnecting the
bridge does **not** stop the agent runtime.

### WebSocket

```yaml
# agent.yaml
channels:
  websocket:
    enabled: true
    bind_addr: 127.0.0.1:8765
    auth: true
```

```bash
cargo run -- channel login websocket --agent-folder examples/dwo-agent
cargo run -- serve --agent-folder examples/dwo-agent
```

Connect with the printed token:

```http
Authorization: Bearer dwo_ws_xxx
```

### WeChat (Weixin)

```bash
cargo run -- channel login weixin --agent-folder examples/dwo-agent
```

```yaml
# agent.yaml
channels:
  weixin:
    enabled: true
    workspace_dir: .
    markdown_filter: true
    media_input: true
    media_output: true
    override_model: deepseek-v4-flash
    override_reasoning_mode: auto
```

- Auth files live under `runtime/channel_secret/weixin/`; runtime channel state
  such as sync cursors lives under `runtime/channel_state/weixin/`.
- Weixin messages route to ordinary sessions. The first message creates a
  normal session unless `default_session_id` is configured.
- `override_model` and `override_reasoning_mode` only apply when the channel
  creates a new default ordinary session.
- `media_input`: when enabled, inbound non-text messages are downloaded as
  attachments under the active ordinary session.
- `media_output`: when enabled, the WeChat media reply tool is exposed only for
  the current Weixin-triggered turn.

### Feishu (Lark)

```bash
cargo run -- channel login feishu --agent-folder examples/dwo-agent \
  --app-id cli_xxx --app-secret xxx

# Or via environment variables:
FEISHU_APP_ID=cli_xxx FEISHU_APP_SECRET=xxx \
  cargo run -- channel login feishu --agent-folder examples/dwo-agent
```

```yaml
# agent.yaml
channels:
  feishu:
    enabled: true
    workspace_dir: .
    domain: feishu
    dm_policy: allow_all
    group_policy: white_list
    allow_from: ["*"]
    group_allow_from: []
    group_require_mention: true
    media_input: true
    media_output: true
    card_output: true
    override_model: deepseek-v4-flash
    override_reasoning_mode: auto
```

- Auth stored in `runtime/channel_secret/feishu/auth.yaml`.
- DM/group routing state lives under `runtime/channel_state/feishu/...`; real
  conversations are ordinary sessions.
- `media_input` (default off): downloads inbound images/files to
  `attachments/` under the active ordinary session and includes them in context.
- `media_output` (default off): exposes `feishu_reply_media(path)` for
  uploading and replying with images/files on the current Feishu-triggered turn.
- `card_output` (default off): exposes `feishu_reply_card(card)` for sending
  interactive Feishu cards on the current Feishu-triggered turn.
- Tool confirmations are relayed via `/approve <id>` and `/deny <id>`. Audit
  records go to `runtime/channel_secret/audit/confirm_audit.jsonl`.

### Channel Commands

WeChat and Feishu channels support these built-in commands:

| Command | Description |
|---|---|
| `/list` | List available sessions |
| `/switch <session_id>` | Switch to and claim a session |
| `/back` | Return to the default session |
| `/where` | Show current session |

Switching to a non-default session claims it exclusively; no other channel,
ACP IPC, or WebSocket connection can occupy the same session simultaneously.

## Automation

Enable automation alongside or independently of channels:

```yaml
# agent.yaml
automation:
  enabled: true
  jobs:
    - id: daily_digest
      enabled: true
      workspace_dir: .
      session:
        mode: new          # new | fixed | sticky
      schedule:
        type: interval
        every_seconds: 3600
      prompt: "总结当前项目状态。"
      notify:
        - channel: weixin
```

- Automation is activated by `serve` but is **not** an ingress channel itself.
- **Session modes:**
  - `new` — create a fresh session each run.
  - `fixed` — reuse a named session.
  - `sticky` — reuse the same session across runs if available.
- If the target session is occupied, the run is recorded as `skipped`.
- Run state is persisted to:
  ```
  runtime/sessions/<year>/<month>/<day>/<session_id>/automation/<job_id>/runs/<run_id>/run.yaml
  ```
- When `notify` is configured, the run result (including `session_id` and a
  `/switch` command) is delivered to the specified channel(s). Delivery status
  is written back to `run.yaml`.

## TUI Dashboard

```bash
cargo run -- tui --agent-folder examples/dwo-agent
```

The TUI reads agent profile files and local run records directly — it does
**not** require `serve` to be running.

| Tab | Content |
|---|---|
| Overview | Service status (detects `serve` via `runtime/channel_secret/stdio/daemon.yaml`) |
| Agent | Agent profile summary |
| Sessions | Session list and details |
| Channels | Channel configuration and state |
| Automation | Job definitions and run history |
| Logs | Recent activity log |

## ACP Sessions

ACP sessions inherit their workspace from `session/new.cwd`. The resolved path
is persisted with the session and reused on subsequent loads.

## License

<!-- TODO -->

## Project Structure

```
src/
├── agent/         Agent lifecycle, sessions, turns, subagents, policy
├── automation/    Scheduled job runtime
├── config/        YAML config loading and models
├── context/       Context window management and compaction
├── ingress/       Channel runtimes: acp, stdio, websocket, weixin, feishu
├── llm/           LLM client and provider abstraction
├── templates/     Prompt templates and tool descriptions
├── tools/         Native tool runtimes (file_edit, terminal, subagent)
├── tui/           Terminal dashboard
├── utils/         Shared utilities
├── watchers/      Environment block watchers
├── cli.rs         CLI definition (clap)
├── host.rs        Host process wiring
├── lib.rs         Library root
└── main.rs        Binary entry point
tests/
examples/
├── dwo-agent/     Default agent profile
└── weixin-agent/  WeChat single-user assistant example
```
