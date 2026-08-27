# Dwo Management RPC v3 契约

管理协议使用 JSON-RPC 2.0 envelope。IPC 请求必须声明 `route: "dwo"`；远程客户端连接
`/dwo?token=<management-token>` 后发送同样的 envelope。聊天仍走 ACP v2，不通过本文件中的
配置方法。

Rust 二开入口分成两层：[dwo-protocol API](../crates/dwo-protocol/README.md) 提供稳定的
envelope、错误、capabilities 和方法注册；[dwo-host API](../crates/dwo-host/README.md)
提供 Host 生命周期、直接 Session API、管理请求和事件订阅。本文集中说明线上可调用的
Management RPC 方法。

## 能力发现

客户端启动后先调用 `dwo.capabilities`。返回值是当前 Host 真正支持的方法、结构化
`methodSpecs`、事件、协议版本、request id 和 event cursor 能力；客户端不得把静态列表
当作运行时真相。`methodSpecs` 的每项包含路由、query/command/subscription 操作类型、
是否有副作用以及对应事件（如果有）。

Flutter/Dart 的 transport-neutral binding 位于 `bindings/dart/lib/dwo_rpc.dart`；它提供
`DwoRpcClient`、`DwoCapabilities`、`DwoEventSubscription` 和 WebSocket `/dwo` transport。
本地 named pipe 只需实现同一 `DwoTransport` 接口即可复用所有管理调用。

聊天方法只通过 ACP 路由提供。Dwo 路由保留 Session 查询、生命周期管理和配置管理；
`session.prompt`、`session.cancel`、`session.permission`、模型/思维链/policy 选项和
`session.watch` 不属于 Dwo 管理入口。

管理事件可通过 `event.read` 按 cursor 读取，或使用 `event.subscribe` 在专用 IPC 连接/
现有 WebSocket 连接上接收 replay 和 live event。默认不传 `event` 时发送全部管理事件；
传入事件名后，IPC/WebSocket 会同时过滤 replay 和 live。当前事件包括 `config.changed`、
`config.apply_failed`、`mcp.status`、`automation.changed` 和 `automation.run`；事件只由
Host 发布，客户端断开不会停止 Host。当前事件还包括 `channel.status`、`skill.changed` 和
`project.changed`。

## Project 与看板

Project 拥有 `pwd`、Board 和 automation 配置/历史；Topic 保存 `sessionIds` 与 `labelIds`，
Automation Job 保存可选的 `topicId`。完整数据关系、默认未分类话题和文件布局见
[Project 与看板](project-board.md)。

| 方法 | 用途 |
| --- | --- |
| `project.list/get/create/update` | Project 查询、创建和改名 |
| `project.board` | 获取 Sections、Topics 和 Labels |
| `project.section.create/update/delete/reorder` | 分区 CRUD 和排序 |
| `project.topic.get/create/update/delete/move/reorder` | Topic 管理；`get` 聚合 Session/Automation 状态 |
| `project.topic.overview.get/set` | 读写概述与计划 Markdown |
| `project.topic.agents.get/set` | 读写 Knowledge `AGENTS.md` |
| `project.topic.session.assign/unassign` | Session 归类或移回未分类 Topic |
| `project.label.create/update/delete/assign/unassign` | Board 标签管理 |

`session.new` 可传 `project_id` 与可选 `topic_id`；指定 Project 时不能再传 `cwd`。省略
`topic_id` 使用该 Project 的未分类话题。省略 `project_id` 时 Host 按 canonical cwd 查找或
创建 Project，没有 `cwd` 则创建 Project workspace。ACP new-session 因而可以只传 cwd；
重复使用同一 cwd 会复用 Project。看板变更发布 `project.changed`。

## Host 配置与模型

| 方法 | 用途 |
| --- | --- |
| `config.snapshot` | Host 配置摘要、默认模型、模型选项、全局 `maxModelSteps` |
| `config.update` | 修改 logging、external skill dirs、external rule files、全局 `maxModelSteps` |
| `model.list` | 模型、Provider 和默认模型配置；credential 已脱敏 |
| `model.available` | 已解析的可用模型、能力、reasoning mode 与当前默认选择 |
| `model.get_default` | 当前 `{model, reasoning, compactionTriggerRatio}` 默认选择 |
| `model.set_default` | 以 `{model, reasoning?, compactionTriggerRatio?}` 修改 Host 默认选择；省略压缩比时保留现值 |
| `model.upsert` | 以 `{provider, name, model}` 新增或替换 Provider 内的显示名模型映射 |
| `model.remove` | 以 `{provider, modelId}` 删除显式模型映射 |
| `provider.list` | Provider 配置与 credential 是否存在，不回显 key |
| `provider.upsert` / `provider.remove` | 新增、替换或删除 Provider |
| `model.catalog.list` | 查看内置和 `resource/models/*.yaml` Model List family |
| `model.catalog.upsert` / `model.catalog.remove` | 校验后写入或删除 Model List family 扩展 |

Model/Provider 写入会先合并完整 Model List 并解析整个 Host 配置，成功后原子替换
`profile.yaml`，再让 SessionService 和 Channel runtime 一次性应用。Automation 的 Project 配置
由 `automation.*` 方法单独写入对应 Project。`maxModelSteps` 只存在于 Host，
每个 turn 启动时读取，不进入 Session metadata。

## Prompt、Rule 与 Skill

| 方法 | 用途 |
| --- | --- |
| `prompt.list/get/set` | 管理 `resource/prompts/*.md`，默认 `System.md` |
| `rule.list/get/set` | 管理 `resource/prompts/*.md`，默认 `AGENTS.md` |
| `skill.list` | 返回有效 Skill snapshot 和 disabled 名称 |
| `skill.install` | 以 `{name, content}` 安装单个 `SKILL.md`，或以 `{name, files:[{path, contentBase64}]}` 导入完整目录 |
| `skill.enable` / `skill.disable` | 在 active/disabled 目录间切换，下一次 prompt 构建立即生效 |
| `skill.uninstall` | 删除 active 或 disabled Skill |

Skill 名称只能是单个路径组件。安装失败会清除未通过 frontmatter/UTF-8 扫描的目录。

## MCP

| 方法 | 用途 |
| --- | --- |
| `mcp.list` | 当前 runtime catalog、server 状态和 tool schema |
| `mcp.config` | 脱敏的 server 配置和 enable/auth/credential 状态 |
| `mcp.get` | 单个 server 的脱敏配置与当前 runtime catalog entry |
| `mcp.install` | 安装单个 `{server, config}` 或批量 `{servers}` / `{config:{mcpServers}}` |
| `mcp.enable` / `mcp.disable` | 启停指定 server 并同步 runtime |
| `mcp.uninstall` | 删除 server 并关闭不再使用的连接 |
| `mcp.auth.login` | OAuth 登录 |
| `mcp.auth.logout` / `mcp.auth.unauth` | 清除 OAuth 凭据 |
| `mcp.search` / `mcp.call` | 搜索和调用当前有效 tool |

安装先对合并后的完整 `mcp.json` 做解析和环境展开验证，验证成功后才原子写入。任何
`env`、header、token 或 OAuth secret 都不会通过 `mcp.config` 回显。

## Automation

| 方法 | 用途 |
| --- | --- |
| `automation.list/status` | Job、下一次执行、active run、recent run |
| `automation.add/update/delete` | 新建、完整替换或局部修改、删除 |
| `automation.enable/disable` | 启停单个或全部 Job |
| `automation.run` | 立即排队执行并返回 `runId` |
| `automation.history` | 按 Job 和 limit 返回轻量 run history |

所有 Automation 方法都要求 `project_id`，配置和 history 位于
`runtime/projects/<project-id>/automation/`；Topic 详情通过 Job 的 `topicId` 筛选任务。

Job 支持 cron/timezone，`every_time`、`once`、`fixed` 三种 Session 策略，以及 prompt、
model、reasoning 和 policy。History 最多保留 100 条，只持久化回答摘要、时间、状态、
`finishReason`、Session/turn id 和错误。

## Channel

每个 `<kind>` 可为 `weixin`、`telegram`、`feishu` 或 `qq`。

| 方法 | 用途 |
| --- | --- |
| `channel.list` / `channel.<kind>.status` | enable、运行、连接、绑定用户和 Session 状态 |
| `channel.<kind>.config` | 读取或整体替换 adapter typed config |
| `channel.<kind>.enable/disable` | 更新 Profile 并由 Host 重启相应 runtime |
| `channel.<kind>.bind/begin/poll` | 启动和轮询平台绑定 |
| `channel.<kind>.unbind/remove` | 停止 runtime 并清理绑定 secret/runtime state |
| `channel.<kind>.send_message/send_file` | 向已绑定目标发送内容 |

Channel secret 保存在私有 runtime 文件中；配置 API 只操作 Host 配置字段，不接受 secret
回读。

## WebSocket Transport

WebSocket 是远程 transport，不是 Channel，不提供绑定用户、发送消息或 Session 选择能力。

| 方法 | 用途 |
| --- | --- |
| `websocket.status` | enabled、running、bind、port 和公开路径 |
| `websocket.enable/disable` | 启停 listener |
| `websocket.config` | 读取或替换 `{enabled, bind, port}` |
| `websocket.token/reset_token` | 获取或轮换独立 ACP/Management token |

ACP token 不能访问 `/dwo`，Management token 不能访问 `/acp`。token 保存在
`runtime/websocket/secret.yaml`，轮换后 listener 会重启并关闭旧连接。

## Session 与断线恢复

会话创建、prompt、cancel、permission、model/reasoning/policy 选择等聊天交互使用 ACP v2。
Dwo RPC 保留 Session 查询和管理方法。ACP 的 `session.watch` 首先返回一致的 snapshot，
随后推送 `session.event`；事件包含单调递增的 `seq`。管理事件使用 `event.read` 和独立
cursor。客户端连接关闭不会 cancel 已接受 turn、Automation run 或 Host。

`session.list` 返回轻量运行状态、模型、思维链模式和 policy；`session.status` 按需返回
`lastTurnStatus` 与 `lastTurnFinishedAtMs`。Desktop Inbox 直接按这些 Session 事实筛选和
排序，不需要 Inbox repository 或单独的后端 API。
