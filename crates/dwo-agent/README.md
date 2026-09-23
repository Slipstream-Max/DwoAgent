# dwo

dwo 是长期运行的 dwoagent Host 和控制 CLI。一个 daemon 持有 profile、Session、Project、
Channel、模型客户端和工具 runtime；CLI 与 ACP 通过本地 IPC 连接，不会各自创建
SessionService。

~~~text
crates/dwo-agent/       binary composition entry point
crates/dwo-cli/         commands, install, rendering
crates/dwo-host/        long-running state owner and management APIs
crates/dwo-ipc/         local named-pipe/Unix-socket transport
crates/dwo-websocket/   remote /acp and /dwo transport
crates/dwo-acp/         ACP v1/v2 adapters
crates/dwo-channels/    message platform adapters
crates/dwo-command/     shared slash-command behavior
crates/dwo-protocol/    Dwo RPC envelopes and method registry
~~~

只有 dwo serve 构造 Host。外部命令通过 IPC 调用 Host；Channel runtime 在 daemon 内运行，
并复用同一套 Session 和 Project 逻辑。

## 常用命令

~~~text
dwo install [--start]
dwo uninstall [--purge]
dwo serve
dwo daemon start|stop|status
dwo config-show

dwo session list [--all] [--archived]
dwo session prompt <message> [--cwd <path> | --project <id> [--section <id>]]
dwo session move <id> --project <id> --section <id>
dwo session archive <id>
dwo session delete <id>
dwo session keep <id>
dwo session cancel <id>
dwo session watch <id> [--cursor <cursor>] [--limit <count>]

dwo project list|get|create|update|delete ...
dwo section list|create|update|delete|reorder ...
dwo session move ...
dwo project worktree list|get|create|attach|rename|detach|remove ...

dwo automation --project <id> list|add|enable|disable|delete|run ...
dwo automation --global list|add|enable|disable|delete|run ...
dwo acp [--protocol v1|v2]
~~~

project create 接收 --cwd <path>，名称可省略；创建时自动得到默认 Section。Project 只管理
pwd、Section、Session assignment、规则文件、Repository、Worktree 和 Automation scope。
Session 通过 assignment 记录到 Project 的 Section，session.json 不保存这些属性。

创建 Session 时：

| 参数 | 行为 |
| --- | --- |
| --project <id> | 使用 Project root 或指定 Worktree，并写入 Section assignment |
| --cwd <path> | 使用外部 cwd，不创建 Project |
| 两者都省略 | 在 runtime/workspaces/<session-id> 创建 managed workspace |

子 Session 和 Fork 默认继承父 Session 的 cwd、Project、Section、Worktree、model 和 policy；
权限只能收紧。--to 继续已有 Session，--from 从 idle Session Fork，两者互斥。

Session 默认是持久 Session；只有创建时传入 --ephemeral 才是临时 Session。Session 先 archive 再 delete。
Archive 将目录移动到 runtime/sessions/archive；delete 时 Host
清理 Project assignment 并真正删除 Session 文件和 DWO 管理的 workspace。Ephemeral Session
结束后进入自动删除宽限期，宽限期结束后直接删除；session keep 可将它转换为持久 Session。

## Runtime 布局

~~~text
profile.yaml
resource/prompts/System.md
resource/prompts/AGENTS.md
resource/skills/
resource/mcp/mcp.json
runtime/sessions/<date>/<session-id>/
  session.json
  model_context.json
  client_transcript.jsonl
runtime/sessions/archive/<session-id>/
runtime/projects/<project-id>/project.json
runtime/automations/<project-id>/{config,history}.yaml
runtime/automations/global/{config,history}.yaml
runtime/workspaces/<session-id>/
runtime/attachments/
runtime/websocket/secret.yaml
logs/
~~~

Project rules 位于 <project.pwd>/AGENTS.md，由 project.rules.get/set 读写。Prompt 使用
profile rules 和当前 cwd 下的 AGENTS.md，不复制规则到 Session。

Project 的 Worktree assignment 决定 Session cwd：没有 Worktree 时使用 Project root；Worktree
detach 只解除登记，remove 才调用 Git 删除。Global Automation 可以指定 cwd；Project
Automation 只能使用 Project root 或 Worktree。三种 Automation Session 行为是 every_time、
once 和 fixed，fixed 继承目标 Session 的 cwd 与 assignment。

Windows 使用 named pipe 和隐藏的登录启动任务；macOS 使用 Unix domain socket 和 launchd。
serve 保持前台运行，系统服务管理器负责后台生命周期。完整命令与 schema 见
[docs/cli.md](../../docs/cli.md)、[docs/projects.md](../../docs/projects.md) 和
[docs/automation.md](../../docs/automation.md)。
