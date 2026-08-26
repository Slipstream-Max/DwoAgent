# ACP 使用指南

ACP（Agent Client Protocol）是编辑器与赤铎之间的连接。你可以把 `dwo acp` 看成一座很薄的桥：客户端通过 stdio JSON-RPC 和它说话，它再通过本地 IPC 找到已经运行的 `dwo` daemon。

这座桥自己不维护第二套 Agent。session、模型、上下文和工具都由 daemon 统一管理，因此 ACP、CLI 和消息 channel 看到的是同一批会话。关掉 IDE 只会结束这条连接，不会带走 session，也不会停掉 daemon。实现上 adapter 只依赖 ACP schema，并仍随主程序一起安装，不需要额外部署一个可执行文件。

## 前置条件

```text
dwo daemon start
dwo daemon status
```

确认 `daemon status` 返回 `healthy: true`。ACP 进程不会自己加载 profile、连接模型或启动 MCP server；若 daemon 没有起来，编辑器即使成功启动 `dwo acp` 也无法工作。

## 客户端配置

支持自定义 ACP agent 的客户端需要以下核心配置：

```json
{
  "command": "dwo",
  "args": ["acp", "--protocol", "v2"]
}
```

`v2` 是默认值，因此 `dwo acp` 与 `dwo acp --protocol v2` 等价。只有仍使用第一代协议的客户端才需要：

```text
dwo acp --protocol v1
```

两个版本共享同一个 Host，只是客户端等待结果的方式不同：

| 协议 | `session/prompt` 的行为 |
| --- | --- |
| v1 | 请求保持打开，turn 结束后直接返回 `stopReason`。 |
| v2 | prompt 被接受后立即响应，运行状态和结束原因随后通过 `state_update` 通知送达。 |

切换协议不会复制或迁移 session，也不会改变模型上下文。

### Zed `Send now` 与取消

ACP v2 允许运行中的 session 立即接受新 prompt；Host 会把它加入当前 turn 的 FIFO，并在当前模型响应或工具批次结束后、下一次 agent step 开始前写入上下文。`session/cancel` 在 v2 中仍按规范立即取消当前工作。

Zed 目前通过 ACP v1 调用 `Send now` 时会先发送 `session/cancel`，收到旧 `session/prompt` 的 `cancelled` 响应后再发送替代 prompt。v1 适配器对此提供 500ms 兼容窗口：它先用 `cancelled` 完成旧 prompt，让 Zed 可以继续发送；若窗口内收到且 Host 成功接受替代 prompt，则撤销真正取消并让该 prompt 进入下一 agent step；若没有替代 prompt，或替代 prompt 校验、提交失败，则向 Host 执行真正取消。

这个 v1 行为是针对旧客户端时序的兼容扩展。它保留 v1 的响应形状，但在替代 prompt 成功入队时不会实际中止底层 turn。

v1 适配器不会通过 `session/update` 回显当前 `session/prompt`，因为 Zed 已经持有并显示了这条输入；`load`/`resume` 回放 transcript 时仍会发送历史用户消息，来自其他 endpoint 的新用户消息也会正常转发。

如果 `dwo` 不在客户端进程的 PATH 中，使用安装后的绝对路径：

```text
Windows: C:\Users\<user>\.dwoagent\bin\dwo.exe
macOS/Linux: /home/<user>/.dwoagent/bin/dwo
```

各家客户端的配置入口不同，但最终启动的都是这条命令。客户端关闭 stdin 后，adapter 会跟着退出；daemon 继续在后台工作。

## WebSocket 客户端

网页可以通过 WebSocket 使用完全相同的 ACP 协议：

```yaml
websocket:
  enabled: true
  bind: 127.0.0.1
  port: 8787
```

服务地址为 `ws://<bind>:8787/acp?token=<acp-token>`。运行 `dwo websocket token` 查看 token。每条 ACP JSON-RPC 消息使用一个 WebSocket text frame，方法、通知、能力和 stdio ACP 相同。WebSocket adapter 固定使用 ACP v2。

默认只监听 `127.0.0.1`。局域网使用时显式修改 `bind` 并放行防火墙端口；公网必须通过 TLS 反向代理使用 `wss://`。

## Session 工作方式

- 新建 ACP session 时，客户端提供的 `cwd` 会成为 session 工作目录。
- ACP 可以新建、列出、加载、继续、关闭和删除 daemon 中的 session。
- 加载 session 后，ACP 会保持 observer 连接，接收其他入口提交的 prompt、tool event 和权限请求。
- session 的 model、reasoning 和 policy 会作为 config options 显示；在客户端修改后，设置会写回持久化 session。

ACP 暂不接受客户端传入的非空 `mcpServers`，也不会为某个编辑器另起一套 MCP runtime；这类 session 请求会直接返回参数错误。MCP server 统一写在 `~/.dwoagent/resource/mcp/mcp.json`，OAuth 凭据保存在同目录的 `oauth/`，由 daemon 托管和复用。

### Zed 的 Send now

Zed 的 Send now 会先发送 `session/cancel`，紧接着再发送 `session/prompt`。如果照字面执行，刚提交的新消息会先把当前 turn 取消掉，这和赤铎原有的排队语义并不一致。

adapter 会给 cancel 留出 `500ms` 的配对窗口：同一连接、同一 session 在窗口内紧跟 prompt，就把这两条消息识别为 Send now，消费 cancel，并将新 prompt 加入当前 turn 的 FIFO 队列。若没有等到 prompt，则把它当作真正的 Stop 转发给 daemon。换句话说，Send now 继续对话，单独 Stop 仍然停止，只是最多晚 `500ms` 生效。

## Slash Commands

创建或恢复 session 后，Agent 通过 ACP v1/v2 `available_commands_update` 宣告 `/compact`、`/resume`、`/fork`、`/status`、`/plan`，以及按当前 catalog 动态生成的 `skill <name>`、`mcp <name>`。命令说明见 [Slash Commands 使用指南](slash-commands.md)，这里只保留 ACP 特有行为。

ACP 的 command input 目前只有自由文本 hint，没有参数候选列表。为支持名称补全，adapter 会把每个可用项发布为完整命令名，例如 `skill <skill-name>`、`mcp <server-name>`，并把后续输入声明为文本。因此在支持继续显示 slash command popup 的客户端中，键入 `/skill ` 或 `/mcp ` 后可以选择名称并回车补全；当前 Zed 的 slash completion 会把选中项插入为 `/skill <skill-name> ` 或 `/mcp <server-name> `。名称含空白的 skill/MCP 不会发布为候选，因为 directive 的 `NAME` 是单个 token。

这两个 directive 也可以放在正文中并重复或混合使用。daemon 仅替换与当前有效 skill/MCP catalog 精确匹配的名称；未知名称和只有 `/skill`、`/mcp` 的文本保持原样并作为普通 prompt 发送。具体 XML 提示和各消息 channel 的一致行为见 [Slash Commands 使用指南](slash-commands.md)。

Slash command 仍通过普通 `session/prompt` 发送，由 Agent 识别并执行。ACP 同时声明并实现实验性原生 `session/fork`；它和 `/fork` 都返回副本 ID，但都不会切换当前 ACP session。ACP 协议自身的 `session/resume` 是重新接入已有 session、恢复 observer 和可选回放历史，不会启动模型；它与自定义 `/resume` 命令不是同一功能。

## 系统通知

压缩、模型重试、`/fork` 等运行时状态统一映射为 `agent_message`，并在
`_meta.dwo` 中携带机器可读信息：`kind: "system_notification"`、`category`、
`level` 和 `data`。这样 v1/v2 客户端即使没有专用 update schema 也能显示文本，
支持 metadata 的客户端则可以渲染独立通知样式。通知写入 session transcript，
因此 `session/load` 或带 replay 的 `session/resume` 会在原时间线位置恢复。

压缩通知使用 `compaction_started`、`compaction_completed`、
`compaction_failed`、`compaction_cancelled` category，并在 `data` 中携带同一个
`compactionId`、trigger、summary 或 error。`usage_update` 仍独立报告压缩后的
context window 使用量。旧 `compaction_update` 不再发送，也不再依赖客户端声明
compaction capability。

模型流中断时，已显示的 reasoning/assistant partial 会作为带
`interrupted_attempt` metadata 的原消息保留；随后发送 `model_retrying` 系统通知。
重试期间 `state_update` 仍为 Running，turn ID 和 ACP prompt 不变。最多五次重试
耗尽后才发送最终 Failed/stop reason。其他消息 channel 不展示中间 retry 通知，
只在最终失败时报告原因。

## 执行计划

内置 `plan` 工具只支持读取或完整替换当前 session 的执行清单，不负责调度 turn。
ACP adapter 会隐藏 `plan` 的普通 tool-call lifecycle，只发布计划状态：v2 使用标准
`plan_update`；v1 使用兼容的 `sessionUpdate: "plan"`。v1 没有 `cancelled` 状态，
因此取消项以 `completed` 发送，并在 `_meta._dwo_original_status` 保留原状态。
计划清除时两个协议都发布空 entries；snapshot 没有当前计划时也发送空计划，确保持续
连接和重新接入的客户端最终显示一致。

agent turn 结束时，如果仍有未完成计划，daemon 会向模型上下文追加一条简短的
`PlanNotice`，提示模型通过 `plan(get)` 获取详情；完整 entries 只保存在
`SessionRecord.current_plan`。session 保持 Idle；只有后续用户 prompt 或显式
`/resume` 才会启动新 turn。Cancel、Failed、daemon shutdown 和重启都不会因为存在
计划而自动调用模型。compaction 将 notice 当作普通内部消息处理，不维护可替换的计划
快照。恢复 ACP session 时只从 snapshot 展示当前计划和真实 phase。

## 输入与输出能力

当前 ACP adapter 支持：

- 普通文本 prompt。
- 图片输入；最终是否接受取决于当前模型是否支持图片。
- 文本型 embedded resource。
- resource link，包括名称、URI、MIME type 和可用元数据。
- reasoning、assistant message、usage、系统通知和 session state 更新。
- 本地与 provider 托管的 tool call/result、权限请求和取消。

当前不支持：

- ACP audio input。
- 二进制 embedded resource。
- session 级 `mcpServers` 和 `additionalDirectories`。

客户端粘贴的文本文件可以通过 `embeddedContext` 进入 prompt。引用文件或目录时，resource link 会保留明确路径和元数据，方便模型定位工作区内容。图片会保持为结构化 image block，不会被拼成一段文本。

OpenAI、DeepSeek 或兼容网关提供的 Web Search 属于 provider 托管工具：搜索在远端完成，不经过本地 ToolManager，也不会触发本地工具权限确认。adapter 仍会把它映射为普通工具事件，因此客户端可以看到并回放这次调用；provider 返回了结果内容时，工具结果中也会一并显示。

## 权限

Session policy 有三种：

| Policy | 行为 |
| --- | --- |
| `full_access` | 除显式 deny rule 外，终端和文件编辑直接执行 |
| `confirm` | 简单只读命令直接执行，其他终端命令和文件编辑请求客户端批准 |
| `watch` | 只允许简单只读命令和 watch allow rule，拒绝文件编辑及其他命令 |

客户端收到 permission request 后应返回 allow/deny。若客户端不实现权限 UI，建议先在 profile 中选用与客户端能力匹配的 policy，不要让请求长期悬空。

## 与其他入口协作

同一 session 可以同时被 ACP 和 channel 观察。例如，在 IDE 中创建 session 后，可以在 Telegram 中执行 `/list`，再用 `/use <session-id>` 切换过去继续对话。

外部 prompt 使用 FIFO 语义：当前 turn 运行时，新 prompt 会在模型响应或 tool-call batch 的边界加入，不会隐式取消正在执行的工具。需要中断时使用 ACP cancel、channel `/cancel` 或 CLI `dwo session cancel <session-id>`。

## 排查

1. `dwo daemon status`：确认 daemon 可连接。
2. `dwo config-show`：确认模型和默认配置可解析。
3. 检查客户端启动的命令是否为安装后的 `dwo ... acp`。
4. 查看 `~/.dwoagent/logs/` 中的 daemon 日志。
