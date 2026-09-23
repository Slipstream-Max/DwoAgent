# API 说明

Dwo 对外有两套协议：

| 协议 | 用途 | 入口 |
| --- | --- | --- |
| ACP v1/v2 | 创建 Session、提交 prompt、接收 Agent 事件 | dwo acp 或 WebSocket /acp |
| Management RPC v3 | 管理配置、Project、MCP、Channel、Automation 和事件 | 本地 IPC 或 WebSocket /dwo |

不要用 Management RPC 发送聊天 prompt；聊天交互统一走 ACP。Rust 类型见
[dwo-protocol](../crates/dwo-protocol/README.md)。

## Envelope

Management RPC 使用 JSON-RPC 2.0 envelope。IPC 请求必须带 route: dwo；WebSocket /dwo 发送
相同 JSON：

~~~json
{
  "jsonrpc": "2.0",
  "id": "request-1",
  "route": "dwo",
  "method": "dwo.capabilities",
  "params": {}
}
~~~

客户端启动后先调用 dwo.capabilities，读取当前 Host 支持的方法、query/command 类型、副作用、
事件和协议能力。未知 method、参数错误和业务校验失败返回结构化 error。

## Management RPC

### Daemon、配置和模型

~~~text
daemon.status
daemon.shutdown
config.snapshot
config.update
model.list / model.available / model.get_default / model.set_default
provider.list / provider.upsert / provider.remove
model.upsert / model.remove
model.catalog.list / upsert / remove
~~~

### Project

~~~text
project.list / project.get / project.create / project.update / project.delete
project.rules.get / project.rules.set
project.section.list / project.section.create / project.section.update
project.section.delete / project.section.reorder
project.session.assign / project.session.unassign
project.worktree.list / project.worktree.get / project.worktree.create
project.worktree.attach / project.worktree.update / project.worktree.detach
project.worktree.remove
~~~

Project 只记录 pwd、Section、Session assignment、Repository 和 Worktree。Project rules 读写
项目根目录的 AGENTS.md。Section 删除需要没有 Session assignment 或 Automation Job 引用。
Worktree detach 只解除登记，remove 调用 Git 清理由 Host 管理的 Worktree。

### Session

~~~text
session.list / session.status / session.snapshot / session.read
session.new / session.fork / session.set / session.keep
session.archive / session.delete
~~~

Session 文件不保存 Project 属性。archive 将 Session 移入 archive 目录；delete 时 Host 清理 assignment；
delete 只接受 archived Session，才会真正删除 Session 文件和 managed workspace。prompt、cancel、
permission 和 watch 事件属于 ACP。

### Automation

~~~text
automation.list / automation.status / automation.history
automation.add / automation.update / automation.delete
automation.enable / automation.disable / automation.run
~~~

Automation 的 project_id 是可选的：传 Project 时读写
runtime/automations/<project-id>/，省略并设置 global 时读写 runtime/automations/global/。
Project scope 可以由 caller_session_id 推导；Global Job 可以带 cwd。字段和 Session 行为见
[automation.md](automation.md)。

### Prompt、Rule、Skill、MCP、Channel 和 WebSocket

~~~text
prompt.list / prompt.get / prompt.set
rule.list / rule.get / rule.set
skill.list / skill.install / skill.enable / skill.disable / skill.uninstall
mcp.list / mcp.get / mcp.config / mcp.install / mcp.enable / mcp.disable / mcp.uninstall
mcp.auth.login / mcp.auth.logout / mcp.search / mcp.call
channel.list / channel.<name>.status / channel.<name>.config
channel.<name>.enable / disable / bind / begin / poll / unbind / remove
channel.<name>.send_message / channel.<name>.send_file
websocket.status / websocket.config / websocket.enable / websocket.disable
websocket.token / websocket.reset_token
~~~

## 事件和断线恢复

~~~text
event.read
event.subscribe
~~~

常见事件有 config.changed、config.apply_failed、mcp.status、skill.changed、channel.status、
project.changed、automation.changed 和 automation.run。Session 事件由 ACP 的 session/update 或
state_update 传递。重连后先调用 dwo.capabilities，再用 event.read 的 cursor 补齐管理事件；
Session 则重新 ACP load/resume 并按需回放 transcript。

## Transport 和安全

本地 IPC 不需要 WebSocket token；远程 /dwo 使用 Management token，/acp 使用另一枚 ACP token。
Token 只在 dwo websocket token 输出和 runtime/websocket/secret.yaml 中出现，不会通过 config API
返回。
