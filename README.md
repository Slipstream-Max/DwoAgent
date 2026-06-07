# Dwo Agent (赤铎)

Native Rust implementation of a modular agent runtime.

A modular, multi-turn AI agent runtime with native tool execution, subagent delegation,
and streaming ACP transport.

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

登录 Weixin channel：

```bash
cargo run -- channel login weixin --agent-folder examples/dwo-agent
```

然后在 `channels.yaml` 里启用：

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

- Agent profile 文件夹结构、`agent.yaml` / `model.yaml`、`channels.yaml`、
  rules 和 skills 见
  [docs/agent-profile-structure-config.md](docs/agent-profile-structure-config.md)。
- `acp` 通过 stdio 运行 ACP，client 关闭 stdin 后进程退出。`serve`
  用于 websocket、Feishu、Weixin 等长生命周期 ingress channels。
- Weixin 使用 `channel_secret/weixin/auth.yaml` 和
  `channel_secret/weixin/context_tokens.json` 保存凭据和 token 状态，并把单个
  channel session 存在 `channel_sessions/weixin/session/` 下。这个根目录可通过
  `agent.yaml` 的 `channel_session_dir` 覆盖。它只接受完成扫码登录的用户发来的消息。
  `override_model` 和 `override_reasoning_mode` 只在 channel session 首次创建时使用；已有 session 保留持久化的模型设置。
  `media_input` 控制是否把入站非文本消息下载成附件。`media_output` 控制 channel session 首次创建时是否加入 Weixin 媒体回复工具；已有 session 保留持久化的 channel tool schemas。
- `agent.yaml` 里的 `max_running_turn` 是可选项。省略时，agent loop 会一直运行，直到模型停止、会话取消或发生错误。设置为正整数可以保留旧的 max-turn guard。
- `agent.yaml` 的 `tools` 可以把 `file_edit`、`terminal`、`subagent` 设置为
  `enable` 或 `disable`。这些值会在 session 创建时快照；之后修改
  `agent.yaml` 只影响新 session。
- ACP sessions 的 workspace 来自 `session/new.cwd`；解析后的路径会随 session 保存，并在加载时复用。
