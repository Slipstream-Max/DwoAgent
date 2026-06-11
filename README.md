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

Feishu channel 的 WebSocket 依赖需要 `protoc`。如果构建时报
`Could not find protoc`，请安装 protobuf compiler，或设置 `PROTOC` 指向
`protoc` 可执行文件。

## Run

```bash
cargo run -- acp --agent-folder examples/dwo-agent
```

启用 ACP WebSocket ingress：

```yaml
websocket:
  enabled: true
  bind_addr: 127.0.0.1:8765
  auth: true
```

然后运行：

```bash
cargo run -- channel login websocket --agent-folder examples/dwo-agent
```

客户端连接时使用输出的 token：

```http
Authorization: Bearer dwo_ws_xxx
```

启动服务：

```bash
cargo run -- serve --agent-folder examples/dwo-agent
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

保存 Feishu channel 凭据：

```bash
cargo run -- channel login feishu --agent-folder examples/dwo-agent --app-id cli_xxx --app-secret xxx
```

也可以使用环境变量：

```bash
FEISHU_APP_ID=cli_xxx FEISHU_APP_SECRET=xxx \
  cargo run -- channel login feishu --agent-folder examples/dwo-agent
```

然后在 `channels.yaml` 里启用：

```yaml
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

## Notes

- Agent profile 文件夹结构、`agent.yaml` / `model.yaml`、`channels.yaml`、
  `policy.yaml`、rules 和 skills 见
  [docs/agent-profile-structure-config.md](docs/agent-profile-structure-config.md)。
- `acp` 通过 stdio 运行 ACP，client 关闭 stdin 后进程退出。`serve`
  用于 websocket、Feishu、Weixin 等长生命周期 ingress channels。
- Weixin 使用 `channel_secret/weixin/auth.yaml` 和
  `channel_secret/weixin/context_tokens.json` 保存凭据和 token 状态，并把单个
  channel session 存在 `channel_sessions/weixin/session/` 下。这个根目录可通过
  `agent.yaml` 的 `channel_session_dir` 覆盖。它只接受完成扫码登录的用户发来的消息。
  `override_model` 和 `override_reasoning_mode` 只在 channel session 首次创建时使用；已有 session 保留持久化的模型设置。
  `media_input` 控制是否把入站非文本消息下载成附件。`media_output` 控制 channel session 首次创建时是否加入 Weixin 媒体回复工具；已有 session 保留持久化的 channel tool schemas。
- Feishu 使用 `channel_secret/feishu/auth.yaml` 保存 app 凭据。私聊和群聊使用独立 channel session：
  `channel_sessions/feishu/dm/<sender>/` 与 `channel_sessions/feishu/group/<chat_id>/`。
  `media_input` 默认关闭；开启后会把入站图片和文件下载到对应 session 的 `attachments/` 并加入上下文。
  `media_output` 默认关闭；开启后会向模型暴露 `feishu_reply_media(path)`，用于上传并回复图片或文件。
  `card_output` 默认关闭；开启后会向模型暴露 `feishu_reply_card(card)`，用于发送飞书交互卡片。
- `agent.yaml` 里的 `max_running_turn` 是可选项。省略时，agent loop 会一直运行，直到模型停止、会话取消或发生错误。设置为正整数可以保留旧的 max-turn guard。
- `agent.yaml` 的 `tools` 可以把 `file_edit`、`terminal`、`subagent` 设置为
  `enable` 或 `disable`。这些值会在 session 创建时快照；之后修改
  `agent.yaml` 只影响新 session。
- ACP sessions 的 workspace 来自 `session/new.cwd`；解析后的路径会随 session 保存，并在加载时复用。
