# dwo 命令参考

`dwo` 是本地 daemon 的控制 CLI。除 `serve` 外，命令都通过 profile 的本地 IPC 连接已经运行的 daemon。默认 profile 是 `~/.dwoagent/profile.yaml`，也可以用全局 `--config-path <path>` 指定。

## 生命周期

```text
dwo install [--start]
dwo uninstall [--purge]
dwo serve
dwo daemon start
dwo daemon stop
dwo daemon status
```

`install` 创建固定的 profile/resource/runtime 目录并注册 daemon 自启动任务。`--start` 同时启动 daemon。`serve` 在前台运行 host；通常由系统任务或 `daemon start` 管理。`daemon status` 返回 YAML 风格的健康状态、session 数量、channel 数量和 automation 数量。

daemon 启动 host 时会并发初始化 `resource/mcp.json` 中的全部 MCP server。每个 server 会等待到 `ready`、`auth_required` 或 `failed`；stdio/HTTP 连接由 daemon 持续托管并复用。新增或修改配置由 watcher 使用相同流程初始化。`runtime/mcp/catalog.json` 是内存 catalog 的派生投影，不是活跃连接的凭据。

## Session

```text
dwo session list
dwo session new [name] [--cwd <path>]
dwo session delete <session-id>
dwo session prompt <session-id> <message>
dwo session cancel <session-id>
dwo session watch <session-id>
dwo session model <session-id> <model>
dwo session reasoning <session-id> [reasoning]
dwo session approve <session-id> <permission-id>
dwo session deny <session-id> <permission-id> [reason]
```

`session list` 只输出 session id 和标题。`session watch` 先输出当前 snapshot，然后持续流式输出 reasoning、tool call、tool result、answer 和状态事件；它是持续订阅，使用 `Ctrl+C` 退出。

普通 CLI 输出使用适合终端阅读的 YAML 风格文本，不把 JSONL 直接打印给用户。`session prompt`、`session new` 等命令的响应也遵循这一规则。

`session cancel` 只请求取消当前 turn。正在运行的 terminal/tool future 会先清理，session 等待取消结果后才恢复 idle；取消期间提交的新 prompt 会排队到旧 turn 完成之后。

`session model` 的模型切换应用于后续 model request。若目标模型支持图片则直接切换；若目标模型是纯文本模型且当前 context 含图片，idle session 会先使用当前或最后成功的视觉模型生成文字摘要，再重建无图 model context。摘要失败时模型和 context 都不改变；图片 turn 运行期间不允许降级切换。client transcript 始终保留原始图片用于 replay，文本模型也会在持久化前拒绝新的图片 prompt。迁移成功会将当前 context token 重置为 0，并按目标模型窗口发送新的 usage update。

Session 文件布局为：

```text
runtime/sessions/YYYY/MM/DD/<session-id>/
|- session.json
|- model_context.json
`- client_transcript.jsonl
```

`session.json` 保存 session 配置，`model_context.json` 保存当前模型上下文和 usage，`client_transcript.jsonl` 是追加式的完整客户端事件流。

## MCP

```text
dwo mcp search <query>
dwo mcp call <server.tool> --args '<json>'
dwo mcp auth <server> [--logout]
```

`search` 只查询 daemon 当前的内存 catalog，不启动或重连 server。查询按 server 名称/描述和 tool 名称/描述匹配：

- 只命中 server：列出该 server 的全部工具，但不展开 schema。
- 只命中 tool：只列出匹配工具，并展开其输入 schema。
- 两者同时命中：列出 server 的全部工具，只展开直接命中的工具 schema。

`call` 使用 `server.tool` selector 调用已发现的工具；daemon 会复用托管连接，连接失效时按需重连并刷新 catalog。`auth` 启动 OAuth 登录，`--logout` 删除授权并使该 server 重新进入初始化流程。

MCP 命令输出为 YAML 风格文本。只有 `--args` 的工具参数使用 JSON，因为它们会原样作为 MCP 调用 payload。

## Channel

```text
dwo channel list
dwo channel weixin status
dwo channel weixin bind
dwo channel weixin unbind
dwo channel weixin send-message <message>
dwo channel weixin send-file <path>
dwo channel telegram status
dwo channel telegram bind
dwo channel telegram unbind
dwo channel telegram send-message <message>
dwo channel telegram send-file <path>
dwo channel feishu status
dwo channel feishu bind
dwo channel feishu unbind
dwo channel feishu send-message <message>
dwo channel feishu send-file <path>
```

`weixin bind` 在终端显示 QR 登录流程。`telegram bind` 从 `botTokenEnv` 读取 BotFather token，终端显示一次性 `/bind <code>`；在 bot 私聊中发送后，daemon 把该 user/chat 写入 `channels/telegram/secret.yaml`。`feishu bind` 从 `appIdEnv`/`appSecretEnv` 读取企业自建应用凭据，临时建立长连接并等待同样的私聊命令，随后只把 `open_id/chat_id` 写入 `channels/feishu/secret.yaml`。只有绑定用户和私聊可以使用对应 bot，token/App Secret 都不会落盘。

Telegram 使用 long polling，不需要 webhook 或公网地址。`tgProxy` 是仅作用于 Telegram Bot API 和媒体下载的可选 HTTP 代理。入站 photo、document、video 下载到 `runtime/attachments/telegram/YYYY/MM/DD/<session-id>/` 并作为带本地路径、MIME、文件名和大小的 resource link 提交。输出使用 Telegram plain text，不启用 parse mode，也不修改模型文本。

Feishu/Lark 使用 `openlark` WebSocket 长连接，也不需要 webhook 或公网地址。`platform: feishu` 对应国内开放平台，`platform: lark` 对应海外开放平台。应用必须启用机器人、以长连接订阅 `im.message.receive_v1`，并开通接收消息、以应用身份发送消息、获取和上传消息资源的权限。入站 text、image、file 均可触发 prompt；image/file 下载到 `runtime/attachments/feishu/YYYY/MM/DD/<session-id>/`。输出使用 plain text。

不同 channel 可以 `/use` 同一个全局 session，也各自在自己的 `runtime.yaml` 中保持当前选择。绑定、解绑或重绑某个 channel 只重启该 channel，不影响其他 channel。

在 `confirm` 模式下，收到授权请求后直接发送 `/allow` 或 `/deny` 即可处理当前 pending permission，不需要复制 request ID。仍可使用 `/allow <id>`、`/deny <id>` 显式指定请求。

## Automation

```text
dwo automation list [--json]
dwo automation status [--json]
dwo automation run <job> [--json]
```

默认输出为可读文本；仅指定 `--json` 时保留机器读取的 JSON 输出。

## ACP

```text
dwo acp
```

ACP 使用 stdio 连接同一个 daemon，共享 session、事件流、模型配置和 tool runtime，不会创建独立的 session 或 MCP 连接。

ACP prompt 会按原顺序把普通文本、文本型 embedded resource 和 resource link 展开为文本。embedded resource 保留 URI、MIME 和正文，resource link 保留名称、URI 与可用元数据，因此 Zed 引用的本地文件或目录会作为明确路径进入模型上下文。当前不声明图片或音频输入能力，并拒绝 image、audio 与二进制 embedded resource；`embeddedContext` 仅用于客户端粘贴文本文件内容。
