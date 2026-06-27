# Dwo Agent (赤铎)

Native Rust implementation of a modular, multi-turn AI agent runtime.

## Architecture

Dwo Agent is split into three layers:

- `cli`: human/script entrypoint for creating config, running one profile directly, or controlling the supervisor.
- `supervisor`: machine-level daemon for desktop/UI WebSocket access, profile registry, and worker-pool routing.
- `agent core`: single-profile runtime where stdio RPC, Weixin, Feishu, and automation all forward into one `AgentService`.

The profile boundary is the `agent.yaml` directory. The supervisor is machine-scoped and is not tied to one profile.

## Quick Start

```bash
# Build the dwoagent binary
cargo build

# Create an agent profile interactively
cargo run -- create agent --name coder

# Run one profile host directly
cargo run -- agent run --agent-profile ~/.dwoagent/profiles/coder

# ACP stdio shim for ACP-compatible clients
cargo run -- supervisor acp --agent-profile ~/.dwoagent/profiles/coder
```

> **Prerequisite for Feishu:** The Feishu channel depends on a WebSocket library
> that requires `protoc`. If you see `Could not find protoc`, install the
> protobuf compiler or set the `PROTOC` environment variable.

## Doctor

`doctor` checks or prepares local environment prerequisites. Running it without
a mode is the same as `doctor --check`.

```bash
# Check local environment dependencies
cargo run -- doctor --check

# Default: same as --check
cargo run -- doctor

# Install missing environment dependencies interactively
cargo run -- doctor --resolve

# Install missing environment dependencies without prompts
cargo run -- doctor --resolve --yes
```

`doctor --resolve` installs missing `mcporter` and `rg` through npm. If either
tool is missing, Node.js/npm must already be installed. `doctor` does not create
agent profiles and does not register supervisor startup.

## Supervisor

The supervisor is the only OS-level daemon. It is responsible for the desktop/UI
WebSocket endpoint, profile registry, and lazy profile host pool.

```bash
# Create default supervisor config at ~/.dwoagent/supervisor.yaml
cargo run -- create supervisor --default

# Register startup at user login
cargo run -- supervisor enable

# Start the supervisor now
cargo run -- supervisor start

# Show startup registration and process status
cargo run -- supervisor status

# Stop running supervisor process(es)
cargo run -- supervisor stop

# Unregister startup at user login
cargo run -- supervisor disable
```

The foreground runtime is internal and used by startup registration:

```bash
cargo run -- supervisor run
```

Supervisor configuration:

```yaml
version: 1
endpoint:
  websocketBindAddr: 127.0.0.1:8766
  secret: dwo_sup_xxx
profiles:
  - id: coder
    path: C:\Users\you\.dwoagent\profiles\coder
defaultProfile: coder
pool:
  maxWorkers: 3
  idleSeconds: 600
```

Supervisor WebSocket speaks JSON messages. Each request must include the
configured `secret` unless the secret is empty:

```json
{"id":1,"type":"profiles.list","secret":"dwo_sup_xxx"}
{"id":2,"type":"worker.request","secret":"dwo_sup_xxx","profile":"coder","method":"_dwo/worker/profile","params":{}}
{"id":3,"type":"worker.request","secret":"dwo_sup_xxx","profile":"coder","method":"session/new","params":{"cwd":".","mcpServers":[]}}
{"id":4,"type":"worker.request","secret":"dwo_sup_xxx","profile":"coder","method":"session/list","params":{}}
{"id":5,"type":"workers.status","secret":"dwo_sup_xxx"}
{"id":6,"type":"worker.stop","secret":"dwo_sup_xxx","profile":"coder"}
```

`worker.request` lazily starts `dwoagent agent run --agent-profile <path>` for the
requested profile and reuses it until LRU/idle pool cleanup removes it.
`worker.stop` stops one profile host; `workers.shutdown` stops every profile host.
`worker.request` forwards JSON-RPC methods to the profile host. Standard ACP
methods include `initialize`, `session/new`, `session/list`, `session/load`, and
`session/prompt`; Dwo-only extension methods currently include
`_dwo/worker/profile` and `_dwo/session/context`.

Long-running worker calls can emit messages before the final
`supervisor.result`. `session/prompt` streams ACP notifications as:

```json
{"id":7,"type":"supervisor.event","profile":"coder","event":{"method":"session/update","params":{"sessionId":"<session-id>","update":{}}}}
{"id":7,"type":"supervisor.result","result":{"profile":"coder","result":{"stopReason":"end_turn"}}}
```

## Agent Core Modes

```bash
# Profile host mode: stdio RPC plus enabled Weixin/Feishu/automation
cargo run -- agent run --agent-profile <path>

# ACP compatibility mode: shim through supervisor
cargo run -- supervisor acp --agent-profile <path>
```

The supervisor also uses `agent run` internally when it lazily starts a profile
host.

## Configuration

Product docs:

- `docs/commands.md`: CLI command reference.
- `docs/agent-profile-structure-config.md`: agent profile structure and `agent.yaml` fields.
- `docs/agent.full.yaml`: full `agent.yaml` template.
- `docs/supervisor-config.md`: machine-level supervisor config.
- `docs/supervisor.full.yaml`: full `supervisor.yaml` template.

Agent behaviour is driven by YAML files under the agent profile folder.

| Path | Purpose |
|---|---|
| `agent.yaml` | Agent identity, model, policy, external channels, automation, tool toggles |
| `runtime/sessions/` | Runtime ordinary sessions: conversations, context, attachments, artifacts |
| `runtime/channel_state/` | Runtime external channel routing state |
| `runtime/channel_secret/` | Runtime external channel credentials and confirmation audit |
| `resources/prompt/system.md` | Required system prompt |
| `resources/prompt/AGENTS.md` | Optional profile-level rule prompt |
| `resources/skills/` | Optional agent-profile skills exposed through `<available_skills>` |
| `resources/mcp.json` | Optional MCP config marker; adds an `<mcp>` context block with mcporter guidance |

Supervisor configuration defaults to `~/.dwoagent/supervisor.yaml` and contains
machine-level settings: WebSocket endpoint, secret, profile registry, default
profile, and worker pool policy.

## Channels

Only external channels live in `agent.yaml`: Weixin and Feishu. ACP stdio and
the supervisor WebSocket are runtime transports that forward into the profile
host, not profile-configured channels.

### WeChat (Weixin)

```bash
cargo run -- channel login weixin --agent-profile ~/.dwoagent/profiles/coder
```

```yaml
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

### Feishu (Lark)

```bash
cargo run -- channel login feishu --agent-profile ~/.dwoagent/profiles/coder \
  --app-id cli_xxx --app-secret xxx
```

```yaml
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

## Project Structure

```text
src/
└── main.rs                  Thin binary entry point
crates/
├── dwo-agent-cli/           Clap commands, doctor, create helpers
├── dwo-agent-core/          Single-profile agent runtime
│   ├── agent/               Agent lifecycle, sessions, turns, subagents, policy
│   ├── automation/          Scheduled job runtime
│   ├── config/              YAML config loading and core models
│   ├── context/             Context window management and compaction
│   ├── ingress/             stdio RPC, Weixin, Feishu transports
│   ├── tools/               Native tools: file_edit, terminal, subagent
│   ├── host.rs              Profile host wiring
│   └── worker.rs            Internal supervisor profile-host entry
├── dwo-agent-supervisor/    Machine-level daemon, WS endpoint, worker pool
└── dwo-llm/                 LLM client, provider catalog, model config types
```
