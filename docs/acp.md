# ACP 使用指南

ACP（Agent Client Protocol）让 IDE 或其他客户端通过标准协议驱动赤铎。`dwo acp` 是一个 stdio adapter：客户端启动它，它再通过本地 IPC 连接已经运行的 `dwo` daemon。

因此 ACP、CLI 和消息 channel 看到的是同一批 session、同一份上下文与同一个 tool runtime。关闭 IDE 不会删除 session，也不会关闭 daemon。

## 前置条件

```text
dwo daemon start
dwo daemon status
```

`daemon status` 必须返回健康状态。ACP 进程本身不负责加载 profile、连接模型或初始化 MCP server；这些工作都由 daemon 完成。

## 客户端配置

支持自定义 ACP agent 的客户端需要以下核心配置：

```json
{
  "command": "dwo",
  "args": ["acp"]
}
```

如果 `dwo` 不在客户端进程的 PATH 中，使用安装后的绝对路径：

```text
Windows: C:\Users\<user>\.dwoagent\bin\dwo.exe
macOS/Linux: /home/<user>/.dwoagent/bin/dwo
```

指定另一个 profile 时，全局参数要放在 `acp` 之前：

```json
{
  "command": "dwo",
  "args": ["--config-path", "/path/to/profile.yaml", "acp"]
}
```

不同客户端的配置文件名和 UI 不同，但最终都应启动同一个命令。客户端关闭 stdin 后，`dwo acp` 会正常退出；daemon 继续运行。

## WebSocket 客户端

网页可以通过 WebSocket 使用完全相同的 ACP 协议：

```yaml
channels:
  websocket:
    enabled: true
    port: 8765
```

服务地址固定为 `ws://<host>:8765/acp?token=<token>`。运行 `dwo channel websocket token` 查看 token。每条 ACP JSON-RPC 消息使用一个 WebSocket text frame，方法、通知、能力和 stdio ACP 相同。

服务监听所有网卡。局域网连接需要放行防火墙端口；公网必须通过 TLS 反向代理使用 `wss://`。

## Session 工作方式

- 新建 ACP session 时，客户端提供的 `cwd` 会成为 session 工作目录。
- ACP 可以列出、加载和继续 daemon 中已有的 session。
- 加载 session 后，ACP 会保持 observer 连接，接收其他入口提交的 prompt、tool event 和权限请求。
- session 的 model、reasoning 和 policy 作为 ACP config options 暴露，修改后写回同一个持久化 session。
- 取消操作会直接调用 daemon 的 `session.cancel`，同时停止实际运行的 turn。

ACP client 传入的 `mcpServers` 不会创建客户端专属 MCP runtime。赤铎的 MCP server 统一在 `~/.dwoagent/resource/mcp.json` 配置，由 daemon 托管。

## 输入与输出能力

当前 ACP adapter 支持：

- 普通文本 prompt。
- 文本型 embedded resource。
- resource link，包括名称、URI、MIME type 和可用元数据。
- session list/load、usage update、tool call/result、reasoning、权限请求和取消。

当前不支持：

- ACP image input。
- ACP audio input。
- 二进制 embedded resource。

客户端粘贴的文本文件可以通过 `embeddedContext` 进入 prompt。引用文件或目录时，resource link 会保留明确路径和元数据，方便模型定位工作区内容。

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
4. 自定义 profile 时，确认客户端和 daemon 使用同一个 `--config-path`。
5. 查看 `~/.dwoagent/runtime/logs/` 中的 daemon 日志。
