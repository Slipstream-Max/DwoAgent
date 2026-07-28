# Channel 部署与使用

赤铎可以把同一个 daemon 接到微信、Telegram 和飞书/Lark 私聊。它们共享全局 session、模型、工具、MCP 和持久化数据，并分别保存当前选择的 session。

所有 channel 都只接受已绑定用户的私聊。修改 `profile.yaml` 后重启 daemon，使 adapter 按新配置启动：

```text
dwo daemon stop
dwo daemon start
dwo channel list
```

## 通用配置

`~/.dwoagent/profile.yaml`：

```yaml
channels:
  weixin:
    enabled: true
    replayTurns: 5
    markdownFilter: true
  telegram:
    enabled: false
    replayTurns: 5
    botTokenEnv: TELEGRAM_BOT_TOKEN
    tgProxy: null
    mediaInput: true
  feishu:
    enabled: false
    replayTurns: 5
    appIdEnv: FEISHU_APP_ID
    appSecretEnv: FEISHU_APP_SECRET
    platform: feishu
    mediaInput: true
```

`replayTurns` 最大为 10，控制 `/use` session 时回放多少个最近 turn。`mediaInput` 控制是否接收平台图片和文件。

## 微信

微信 adapter 使用 `weixin-agent` 的扫码登录流程。

1. 将 `channels.weixin.enabled` 设为 `true`。
2. 重启 daemon。
3. 运行 `dwo channel weixin bind`。
4. 使用微信扫描终端二维码，并按手机提示确认；若要求验证码，在终端输入。
5. 运行 `dwo channel weixin status` 检查绑定状态。

解绑：

```text
dwo channel weixin unbind
```

## Telegram

Telegram 使用 Bot API long polling，不需要 webhook 或公网地址。

1. 在 Telegram 中通过 BotFather 创建 bot，取得 token。
2. 将 token 放入 `botTokenEnv` 指向的环境变量，例如 `TELEGRAM_BOT_TOKEN`。
3. 将 `channels.telegram.enabled` 设为 `true`；需要代理时设置 `tgProxy`。
4. 重启 daemon。
5. 运行 `dwo channel telegram bind`，终端会显示一次性绑定命令。
6. 在 bot 私聊中发送 `/bind <code>`。
7. 运行 `dwo channel telegram status`。

PowerShell 当前进程示例：

```powershell
$env:TELEGRAM_BOT_TOKEN = "123456:token"
dwo daemon start
```

Linux/macOS：

```bash
export TELEGRAM_BOT_TOKEN="123456:token"
dwo daemon start
```

Token 不写入 channel secret 文件。`channels/telegram/secret.yaml` 只保存 bot 标识以及绑定的 user/chat。

## 飞书 / Lark

飞书和 Lark 使用开放平台 WebSocket 长连接，不需要公网 webhook。

1. 在飞书或 Lark 开放平台创建企业自建应用并启用机器人。
2. 选择长连接事件订阅，订阅 `im.message.receive_v1`。
3. 开通接收消息、以应用身份发送消息、获取消息资源、上传图片/文件所需权限。
4. 发布应用，使目标用户可以私聊机器人。
5. 把 App ID 与 App Secret 放入 `appIdEnv`、`appSecretEnv` 指向的环境变量。
6. 将 `channels.feishu.enabled` 设为 `true`；国内飞书使用 `platform: feishu`，海外 Lark 使用 `platform: lark`。
7. 重启 daemon，运行 `dwo channel feishu bind`。
8. 在机器人私聊中发送终端显示的 `/bind <code>`。
9. 运行 `dwo channel feishu status`。

凭据环境变量示例：

```powershell
$env:FEISHU_APP_ID = "cli_xxx"
$env:FEISHU_APP_SECRET = "xxx"
```

`channels/feishu/secret.yaml` 只保存绑定的 `open_id` 和 `chat_id`，不会保存 App ID 或 App Secret。

## Slash Commands

三种 channel 使用同一个命令定义，因此参数校验和 `/help` 内容一致。

| 命令 | 说明 |
| --- | --- |
| `/help` | 显示当前支持的命令 |
| `/list` | 列出全局 session，`*` 表示当前 channel 选择的 session |
| `/new [NAME] [--cwd <PATH>]` | 创建并选择 session；名称可包含空格 |
| `/use <SESSION>` | 选择已有 session，并回放最近 turn |
| `/status` | 显示当前 session 的 cwd、模型、reasoning、policy 和运行状态 |
| `/del <SESSION>` | 删除指定 session |
| `/cancel` | 请求取消当前 turn |
| `/model <NAME>` | 切换当前 session 的模型 alias |
| `/reasoning <LEVEL\|off>` | 修改 reasoning mode，或使用 `off` 关闭 |
| `/policy [full_access\|confirm\|watch]` | 不带参数查看 policy，带参数修改 |
| `/allow [ID]` | 允许当前 pending permission，或指定 request ID |
| `/deny [ID]` | 拒绝当前 pending permission，或指定 request ID |

带空格的工作目录需要引号：

```text
/new Project review --cwd "C:\Users\Example User\Documents\repo"
```

Telegram 发来的 `/status@bot_name` 也会正确识别。

## 对话、文件与权限

- 普通私聊文本会提交到当前 session；没有选择 session 时，adapter 会自动创建并选中一个默认 session。使用 `/new` 可以显式指定名称和 cwd，使用 `/use` 可以切换到已有 session。
- 微信媒体、Telegram photo/document/video、飞书 image/file 会下载到当前 session 的 `runtime/attachments/<channel>/...`，再以 resource link 交给模型。
- 仅发送媒体也是有效 prompt。
- 在 `confirm` 模式中，channel 会显示 permission request。直接发送 `/allow` 或 `/deny` 即可处理当前请求，也可以附带 ID。
- `/cancel` 是显式中断入口；发送新的普通消息不会隐式取消正在运行的工具。

## 主动发送

以下 CLI 命令始终发送到对应 channel 已绑定的私聊目标：

```text
dwo channel weixin send-message "任务完成"
dwo channel telegram send-file ./report.pdf
dwo channel feishu send-message "请查看最新结果"
```

Agent 只应在用户明确要求主动发送消息或文件时调用这些命令。普通回答已经由 channel adapter 自动返回，不需要重复发送。

## 状态与排查

```text
dwo channel list
dwo channel <weixin|telegram|feishu> status
```

`connected` 表示持久化绑定有效；Telegram 还要求 token 环境变量可读取，飞书/Lark 还要求 App ID/Secret 环境变量可读取。它不代表实时网络健康。排查顺序：

1. 确认 `enabled: true` 且 daemon 已在配置修改后重启。
2. 确认 daemon 进程能读取相应环境变量。
3. Telegram 检查 token 与可选代理；飞书检查平台、应用发布状态、事件和权限。
4. 重新执行 `bind`，确认消息来自目标 bot 的私聊。
5. 查看 `~/.dwoagent/runtime/logs/`。
