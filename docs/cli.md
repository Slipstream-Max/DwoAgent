# CLI 命令参考

dwo 是 daemon 的命令行客户端。除 serve 外，命令通过本地 IPC 调用已经运行的 daemon。
默认 Profile 是 ~/.dwoagent/profile.yaml。

本文件列出全部 CLI 命令和参数。对象字段与行为分别见 [Profile](profile.md)、
[Project](projects.md)、[Session](session.md)、[Automation](automation.md) 和
[Channel](channels.md)。

## 安装和 daemon

~~~text
dwo install [--start]
dwo uninstall [--purge]
dwo serve
dwo daemon start
dwo daemon stop
dwo daemon status
dwo config-show
~~~

install 安装可执行文件、默认 Profile 和平台启动项；--start 会立即启动。uninstall 移除安装；
--purge 还会删除 ~/.dwoagent 下的配置和运行数据。serve 在前台运行 daemon，适合调试。
config-show 显示已解析的默认权限、模型、Reasoning、maxModelSteps 和可用模型。

## Session

~~~text
dwo session list [--all]
dwo session status <id> [--json]
dwo session prompt <message> [options]
dwo session set <id> [--title <title>] [--policy <policy>] [--model <model>] [--reasoning <mode>] [--worktree <id>]
dwo session move <id> --project <project-id> --topic <topic-id>
dwo session keep <id>
dwo session watch <id> [--cursor <cursor>] [--limit <count>]
dwo session cancel <id>
dwo session approve <id> <permission-id>
dwo session deny <id> <permission-id>
dwo session delete <id>
~~~

prompt 选项：

~~~text
--title <title>
--cwd <path>
--project <project-id> [--topic <topic-id>]
--policy <full_access|confirm|watch>
--model <provider/modelId>
--reasoning <mode>
--ephemeral
--to <session-id>
--from <session-id>
~~~

--cwd 和 --project 互斥；--topic 要求 --project。--to 继续直接子 Session，--from Fork 直接
子 Session 后发送新 Prompt，两者互斥。--ephemeral 只用于新 Session，不能和 --to/--from
组合。session set 的 --worktree 用于切换到当前 Project 已登记的 Worktree，不能与其他 set 字段
同时使用。keep 取消临时 Session 的自动删除。watch 是按 Cursor 分页读取，不是持续订阅。

外部终端创建根 Session；Agent Session 中默认创建直接子 Agent。完整行为见
[Session 与子 Agent](session.md)。

## Project

~~~text
dwo project list
dwo project get <project-id>
dwo project create <name> [--kind <shared|independent>] [--cwd <path>] [--from-session <session-id>]
dwo project update <project-id> <name>
~~~

Project 默认 kind 为 shared。Shared Project 未传 --cwd 时使用当前 Shell 路径；Independent
Project 禁止 --cwd。--from-session 创建后把来源 Session 移到新 Project 的未分类 Topic。

### Repository

~~~text
dwo project repository get <project-id>
dwo project repository clone <project-id> <url> <path> [--branch <branch>]
dwo project repository attach <project-id> <path> [--name <name>]
~~~

### Worktree

~~~text
dwo project worktree list <project-id>
dwo project worktree get <project-id> <worktree-id>
dwo project worktree create <project-id> <branch> <path> [--start-point <ref>] [--name <name>]
dwo project worktree attach <project-id> <path> [--name <name>]
dwo project worktree rename <project-id> <worktree-id> <name>
dwo project worktree detach <project-id> <worktree-id>
dwo project worktree remove <project-id> <worktree-id>
~~~

detach 只取消登记；remove 还会按 Host 的 Git Worktree 流程移除对应工作树。Project、Repository、
Worktree 字段见 [Project 文件与行为](projects.md)。

## Section

~~~text
dwo section list <project-id>
dwo section create <project-id> <name>
dwo section update <project-id> <section-id> <name>
dwo section delete <project-id> <section-id>
dwo section reorder <project-id> <section-id> <position>
~~~

## Topic

~~~text
dwo topic list <project-id>
dwo topic get <project-id> <topic-id>
dwo topic create <project-id> <section-id> <title>
dwo topic update <project-id> <topic-id> <title>
dwo topic delete <project-id> <topic-id>
dwo topic move <project-id> <topic-id> <section-id> [--to-project <project-id>] [--position <n>]
dwo topic reorder <project-id> <topic-id> <section-id> <position>
~~~

Section、Topic、Label、Session 归属和跨 Project 移动规则见 [Project 文件与行为](projects.md)。

## Model

~~~text
dwo model list
dwo model get-default
dwo model set-default <provider/modelId> --reasoning <mode> [--compaction-trigger-ratio <ratio>]
~~~

set-default 修改新 Session 默认值，不覆盖已有 Session。Provider 配置见 [Profile](profile.md)，
Model List 和能力见 [模型与 Provider](models.md)。

## MCP

~~~text
dwo mcp list
dwo mcp get <name>
dwo mcp add [-t|--transport <stdio|http>] [-e|--env KEY=value] [-H|--header "Name: value"] <name> [<url> | -- <command> [args...]]
dwo mcp add-json <name> <json>
dwo mcp remove <name>
dwo mcp search <query>
dwo mcp call <server.tool> [--args '<json>']
dwo mcp auth <server> [--logout]
~~~

Transport 字段、mcp.json、环境变量、OAuth 和连接生命周期见
[Prompt、Skill 与 MCP](resources.md#mcp)。

## Skills

~~~text
dwo skills list
dwo skills add <file-or-directory> [--name <name>]
dwo skills remove <name>
~~~

Skill 目录格式、Frontmatter、来源优先级和启用状态见
[Prompt、Skill 与 MCP](resources.md#skill)。

## Channel

~~~text
dwo channel list
dwo channel <weixin|telegram|feishu|qq> status
dwo channel <weixin|telegram|feishu|qq> bind
dwo channel <weixin|telegram|feishu|qq> unbind
dwo channel <weixin|telegram|feishu|qq> send-message <message>
dwo channel <weixin|telegram|feishu|qq> send-file <path>
~~~

平台凭据、绑定步骤、媒体和输出行为见 [Channel 配置与行为](channels.md)。send-message 和
send-file 是主动发送；普通 Agent 回答会由 Adapter 自动发送。

## WebSocket

~~~text
dwo websocket status
dwo websocket token
dwo websocket reset-token
~~~

连接地址和安全部署见 [WebSocket 连接](websocket.md)。

## Automation

所有命令接受全局 --project <id>。Agent Session 内省略时可从 DWO_SESSION_ID 推断当前 Project；
外部 Shell 必须显式提供。

~~~text
dwo automation --project <id> list [--json]
dwo automation --project <id> status <job> [--json]
dwo automation --project <id> add <name> --cron <expr> --prompt <text> [--timezone <zone>] [--session <every-time|once|fixed>] [--session-id <id>] [--topic <id>] [--title <title>] [--disabled] [--json]
dwo automation --project <id> enable <job>
dwo automation --project <id> enable --all
dwo automation --project <id> disable <job>
dwo automation --project <id> disable --all
dwo automation --project <id> delete <job>
dwo automation --project <id> delete --all --yes
dwo automation --project <id> run <job> [--json]
~~~

add 默认使用 local 时区、every-time Session，并立即启用。fixed 必须提供 --session-id。
delete 也可写成 del。Schema、调度和无人值守行为见 [Automation](automation.md)。

## ACP

~~~text
dwo acp [--protocol <v1|v2>]
~~~

默认 v2。它使用 stdio 与 ACP Client 通信，再通过本地 IPC 连接 daemon。配置示例和协议差异见
[ACP 连接](acp.md)。
