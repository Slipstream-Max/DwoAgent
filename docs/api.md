# API 说明

Dwo 对外有两套协议：

| 协议 | 用途 | 入口 |
| --- | --- | --- |
| ACP v1/v2 | 创建 Session、提交 prompt、接收 Agent 事件 | dwo acp 或 WebSocket /acp |
| Management RPC v3 | 管理配置、Project、MCP、Channel、Automation 和事件 | 本地 IPC 或 WebSocket /dwo |

不要用 Management RPC 发送聊天 prompt；聊天交互统一走 ACP。Rust 类型的细节见
[dwo-protocol API](../crates/dwo-protocol/README.md)，Host 嵌入接口见
[dwo-host API](../crates/dwo-host/README.md)。

## Envelope

Management RPC 使用 JSON-RPC 2.0 envelope。IPC 请求必须带 route: dwo；WebSocket /dwo 发送
相同 JSON。客户端启动后先调用 dwo.capabilities，不要把静态方法列表写死。

~~~json
{
  "jsonrpc": "2.0",
  "id": "request-1",
  "route": "dwo",
  "method": "dwo.capabilities",
  "params": {}
}
~~~

成功响应包含同一个 id 和 result；失败响应包含 error.code、error.message 和可选 error.data。

capabilities 返回：

- 协议版本、request id 和 event cursor 能力；
- 当前 Host 支持的方法；
- 每个方法的 query、command 或 subscription 类型；
- 是否有副作用及可能发布的事件；
- 当前支持的事件名。

未知 method、参数错误和业务校验失败都会返回结构化 error；敏感字段不会回显。

## Management RPC 方法

### Daemon

| 方法 | 作用 |
| --- | --- |
| daemon.status | Host 健康状态和资源数量 |
| daemon.shutdown | 请求 daemon 正常退出 |

### 配置和模型

| 方法 | 作用 |
| --- | --- |
| config.snapshot | 读取 Host 配置摘要和默认模型 |
| config.update | 更新日志、外部 skills/rules、maxModelSteps |
| model.list / model.available | 查看模型和能力 |
| model.get_default / model.set_default | 读取或修改默认模型 |
| provider.list / provider.upsert / provider.remove | 管理 Provider |
| model.upsert / model.remove | 管理 Provider 下的部署模型 |
| model.catalog.list / upsert / remove | 管理 Model List |

配置写入会先解析完整结果，成功后原子替换 profile.yaml；不会留下半份配置。

### Project 和 Session

| 方法 | 作用 |
| --- | --- |
| project.list / get / create / update | 管理 Project |
| project.board | 读取 Board 快照 |
| project.repository.get / clone / attach | 管理 Git Repository |
| project.worktree.list / get / create / attach / update / detach / remove | 管理 Worktree |
| project.section.* | Section 增删改和排序 |
| project.topic.* | Topic 增删改、移动、排序和详情 |
| project.topic.overview.* | 读写 overview.md |
| project.topic.agents.* | 读写 Topic Knowledge |
| project.topic.session.assign / unassign | 归类或移出 Session |
| project.label.* | 管理和分配标签 |
| session.list / status / snapshot / read | 查询 Session |
| session.new / fork / delete / keep / close / set | 管理 Session 生命周期和配置 |

Project、Topic、Workspace 和跨 Project 移动规则见 [projects.md](projects.md)。Session 的 prompt、
cancel、permission、watch 属于 ACP，不属于 Management RPC。

### Prompt、Skill 和 MCP

| 方法 | 作用 |
| --- | --- |
| prompt.list / get / set | 管理 profile prompts |
| rule.list / get / set | 管理 profile rules |
| skill.list / install / enable / disable / uninstall | 管理 skills |
| mcp.list / get / config | 查看 MCP runtime 和脱敏配置 |
| mcp.install / enable / disable / uninstall | 管理 MCP server |
| mcp.auth.login / logout | 管理 OAuth |
| mcp.search / call | 搜索和调用已发现工具 |

对应的文件格式、加载顺序和凭据边界见 [Prompt、Skill 与 MCP](resources.md)。

### Channel、WebSocket 和 Automation

| 方法 | 作用 |
| --- | --- |
| channel.list / channel.<name>.status | 查看 adapter 状态 |
| channel.<name>.config | 读取或替换 channel 配置 |
| channel.<name>.enable / disable | 启停 channel |
| channel.<name>.bind / begin / poll | 执行绑定流程 |
| channel.<name>.unbind / remove | 清理绑定状态 |
| channel.<name>.send_message / send_file | 主动发送 |
| websocket.status / config / enable / disable | 管理 listener |
| websocket.token / reset_token | 读取或轮换 token |
| automation.list / status / history | 查询 Project 任务 |
| automation.add / update / delete | 修改任务 |
| automation.enable / disable / run | 启停或立即排队任务 |

所有 automation 方法都必须提供 project_id。字段和行为见 [automation.md](automation.md)；
Channel 配置和绑定见 [channels.md](channels.md)。

## 事件和断线恢复

管理事件通过两种方式读取：

~~~text
event.read       # 按 cursor 分页读取
event.subscribe  # replay 后继续接收 live event
~~~

不传 event 时接收全部管理事件；传入事件名时 replay 和 live 都过滤。常见事件有
config.changed、config.apply_failed、mcp.status、skill.changed、channel.status、
project.changed、automation.changed 和 automation.run。

事件 cursor 只属于管理事件流；Session 事件由 ACP 的 session/update 或 state_update 传递。
客户端断开不会停止 Host、已接受的 Session turn 或 Automation run。重连后先调用
dwo.capabilities，再用 event.read 的 cursor 补齐管理事件；Session 则重新 ACP load/resume
并按需回放 transcript。

## Transport 和安全

本地 IPC 不需要 WebSocket token；远程 /dwo 使用 Management token，/acp 使用另一枚 ACP token。
Token 只在 dwo websocket token 输出和 runtime/websocket/secret.yaml 中出现，不会通过 config API
返回。
