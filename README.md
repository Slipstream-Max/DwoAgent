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
cargo run -- acp embedded --agent-folder examples/dwo-agent
```

启用长生命周期本机 stdio bridge：

```yaml
stdio:
  enabled: true
  auth: true
```

然后运行：

```bash
cargo run -- channel login stdio --agent-folder examples/dwo-agent
cargo run -- serve --agent-folder examples/dwo-agent
```

Zed 或其他 stdio ACP client 连接长期 runtime：

```bash
cargo run -- acp connect --agent-folder examples/dwo-agent
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

在平级 `automation.yaml` 启用 automation/job：

```yaml
enabled: true
jobs:
  - id: daily_digest
    enabled: true
    workspace_dir: .
    session:
      mode: new
    schedule:
      type: interval
      every_seconds: 3600
    prompt: "总结当前项目状态。"
    notify:
      - channel: weixin
```

打开本地 TUI dashboard：

```bash
cargo run -- tui --agent-folder examples/dwo-agent
```

TUI 第一版直接读取 agent profile 文件和本地运行记录，不要求 `serve` 正在运行。主导航为
`Overview`、`Agent`、`Sessions`、`Channels`、`Automation`、`Logs`；Service 状态放在
`Overview`，通过 `channel_secret/stdio/daemon.yaml` 检测。

## Notes

- Agent profile 文件夹结构、`agent.yaml` / `model.yaml`、`policy.yaml`、
  `channels.yaml`、`automation.yaml`、rules 和 skills 见
  [docs/agent-profile-structure-config.md](docs/agent-profile-structure-config.md)。
- `acp embedded` 通过 stdio 在当前进程内运行 ACP，client 关闭 stdin 后进程退出。
  `acp connect` 是 stdio bridge，会连接已经启动的 `serve`，bridge 退出不会关闭 agent runtime。
  `serve` 用于 stdio、websocket、Feishu、Weixin 等长生命周期 ingress channels，也会启动 `automation.yaml` 配置的 scheduler。
- Stdio bridge 使用 `channel_secret/stdio/auth.yaml` 保存 token，`serve` 启动时写入
  `channel_secret/stdio/daemon.yaml` 供 `acp connect` 定位本机 IPC endpoint。
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
- Weixin/Feishu 支持 `/list`、`/switch <session_id>`、`/back`、`/where`。切换到非默认 session 时会占用该 session；其他 channel、ACP IPC 或 WebSocket 连接不能同时占用。
- 飞书工具确认会通过 `/approve <confirmation_id>`、`/deny <confirmation_id>` 透传，确认审计写入 `channel_secret/audit/confirm_audit.jsonl`。
- Automation 由 `serve` 激活，但不是 ingress channel。Job 可配置 `new`、`fixed` 或 `sticky` session 模式；目标 session 被占用时本次 run 会记为 `skipped`。
  Run 状态写入 `<session_store_dir>/<year>/<month>/<day>/<session_id>/automation/<job_id>/runs/<run_id>/run.yaml`。
  配置 `notify` 后，run 结束会投递包含 `session_id` 和 `/switch <session_id>` 的通知；投递结果也会写回 `run.yaml`。
- `agent.yaml` 里的 `max_running_turn` 是可选项。省略时，agent loop 会一直运行，直到模型停止、会话取消或发生错误。设置为正整数可以保留旧的 max-turn guard。
- `agent.yaml` 的 `tools` 可以把 `file_edit`、`terminal`、`subagent` 设置为
  `enable` 或 `disable`。这些值会在 session 创建时快照；之后修改
  `agent.yaml` 只影响新 session。
- ACP sessions 的 workspace 来自 `session/new.cwd`；解析后的路径会随 session 保存，并在加载时复用。
