# Dwo Agent (赤铎)

Native Rust implementation of a modular agent runtime.

A modular, multi-turn AI agent runtime with native tool execution, subagent delegation,
MCP integration, and streaming ACP transport.

## Status

Port in progress. The Rust crate is now laid out as a standalone project.

## Build

```bash
cargo build
```

## Run

```bash
cargo run -- acp --agent-folder examples/dwo-agent
```

Log in to the Weixin channel:

```bash
cargo run -- channel login weixin --agent-folder examples/dwo-agent
```

Then enable it in `channels.yaml`:

```yaml
weixin:
  enabled: true
  workspace_dir: .
  markdown_filter: true
  media_input: true
  media_output: true
  override_model: deepseek-v4-flash
  override_reasoning_mode: auto
```

## Notes

- `acp` runs ACP over stdio and exits when the client closes stdin. `serve`
  is reserved for long-lived ingress channels such as websocket, Feishu, and
  Weixin.
- Weixin uses `channel_secret/weixin/auth.yaml` and
  `channel_secret/weixin/context_tokens.json` for credentials/token state, and
  stores its single channel session under `channel_session/weixin/session/`.
  It only accepts messages from the user who completed QR login.
  `override_model` and `override_reasoning_mode` are used only when that
  channel session is first created; existing sessions keep their persisted
  model settings.
  `media_input` controls whether non-text inbound messages are downloaded as
  attachments. `media_output` controls whether the Weixin media reply tool is
  added when the channel session is first created; existing sessions keep their
  persisted channel tool schemas.
- `max_running_turn` in `agent.yaml` is optional. If it is omitted, the agent
  loop keeps advancing until the model stops, the session is cancelled, or an
  error occurs. Set it to a positive integer to keep the old max-turn guard.
- `tools` in `agent.yaml` can set `mcp`, `file_edit`, `terminal`, and
  `subagent` to `enable` or `disable`. These values are snapshotted when a
  session is created; changing `agent.yaml` later only affects new sessions.
- ACP sessions take their workspace from `session/new.cwd`; the resolved path
  is saved with the session and is reused on load.
- `tools/codemode/monty_backend.rs` uses the `monty` crate (Rust port of
  `pydantic-monty`) as a git dependency. Requires Rust 1.95+.
