# 赤铎文档

每个主题只有一个主文档。根目录 [README](../README.md) 只负责安装和最小配置；详细内容按下面
的职责拆分。

| 文档 | 内容 |
| --- | --- |
| [Profile 配置](profile.md) | profile.yaml 全部字段、默认值、Provider、Channel 字段和资源目录 |
| [模型与 Provider](models.md) | Model List、新增模型、模型能力和支持范围 |
| [Prompt、Skill 与 MCP](resources.md) | Prompt/Rule、Skill 目录、MCP JSON、优先级和资源热加载 |
| [Channel 配置与行为](channels.md) | 微信、Telegram、飞书/Lark、QQ 的配置、绑定和消息行为 |
| [CLI 命令参考](cli.md) | dwo 的全部命令，按资源分类 |
| [Project 文件与行为](projects.md) | project.json、Board/Section/Topic/Label/Worktree 字段和 Workspace 规则 |
| [Automation](automation.md) | config.yaml、history.yaml、Cron、Session 策略和无人值守行为 |
| [Slash Commands](slash-commands.md) | ACP 和消息平台中的 / 命令 |
| [ACP 连接](acp.md) | ACP Client 配置、v1/v2 和输入输出能力 |
| [WebSocket 连接](websocket.md) | /acp、/dwo、Token、TLS 和远程连接 |
| [Session 与子 Agent](session.md) | 创建、继续、Fork、临时子 Agent、队列和持久化 |
| [Agent 工具](tools.md) | terminal、read_file、file_edit、plan、handoff 和权限 |
| [API 说明](api.md) | ACP 与 Management RPC 的边界、方法和事件 |

源码级 Rust API 另见 [dwo-protocol](../crates/dwo-protocol/README.md)、
[dwo-host](../crates/dwo-host/README.md) 和
[dwo-agent-service](../crates/dwo-agent-service/README.md)。
