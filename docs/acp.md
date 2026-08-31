# ACP 连接

ACP（Agent Client Protocol）用于把 IDE 或其他 Agent Client 接到已运行的 Dwo daemon。
dwo acp 是 stdio 与本地 IPC 之间的 Adapter，不会启动第二套 Agent Runtime。

## 前置条件

~~~text
dwo daemon start
dwo daemon status
~~~

status 应显示 healthy: true。模型、MCP、Session 和工具都由 daemon 管理；关闭 IDE 只会断开
当前 ACP 连接，不会停止 Session。

## Client 配置

支持自定义 ACP Agent 的客户端使用：

~~~json
{
  "command": "dwo",
  "args": ["acp", "--protocol", "v2"]
}
~~~

v2 是默认值，args 也可以只写 acp。旧 ACP Client 使用：

~~~json
{
  "command": "dwo",
  "args": ["acp", "--protocol", "v1"]
}
~~~

如果客户端进程找不到 dwo，使用安装后的绝对路径：

~~~text
Windows: C:\Users\<user>\.dwoagent\bin\dwo.exe
macOS/Linux: /home/<user>/.dwoagent/bin/dwo
~~~

远程客户端不要启动 stdio Adapter，直接连接 [WebSocket /acp](websocket.md)。

## v1 和 v2

| 协议 | session/prompt 行为 |
| --- | --- |
| v1 | 请求保持打开，Turn 结束后返回 stopReason |
| v2 | Prompt 接受后立即响应，运行状态和结果通过 state_update 通知 |

两个版本连接同一个 Host，不复制 Session，也不改变上下文。

Zed 的旧 v1 Send now 会先发送 cancel 再发送替代 Prompt。Adapter 提供 500ms 配对窗口：同一
连接和 Session 在窗口内收到新 Prompt 时，把它作为排队消息，不真正取消底层 Turn；没有后续
Prompt 时，cancel 正常生效。这是唯一的 Zed 时序兼容行为。

## Session

ACP 可以新建、列出、加载、继续、关闭和删除 daemon 中的 Session。Client 创建 Session 时提供
的 cwd 会成为 External Workspace，并归入固定的“未分配会话” Project。完整的 Project/Topic
归类和 Workspace 类型见 [Project 文件与行为](projects.md)。

加载 Session 后，ACP 会持续接收其他入口提交的 Prompt、Tool Event、权限请求和状态变化。
Model、Reasoning 和 Policy 作为 Config Option 显示，客户端修改后会写回 Session。

ACP 不接受非空的 session mcpServers 或 additionalDirectories。MCP 统一由 daemon 托管，配置见
[Prompt、Skill 与 MCP](resources.md#mcp)。

## 输入和输出

支持：

- 文本 Prompt；
- 当前模型支持时的图片；
- 文本 Embedded Resource；
- Resource Link 及其名称、URI、MIME 和 Metadata；
- Assistant、Reasoning、Usage、Tool、Permission 和 Session State Event；
- 本地工具和 Provider Hosted Tool。

不支持 ACP Audio 和二进制 Embedded Resource。图片会保持结构化 Image Block，不会拼成文本。
Provider Hosted Tool 在远端执行，不经过本地工具权限确认，但仍作为 Tool Event 显示和回放。

## Slash Commands 和计划

ACP 会发布 /compact、/resume、/fork、/status、/plan，以及当前 Catalog 中的
/skill <name>、/mcp <name>。语法和消息平台差异见 [Slash Commands](slash-commands.md)。

plan 工具的普通 Tool Lifecycle 不直接显示；ACP v2 使用 plan_update，v1 使用兼容的 plan
Session Update。计划不会自动启动下一 Turn，只有新 Prompt 或 /resume 会继续。

ACP 协议自己的 session/resume 是重新连接已有 Session，不会调用模型；它和 /resume 不是一件事。

## 权限

full_access 直接执行允许的工具；confirm 对非只读操作请求 Client 批准；watch 拒绝写入和未明确
允许的命令。如果 ACP Client 没有 Permission UI，不要把 Session 长期留在等待确认状态。

工具的具体权限范围见 [Agent 工具](tools.md)。

## 排查

~~~text
dwo daemon status
dwo config-show
~~~

确认 Client 启动的是安装后的 dwo acp，Profile 可以正常解析，然后查看 ~/.dwoagent/logs/。
如果 stdio 正常但远程连接失败，改查 [WebSocket 连接](websocket.md)。
