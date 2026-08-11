# Channel 部署与使用

赤铎可以把同一个 daemon 接到微信、Telegram、飞书/Lark、QQ Bot 私聊和 ACP WebSocket。所有入口共享全局 session、模型、工具、MCP 和持久化数据。

四个消息 channel 只接受已绑定用户的私聊；WebSocket 使用独立 token 鉴权。修改 `profile.yaml` 后重启 daemon，使 adapter 按新配置启动：

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
    replayMode: response
    markdownFilter: true
  telegram:
    enabled: false
    replayTurns: 5
    replayMode: response
    botTokenEnv: TELEGRAM_BOT_TOKEN
    tgProxy: null
    mediaInput: true
  feishu:
    enabled: false
    replayTurns: 5
    replayMode: response
    appIdEnv: FEISHU_APP_ID
    appSecretEnv: FEISHU_APP_SECRET
    platform: feishu
    mediaInput: true
  qq:
    enabled: false
    replayTurns: 5
    replayMode: response
    mediaInput: true
  websocket:
    enabled: false
    port: 8765
```

`replayTurns` 最大为 10，控制 `/use` session 时回放多少个最近 turn。`replayMode` 支持 `response` 和 `full`：前者只发送最终回答，后者还发送 reasoning 和 tool-call 阶段；微信固定使用 `response`。`mediaInput` 控制是否接收平台图片和文件。

## QQ Bot

QQ channel 只支持单用户 C2C 私聊，不支持群聊。配置最小化为：

```yaml
channels:
  qq:
    enabled: true
    replayTurns: 5
    mediaInput: true
```

运行 `dwo channel qq bind`，终端会显示 QQ 官方二维码。扫码成功后，QQ 返回的 AppID、AppSecret 和扫码用户 OpenID 会写入私有的 `channels/qq/secret.yaml`；如果官方结果没有返回 `userOpenid`，绑定会失败，不会自动绑定第一个发消息的人。

QQ 支持私聊文本和附件入站、文本回复以及主动发送本地文件。出站文件首版限制为 20 MiB。`response` 模式只在 turn 完成时发送一次逻辑上的最终回答，短回答使用一次被动回复；如果回答超过单条长度，首个分片使用被动回复，后续分片才改用主动消息。`full` 模式的 reasoning、tool-call 和最终回答全部使用主动消息。主动消息仍受 QQ 的用户授权、频率和每日额度限制。`confirm` 模式下，工具批准会显示 QQ Markdown 消息和“允许/拒绝”按钮，点击只对绑定用户生效。

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

## ACP WebSocket

WebSocket channel 把现有 ACP 协议开放给网页客户端。它不使用 slash commands，也没有绑定用户或当前 session；每个连接都是独立的 ACP client。

```yaml
channels:
  websocket:
    enabled: true
    port: 8765
```

重启 daemon 后，服务固定监听 `0.0.0.0:8765`，路径固定为 `/acp`。首次启用时会生成 256-bit token，并保存到 `channels/websocket/secret.yaml`。

查看状态和 token：

```text
dwo channel websocket status
dwo channel websocket token
dwo channel websocket reset-token
```

网页连接示例：

```js
const ws = new WebSocket(
  "ws://192.168.1.20:8765/acp?token=" + encodeURIComponent(token)
);
```

一条 ACP JSON-RPC 消息对应一个 WebSocket text frame。Binary frame 会被拒绝。重置 token 会立即断开已有连接，旧 token 随即失效。

局域网使用前需要在系统防火墙中放行对应 TCP 端口。公网不要直接暴露明文 `ws://`；应通过 Caddy、Nginx 或其他反向代理提供 TLS，并使用 `wss://`。query token 可能进入代理访问日志，代理应避免记录完整 query string。

## Slash Commands

四个消息 channel 使用同一个命令定义，因此参数校验和 `/help` 内容一致。WebSocket 直接使用 ACP；ACP client 会收到 Agent 宣告的 `/compact`、`/resume` 和 `/fork`。

| 命令 | 说明 |
| --- | --- |
| `/help` | 显示当前支持的命令 |
| `/list` | 列出全局 session，`*` 表示当前 channel 选择的 session |
| `/new [NAME] [--cwd <PATH>]` | 创建并选择 session；名称可包含空格 |
| `/fork` | 把当前 session 复制为一个新 session，并返回新 ID；当前选择不变 |
| `/use <SESSION>` | 选择已有 session，并回放最近 turn |
| `/status` | 显示当前 session 的 cwd、模型、reasoning、policy 和运行状态 |
| `/del <SESSION>` | 删除指定 session |
| `/cancel` | 请求取消当前 turn |
| `/compact` | 手动压缩当前 session context；命令本身不进入模型上下文 |
| `/resume` | 仅在 session idle 时继续上一项工作；运行中静默忽略 |
| `/model <NAME>` | 切换当前 session 的模型 alias |
| `/reasoning <LEVEL\|off>` | 修改 reasoning mode，或使用 `off` 关闭 |
| `/policy [full_access\|confirm\|watch]` | 不带参数查看 policy，带参数修改 |
| `/allow [ID]` | 允许当前 pending permission，或指定 request ID |
| `/deny [ID]` | 拒绝当前 pending permission，或指定 request ID |
| `/skill <NAME> [PROMPT]` | 要求 Agent 使用当前 session 可用的指定 skill |
| `/mcp <NAME> [PROMPT]` | 要求 Agent 使用已配置的指定 MCP server |

`/skill` 和 `/mcp` 是进入模型的 prompt directive，不是本地 session 控制命令。它们可以出现在正文任意位置，同一条消息可以重复或混合使用，例如：

```text
先 /skill review 检查改动，再 /mcp github 创建 issue，最后 /skill summarize
```

daemon 只替换名称与当前有效 catalog 精确匹配的 directive：skill 使用 profile、`externalSkillsDirs` 和 `<session-cwd>/.agents/skills/` 合并后的结果，MCP 使用当前 daemon catalog。匹配成功后会插入带名称和路径或 MCP 搜索要求的 XML block，提示 Agent 先用 `read_file` 读取 `SKILL.md`，或先在终端运行 `dwo mcp search`。未知名称、单独的 `/skill`、`/skill `、`/mcp` 和 `/mcp ` 都保持原文并作为普通 prompt 透传，不产生额外提示。

带空格的工作目录需要引号：

```text
/new Project review --cwd "C:\Users\Example User\Documents\repo"
```

Telegram 发来的 `/status@bot_name` 也会正确识别。

## 对话、文件与权限

- 普通私聊文本会提交到当前 session；没有选择 session 时，adapter 会自动创建并选中一个默认 session。使用 `/new` 可以显式指定名称和 cwd，使用 `/fork` 可以复制当前话题但不会切换，使用 `/use` 可以切换到已有 session。
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
dwo channel qq send-file ./report.zip
```

Agent 只应在用户明确要求主动发送消息或文件时调用这些命令。普通回答已经由 channel adapter 自动返回，不需要重复发送。

## 状态与排查

```text
dwo channel list
dwo channel <weixin|telegram|feishu|qq|websocket> status
```

`connected` 表示持久化绑定有效；Telegram 还要求 token 环境变量可读取，飞书/Lark 还要求 App ID/Secret 环境变量可读取。QQ 的 AppID/AppSecret 由二维码绑定写入私有 secret 文件。它不代表实时网络健康。排查顺序：

1. 确认 `enabled: true` 且 daemon 已在配置修改后重启。
2. 确认 daemon 进程能读取相应环境变量。
3. Telegram 检查 token 与可选代理；飞书检查平台、应用发布状态、事件和权限。
4. 重新执行 `bind`，确认消息来自目标 bot 的私聊。
5. 查看 `~/.dwoagent/logs/`。
