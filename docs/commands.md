# dwo 命令参考

`dwo` 是本地 daemon 的控制 CLI。除 `serve` 外，命令都通过本地 IPC 连接已经运行的 daemon。配置固定为 `~/.dwoagent/profile.yaml`。

面向首次使用者的安装与对话流程见根目录 [README](../README.md)；ACP 客户端接入见 [ACP 使用指南](acp.md)，消息平台部署见 [Channel 部署与使用](channels.md)，对话中的 `/` 命令见 [Slash Commands 使用指南](slash-commands.md)。

## 生命周期

```text
dwo install [--start]
dwo uninstall [--purge]
dwo serve
dwo daemon start
dwo daemon stop
dwo daemon status
```

`install` 把当前 CLI 复制到 `~/.dwoagent/bin/dwo`（Windows 为 `dwo.exe`），在 Windows 用户级 PATH 中幂等加入该目录，创建固定的 profile/resource/runtime/logs 目录，并使用安装后的固定路径注册 daemon 自启动任务。Windows 的隐藏启动 launcher 与 executable 一起放在 `bin/`，不属于 runtime。`--start` 同时启动 daemon。`serve` 在前台运行 host；通常由系统任务或 `daemon start` 管理。`daemon status` 返回 YAML 风格的健康状态、session 数量、channel 数量和 automation 数量。

daemon 启动 host 时会并发初始化 `resource/mcp/mcp.json` 中的全部 MCP server。每个 server 会等待到 `ready`、`auth_required` 或 `failed`；stdio/HTTP 连接由 daemon 持续托管并复用。新增或修改配置由 watcher 使用相同流程初始化。MCP catalog 只保存在 daemon 内存中，连接是否可用仍以 daemon 内的运行状态为准。

## Session

本节用于查询具体命令和参数。父子 session、继承规则与结果回传流程见 [Subsessions 使用指南](subsessions.md)。

```text
dwo config-show
dwo session list [--all]
dwo session status <session-id> [--json]
dwo session delete <session-id>
dwo session prompt <message> [--title <title>] [--cwd <path>] [--policy <policy>] [--model <model>] [--reasoning <mode>] [--to <session-id> | --from <session-id>]
dwo session set <session-id> [--title <title>] [--policy <policy>] [--model <model>] [--reasoning <mode>]
dwo session cancel <session-id>
dwo session watch <session-id> [--cursor <cursor>] [--limit <count>]
dwo session approve <session-id> <permission-id>
dwo session deny <session-id> <permission-id>
```

agent 进程通过 `DWO_SESSION_ID` 标识当前 session。`session prompt` 不带 `--to` 或 `--from` 时创建当前 agent 的直接子 session，默认继承父 session 的 cwd、policy、model 和 reasoning；带 `--to` 时继续指定的直接子 session；带 `--from` 时复制指定直接子 session 的 context 和 transcript，再把 prompt 发给副本。子 session 的 policy 不得比父 session 更宽松。外部人工终端没有当前 session，因此默认创建根 session。

`--from` 和 `--to` 严格互斥。`--title` 可用于新建或重命名 fork；`--cwd` 只用于全新 session，和 `--to` 或 `--from` 同时使用会被拒绝。来源必须处于 idle，fork 会保留来源的 cwd、父子关系和配置。`--policy` 接受 `full_access`、`confirm` 或 `watch`；`--model` 和 `--reasoning` 必须是 `config-show` 中列出的有效组合。继续或 fork session 时，policy/model/reasoning 更新会在提交新 prompt 前写入目标配置。

`session set` 原子修改已有 session 的标题、policy、model 和 reasoning，至少需要提供一个选项。Agent 内只能修改自己的直接子 session，且不能把子 session 的 policy 提升到父 session 以上；普通终端可以按 session ID 修改任意已有 session。

`session list` 默认只列出当前 agent 的直接子 session；外部终端默认列出根 session。`--all` 列出 Host 中的全部 session。输出包含总数、运行状态、模型、更新时间和标题。`session status` 显示单个 session 的配置、usage、active turn 和最后一条最终回答；回答会折叠空白并限制在 100 个字符内。完整内容继续使用 `session watch`。`config-show` 输出默认 policy、可用 model/reasoning、默认 model、全局 `maxModelSteps` 和 session 总数。

`session watch` 默认返回最近 3 个内容事件及 `next_cursor`，不会建立持续广播。传入 `--cursor <next_cursor>` 可读取之后的事件，`--limit` 范围为 1 到 100。普通 CLI 输出使用适合终端阅读的 YAML 风格文本。

子 session 的 turn 结束后，daemon 自动向父 agent 投递 `<subsession_result>` internal message，其中包含子 session ID、状态、最终文本和可选错误。它不会产生 `UserPromptSubmitted` 事件。父 agent idle 时立即启动；父 agent 正在运行时先缓冲，并在当前 model response 或 tool-call batch 完成后的安全边界写入上下文。

active turn 运行期间收到的新 prompt 会进入 session FIFO，在当前 model response 或 tool-call batch 完成后按顺序写入同一个 turn，不会隐式 cancel 或关闭 tools。内部 watcher/子 session 消息使用 internal context message，不会伪装成用户事件。

`session cancel` 是唯一主动中断当前 turn 的入口。取消会清理排队的用户 prompt；已经到达的 internal watcher 消息仍写入 context，但不会因此启动下一次模型调用。

`session prompt --to ... --model ...` 的模型切换应用于后续 model request。若目标模型支持图片则直接切换；若目标模型是纯文本模型且当前 context 含图片，idle session 会先使用当前或最后成功的视觉模型生成文字摘要，再重建无图 model context。摘要失败时模型和 context 都不改变；图片 turn 运行期间不允许降级切换。client transcript 始终保留原始图片用于 replay，文本模型也会在持久化前拒绝新的图片 prompt。迁移成功会将当前 context token 重置为 0，并按目标模型窗口发送新的 usage update。

若新模型属于另一个 provider instance，daemon 还会在下一次请求前移除只对旧 provider 有效的 reasoning 和托管工具调用。用户与 assistant 消息、本地工具 call/result 以及完整 client transcript 都会保留；同一 provider 内切换模型不会触发这项裁剪。

Session 文件布局和持久化说明见 [Profile 配置指南](profile.md#session-数据)。

## Model

```text
dwo model list
dwo model get-default
dwo model set-default <provider/model> --reasoning <mode> [--compaction-trigger-ratio <ratio>]
```

`list` 按 provider 分组列出当前有效模型的显示名称、稳定 ID、图片输入/tool call/hosted tool 能力和全部 reasoning mode。`get-default` 显示新 session 的默认 model/reasoning 和全局 `compactionTriggerRatio`。`set-default` 的 `--reasoning` 必填，daemon 会校验该 mode 属于指定模型；可选的 `--compaction-trigger-ratio` 同时更新没有单模型 override 时使用的全局压缩触发比例，有效范围为 `(0, 1]`。这些设置会原子写入 `profile.yaml`，影响之后创建的 session，不覆盖已有 session 的明确选择。

## MCP

```text
dwo mcp list
dwo mcp get <name>
dwo mcp add [-t|--transport <stdio|http>] [-e|--env KEY=value] [-H|--header "Name: value"] <name> [<url> | -- <command> [args...]]
dwo mcp add-json <name> <json>
dwo mcp remove <name>
dwo mcp search <query>
dwo mcp call <server.tool> --args '<json>'
dwo mcp auth <server> [--logout]
```

`list` 列出已配置的 MCP server 名称、当前状态和工具数量，不展开工具详情。`get` 显示单个 server 的脱敏配置和 runtime status/tool catalog。`add` 的参数与 Claude Code MCP CLI 对齐：默认 transport 是 `stdio`，将命令及其参数放在 `--` 后；HTTP server 使用 `-t http <url>`，`-e` 仅用于 stdio env，`-H` 仅用于 HTTP header。DWO 当前支持 `stdio` 和 streamable HTTP，`streamable-http` 作为 HTTP 配置别名可用于 `add-json`。`add-json` 接受单个 server entry JSON，不接受 `mcpServers` 包装对象。`remove` 删除配置并关闭不再使用的托管连接。

`search` 只查询 daemon 当前的内存 catalog，不启动或重连 server。查询按 server 名称/描述和 tool 名称/描述匹配：

- 只命中 server：列出该 server 的全部工具，但不展开 schema。
- 只命中 tool：只列出匹配工具，并展开其输入 schema。
- 两者同时命中：列出 server 的全部工具，只展开直接命中的工具 schema。

`call` 使用 `server.tool` selector 调用已发现的工具；daemon 会复用托管连接，连接失效时按需重连并刷新 catalog。`auth` 启动 OAuth 登录，`--logout` 删除授权并使该 server 重新进入初始化流程。

MCP 命令输出为 YAML 风格文本。只有 `--args` 的工具参数使用 JSON，因为它们会原样作为 MCP 调用 payload。

## Skills

```text
dwo skills list
dwo skills add <file-or-directory> [--name <name>]
dwo skills remove <name>
```

`add` 接受单个 `.md` 文件或 skill 目录。单文件会安装为 `<name>/SKILL.md`，默认名称取文件名；目录会递归导入并要求根目录存在 `SKILL.md`，以保留 references、scripts 和 assets。`--name` 可覆盖目标名称。`list` 列出 active 和 disabled skills；`remove` 删除任一状态下的同名 skill。只有 active skills 会在构建 system prompt 时写入上下文。

## Channel

本节是 CLI 命令参考。各平台的环境变量、开放平台配置、绑定步骤和聊天内命令见 [Channel 部署与使用](channels.md)。

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
dwo channel qq status
dwo channel qq bind
dwo channel qq unbind
dwo channel qq send-message <message>
dwo channel qq send-file <path>
dwo websocket status
dwo websocket token
dwo websocket reset-token
```

`weixin bind` 在终端显示 QR 登录流程。`telegram bind` 从 `botTokenEnv` 读取 BotFather token，终端显示一次性 `/bind <code>`；在 bot 私聊中发送后，daemon 把该 user/chat 写入 `channels/telegram/secret.yaml`。`feishu bind` 从 `appIdEnv`/`appSecretEnv` 读取企业自建应用凭据，临时建立长连接并等待同样的私聊命令，随后只把 `open_id/chat_id` 写入 `channels/feishu/secret.yaml`。`qq bind` 只使用 QQ 官方二维码绑定，并要求扫码结果带有单用户 `userOpenid`。QQ 扫码返回的 AppID/AppSecret 保存在受限权限的 `channels/qq/secret.yaml`。

Telegram 使用 long polling，不需要 webhook 或公网地址。`tgProxy` 是仅作用于 Telegram Bot API 和媒体下载的可选 HTTP 代理。入站 photo、document、video 下载到 `runtime/attachments/telegram/YYYY/MM/DD/<session-id>/` 并作为带本地路径、MIME、文件名和大小的 resource link 提交。输出使用 Telegram plain text，不启用 parse mode，也不修改模型文本。

Feishu/Lark 使用 `openlark` WebSocket 长连接，也不需要 webhook 或公网地址。`platform: feishu` 对应国内开放平台，`platform: lark` 对应海外开放平台。应用必须启用机器人、以长连接订阅 `im.message.receive_v1`，并开通接收消息、以应用身份发送消息、获取和上传消息资源的权限。入站 text、image、file 均可触发 prompt；image/file 下载到 `runtime/attachments/feishu/YYYY/MM/DD/<session-id>/`。输出使用 plain text。

独立 WebSocket transport 在配置的 `<bind>:<port>` 上提供 `/acp` 和 `/dwo`。token 自动保存到 `runtime/websocket/secret.yaml`；`token` 显示两条路径各自的连接凭据，`reset-token` 使旧 token 失效并断开已有连接。

不同 channel 可以 `/use` 同一个全局 session，也会在各自的 `runtime.yaml` 中保持当前选择。绑定、解绑或重绑某个 channel 只重启该 channel，不影响其他 channel。

在 `confirm` 模式下，收到授权请求后直接发送 `/allow` 或 `/deny` 即可处理当前 pending permission，不需要复制 request ID。仍可使用 `/allow <id>`、`/deny <id>` 显式指定请求。

## Automation

配置示例、cron、时区、session 模式和无人值守权限行为见 [Automation 使用指南](automation.md)。

```text
dwo automation --project <id> list [--json]
dwo automation --project <id> status <job> [--json]
dwo automation --project <id> add <job> --cron <expr> --prompt <text> [--topic <id>] [--session every-time|once|fixed] [--session-id <id>] [--title <title>] [--disabled] [--json]
dwo automation --project <id> enable <job>
dwo automation --project <id> enable --all
dwo automation --project <id> disable <job>
dwo automation --project <id> disable --all
dwo automation --project <id> delete <job>
dwo automation --project <id> delete --all --yes
dwo automation --project <id> run <job> [--json]
```

默认输出为可读文本；仅指定 `--json` 时保留机器读取的 JSON 输出。

`add` 默认创建启用的 `new + every_time` 任务；`--session once` 复用 sticky session，`--session fixed` 必须同时提供 `--session-id`。`enable` 和 `disable` 接受任务名或 `--all`，只控制定时调度；手动 `run` 仍可执行 disabled 任务。`delete --all` 必须显式提供 `--yes`。

Automation Job 属于 Project，配置和 history 写在 `runtime/projects/<project-id>/automation/`。Agent Session 内可省略 `--project`，CLI 会从 `DWO_SESSION_ID` 推导当前 Project；外部 shell 必须显式指定。`automation delete` 在存在 `DWO_SESSION_ID` 时被拒绝。`automation run` 在 session 已创建或解析、prompt 已成功提交后返回 `runId`、`sessionId` 和 `turnId`，但不等待 Agent 完成。在 Agent session 内调用时，最终结果或错误会作为 `<automation_result>` 内部消息自动进入调用方上下文，无需轮询。

## ACP

客户端配置、session 协作、权限和内容类型限制见 [ACP 使用指南](acp.md)。

```text
dwo acp [--protocol v1|v2]
```

ACP 使用 stdio 连接同一个 daemon，共享 session、事件流、模型配置和 tool runtime，不会创建独立的 Host 或 MCP 连接。非空的 session `mcpServers` 和 `additionalDirectories` 会被拒绝。`--protocol` 默认是 `v2`；v1 保持 `session/prompt` 到 turn 结束，v2 接受 prompt 后立即响应并通过 `state_update` 报告完成。

ACP prompt 会按原顺序处理文本、图片、文本型 embedded resource 和 resource link。embedded resource 保留 URI、MIME 和正文，resource link 保留名称、URI 与可用元数据，因此 Zed 引用的本地文件或目录会作为明确路径进入模型上下文。图片只会交给支持 image input 的模型；audio 和二进制 embedded resource 会被拒绝。Zed 的 Send now 所产生的同 session `cancel + prompt` 会在 500ms 窗口内合并为排队 prompt，单独 cancel 仍会正常中断 turn。
