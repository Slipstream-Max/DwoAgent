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

Everything is configured through a single agent profile folder (`agent.yaml`,
`model.yaml`, `policy.yaml`, `channels.yaml`, `automation.yaml`).

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

| File | Purpose |
|---|---|
| `agent.yaml` | Agent identity, policy mode, session directories, tool toggles, external rules/skills |
| `model.yaml` | LLM provider and model settings |
| `policy.yaml` | Tool-use policy rules (confirm, watch, allow, deny) |
| `channels.yaml` | Ingress channel configuration (stdio, websocket, weixin, feishu) |
| `automation.yaml` | Scheduled job definitions |

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

Enable in `channels.yaml`:

```yaml
stdio:
  enabled: true
  auth: true
```

```bash
# First-time login (stores token in channel_secret/stdio/auth.yaml)
cargo run -- channel login stdio --agent-folder examples/dwo-agent

# Start the service
cargo run -- serve --agent-folder examples/dwo-agent

# Connect from Zed or any ACP stdio client
cargo run -- acp connect --agent-folder examples/dwo-agent
```

`acp connect` talks to the already-running service via a local IPC endpoint
(written by `serve` to `channel_secret/stdio/daemon.yaml`). Disconnecting the
bridge does **not** stop the agent runtime.

### WebSocket

```yaml
# channels.yaml
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
# channels.yaml
weixin:
  enabled: true
  workspace_dir: .
  markdown_filter: true
  media_input: true
  media_output: true
  override_model: deepseek-v4-flash
  override_reasoning_mode: auto
```

- Auth files live under `channel_secret/weixin/` (auth token and context state).
- Each channel gets a single session under `channel_sessions/weixin/session/`
  (root overridable via `agent.yaml` → `channel_session_dir`).
- `override_model` and `override_reasoning_mode` only apply on **first session
  creation**; an existing session keeps its persisted settings.
- `media_input`: when enabled, inbound non-text messages are downloaded as
  attachments.
- `media_output`: when enabled, the WeChat media reply tool is exposed to the
  model on session creation. Existing sessions preserve their tool schemas.

### Feishu (Lark)

```bash
cargo run -- channel login feishu --agent-folder examples/dwo-agent \
  --app-id cli_xxx --app-secret xxx

# Or via environment variables:
FEISHU_APP_ID=cli_xxx FEISHU_APP_SECRET=xxx \
  cargo run -- channel login feishu --agent-folder examples/dwo-agent
```

```yaml
# channels.yaml
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

- Auth stored in `channel_secret/feishu/auth.yaml`.
- Separate sessions for DMs and groups:
  `channel_sessions/feishu/dm/<sender>/` and `channel_sessions/feishu/group/<chat_id>/`.
- `media_input` (default off): downloads inbound images/files to
  `attachments/` under the session and includes them in context.
- `media_output` (default off): exposes `feishu_reply_media(path)` for
  uploading and replying with images/files.
- `card_output` (default off): exposes `feishu_reply_card(card)` for sending
  interactive Feishu cards.
- Tool confirmations are relayed via `/approve <id>` and `/deny <id>`. Audit
  records go to `channel_secret/audit/confirm_audit.jsonl`.

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
# automation.yaml
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
  <session_store_dir>/<year>/<month>/<day>/<session_id>/automation/<job_id>/runs/<run_id>/run.yaml
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
| Overview | Service status (detects `serve` via `channel_secret/stdio/daemon.yaml`) |
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
