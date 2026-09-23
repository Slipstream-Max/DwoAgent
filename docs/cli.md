# CLI 命令参考

dwo 是 daemon 的命令行客户端。除 serve 外，命令通过本地 IPC 调用已运行的 daemon。默认
Profile 是 ~/.dwoagent/profile.yaml。

## 安装和 daemon

~~~text
dwo install [--start]
dwo uninstall [--purge]
dwo serve
dwo daemon start|stop|status
dwo config-show
~~~

## Session

~~~text
dwo session list [--all] [--archived]
dwo session status <id> [--json]
dwo session prompt <message> [options]
dwo session set <id> [--title <title>] [--policy <policy>] [--model <model>] [--reasoning <mode>]
dwo session move <id> --project <project-id> --section <section-id>
dwo session keep <id>
dwo session archive <id>
dwo session watch <id> [--cursor <cursor>] [--limit <count>]
dwo session cancel <id>
dwo session approve|deny <id> <permission-id>
dwo session delete <id>
~~~

prompt 选项：

~~~text
--title <title>
--cwd <path>
--project <project-id> [--section <section-id>]
--policy <full_access|confirm|watch>
--model <provider/modelId>
--reasoning <mode>
--ephemeral                 # 默认是持久 Session；传入后才是临时 Session
--to <session-id>
--from <session-id>
~~~

--cwd 和 --project 互斥；--section 要求 --project。两者都省略时创建
runtime/workspaces/<session-id>。--to 继续子 Session，--from Fork 后发送新 Prompt，两者互斥。
新建 Session 默认持久；传入 --ephemeral 后才会在完成后自动清理。archive 后才允许 delete；
keep 将临时 Session 转为持久 Session。完整行为见 [Session](session.md)。

## Project

~~~text
dwo project list
dwo project get <project-id>
dwo project create [name] --cwd <path>
dwo project update <project-id> <name>
dwo project delete <project-id>
~~~

Project 创建会规范化 cwd，创建默认 Section；name 可省略。Project 命令只覆盖项目、规则、
Section、Session assignment 和 Worktree。

### Section 和 Session assignment

~~~text
dwo section list <project-id>
dwo section create <project-id> <name>
dwo section update <project-id> <section-id> <name>
dwo section delete <project-id> <section-id>
dwo section reorder <project-id> <section-id> <position>
dwo session move <session-id> --project <project-id> --section <section-id>
~~~

没有 Section 参数时使用 Project 默认 Section。默认 Section、仍被 Session 使用的 Section 或
仍被 Automation 引用的 Section 不能删除。

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

detach 只取消登记；remove 还会按 Host 的 Git Worktree 流程移除对应工作树。Session 使用
Project root 或其 assignment 指定的 Worktree cwd。

## Model、Skill、Channel 和 WebSocket

~~~text
dwo model list
dwo model get-default
dwo model set-default <provider/modelId> --reasoning <mode>
dwo skills list|add|remove ...
dwo channel list
dwo channel <weixin|telegram|feishu|qq> status|bind|unbind
dwo channel <weixin|telegram|feishu|qq> send-message <message>
dwo channel <weixin|telegram|feishu|qq> send-file <path>
dwo websocket status|token|reset-token
~~~

详细字段见 [Profile](profile.md)、[资源](resources.md)、[Channel](channels.md) 和
[WebSocket](websocket.md)。

## Automation

Project Scope：

~~~text
dwo automation --project <id> list [--json]
dwo automation --project <id> add <name> --cron <expr> --prompt <text> [--section <id>] [options]
dwo automation --project <id> enable <job|--all>
dwo automation --project <id> disable <job|--all>
dwo automation --project <id> delete <job|--all> [--yes]
dwo automation --project <id> run <job> [--json]
~~~

Global Scope：

~~~text
dwo automation --global list [--json]
dwo automation --global add <name> --cron <expr> --prompt <text> [--cwd <path>] [options]
dwo automation --global enable|disable|delete|run ...
~~~

Session 行为用 --session every-time、once 或 fixed；fixed 还必须传 --session-id。Agent Session
内省略 scope 参数时，Host 从 caller session 推导 Project；外部 Shell 必须显式使用
--project 或 --global。Schema 和触发规则见 [Automation](automation.md)。

## ACP

~~~text
dwo acp [--protocol <v1|v2>]
~~~

默认 v2，通过 stdio 与 ACP Client 通信，再经本地 IPC 连接 daemon。
