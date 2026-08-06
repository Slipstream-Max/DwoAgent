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

如果 `dwo` 不在客户端进程的 PATH 中，使用安装后的绝对路径：

```text
Windows: C:\Users\<user>\.dwoagent\bin\dwo.exe
macOS/Linux: /home/<user>/.dwoagent/bin/dwo
```

各家客户端的配置入口不同，但最终启动的都是这条命令。客户端关闭 stdin 后，adapter 会跟着退出；daemon 继续在后台工作。

## WebSocket 客户端

网页可以通过 WebSocket 使用完全相同的 ACP 协议：

```yaml
channels:
  websocket:
    enabled: true
    port: 8765
```

服务地址固定为 `ws://<host>:8765/acp?token=<token>`。运行 `dwo channel websocket token` 查看 token。每条 ACP JSON-RPC 消息使用一个 WebSocket text frame，方法、通知、能力和 stdio ACP 相同。WebSocket adapter 当前固定使用 ACP v2。

服务监听所有网卡。局域网连接需要放行防火墙端口；公网必须通过 TLS 反向代理使用 `wss://`。

## Session 工作方式

- 新建 ACP session 时，客户端提供的 `cwd` 会成为 session 工作目录。
- ACP 可以新建、列出、加载、继续、关闭和删除 daemon 中的 session。
- 加载 session 后，ACP 会保持 observer 连接，接收其他入口提交的 prompt、tool event 和权限请求。
- session 的 model、reasoning 和 policy 会作为 config options 显示；在客户端修改后，设置会写回持久化 session。

ACP 暂不接受客户端传入的非空 `mcpServers`，也不会为某个编辑器另起一套 MCP runtime；这类 session 请求会直接返回参数错误。MCP server 统一写在 `~/.dwoagent/resource/mcp.json`，由 daemon 托管和复用。

### Zed 的 Send now

Zed 的 Send now 会先发送 `session/cancel`，紧接着再发送 `session/prompt`。如果照字面执行，刚提交的新消息会先把当前 turn 取消掉，这和赤铎原有的排队语义并不一致。

adapter 会给 cancel 留出 `150ms` 的配对窗口：同一连接、同一 session 在窗口内紧跟 prompt，就把这两条消息识别为 Send now，消费 cancel，并将新 prompt 加入当前 turn 的 FIFO 队列。若没有等到 prompt，则把它当作真正的 Stop 转发给 daemon。换句话说，Send now 继续对话，单独 Stop 仍然停止，只是最多晚 `150ms` 生效。

## Slash Commands

创建或恢复 session 后，Agent 通过 ACP v2 `available_commands_update` 宣告以下命令：

| 命令 | 行为 |
| --- | --- |
| `/compact` | 手动压缩当前 session context；命令文本不进入模型上下文，并通过下述 compaction update 回显进度与摘要 |
| `/resume` | session idle 时加入内部继续指令并启动新 turn；运行中静默忽略，不排队也不报错 |
| `/fork` | 复制当前 session 并显示副本 ID；当前 ACP session 不变 |

Slash command 仍通过普通 `session/prompt` 发送，由 Agent 识别并执行。ACP 同时声明并实现实验性原生 `session/fork`；它和 `/fork` 都返回副本 ID，但都不会切换当前 ACP session。ACP 协议自身的 `session/resume` 是重新接入已有 session、恢复 observer 和可选回放历史，不会启动模型；它与自定义 `/resume` 命令不是同一功能。

## 上下文压缩回显

手动 `/compact`、达到 token 阈值后的自动压缩、上下文超限恢复，以及 Agent 调用 `handoff` 触发的上下文重建，都会在 ACP 时间线中产生同一个 ID 寻址的压缩实体：

1. 开始时发送 `compaction_update`，状态为 `in_progress`。
2. 成功时复用同一个 `compactionId` 发送 `completed`，并在存在安全、用户可见的保留摘要时附带 `summary`。
3. 失败或取消时复用同一个 ID 发送 `failed` 或 `cancelled`；失败状态包含可读的 `error`。

这些事件会写入 session transcript，因此 `session/load` 或带 replay 的 `session/resume` 可以在原来的时间线位置重建压缩边界。`usage_update` 仍独立报告压缩后的 context window 使用量。

ACP v2 始终发送这些标准 update。ACP v1 仅在客户端初始化时声明 `clientCapabilities.session.compaction: {}` 后发送；未声明该能力的 v1 客户端继续接收原有的普通文本结果。

## 输入与输出能力

当前 ACP adapter 支持：

- 普通文本 prompt。
- 图片输入；最终是否接受取决于当前模型是否支持图片。
- 文本型 embedded resource。
- resource link，包括名称、URI、MIME type 和可用元数据。
- reasoning、assistant message、usage、compaction lifecycle 和 session state 更新。
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
2. `dwo profile-list`：确认模型和默认配置可解析。
3. 检查客户端启动的命令是否为安装后的 `dwo ... acp`。
4. 查看 `~/.dwoagent/runtime/logs/` 中的 daemon 日志。
