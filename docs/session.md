# Session 与子 Agent

Session 默认是持久的 Agent 对话，保存模型上下文、transcript、权限设置和运行状态。只有创建时
显式传入 `--ephemeral` 才是临时 Session。Project
只通过自己的 sessionAssignments 记录归属；session.json 不保存 project、section、worktree
或规则注入字段。

## 创建 Session

~~~text
dwo session prompt "检查这个项目的结构" --cwd <path> --title "项目检查"
dwo session prompt "实现登录页" --project <project-id> [--section <section-id>]
~~~

| 选项 | 作用 |
| --- | --- |
| --cwd | 使用指定外部工作目录，不创建 Project assignment |
| --project | 使用 Project root 或 Worktree，并记录 Session assignment |
| --section | 选择 Project Section；省略时使用默认 Section |
| --title | Session 标题 |
| --model / --reasoning | 覆盖默认模型和 reasoning |
| --policy | 使用 full_access、confirm 或 watch |

cwd 与 project 不能同时指定。两者都省略时，Host 在 runtime/workspaces/<session-id> 创建
managed workspace。外部终端创建根 Session；Agent Session 中创建的是当前 Session 的直接
子 Session。

## 继续、Fork 和临时子 Agent

~~~text
dwo session prompt "补充检查错误处理" --to <session-id>
dwo session prompt "换一种方案验证" --from <session-id> --title "方案 B"
dwo session prompt "只检查 crates/dwo-auth" --policy watch --ephemeral
~~~

不显式覆盖 cwd/project/section/worktree 时，子 Session 继承父 Session 的 cwd 和 Project
assignment。Fork 复制 context、transcript、父子关系和运行配置；它也继承来源 Session 的
工作目录与归属。--to 和 --from 互斥，Fork 来源必须 idle。

Ephemeral Session 在 turn 结束后进入自动删除宽限期，宽限期结束后直接 delete，不经过手动
archive。清理前运行 session keep 可以将它转换为持久 Session。

## 配置继承和权限

新建或 Fork 时默认继承父 Session 的 cwd、policy、model 和 reasoning，也可以用选项覆盖。
权限只能收紧：

~~~text
watch < confirm < full_access
~~~

Project rules 不会注入 Session 文件；模型直接按当前 cwd 加载 profile rules 和 AGENTS.md。

## 运行中的消息

同一个 Session 可以同时被 CLI、ACP 和消息 channel 使用。运行中的新 prompt 按 FIFO 排队，
在模型响应或 tool-call batch 边界进入下一步。需要中断时使用：

~~~text
dwo session cancel <session-id>
~~~

取消会清理排队的用户 prompt；已经到达的内部 watcher 或子 Agent 结果仍会写入 context。

## 查看、归档和删除

~~~text
dwo session list [--all] [--archived]
dwo session status <session-id>
dwo session watch <session-id> [--cursor <cursor>] [--limit <count>]
dwo session set <session-id> [--title <title>] [--policy <policy>] [--model <model>] [--reasoning <mode>]
dwo session archive <session-id>
dwo session delete <session-id>
~~~

archive 将目录移动到 runtime/sessions/archive。Project assignment 在真正 delete 时由 Host 清理。Archive Session
可以查询，但不能继续执行 Prompt 或作为 fixed Automation 目标。delete 只接受 archived
Session，成功后才真正删除 session 文件和 DWO 管理的 managed workspace；Project root、外部
cwd 和 Git Worktree 不会被删除。

list 默认显示当前调用上下文可见的 Session；--all 显示 Host 中全部 active Session，--archived
显示归档内容。watch 是按 cursor 分页读取，不是持续订阅。

## ACP 和 Channel

ACP new_session 可以只提供 cwd；Host 将它作为 external Session，不创建隐藏 Project。消息
channel 维护当前选择的 Session，/new 创建、/use 切换、/fork 复制但不切换。入口差异见
[ACP 连接](acp.md)、[Channel 配置与行为](channels.md) 和 [Slash Commands](slash-commands.md)。

规则来源和 Project/Worktree 解析见 [Project 文件与行为](projects.md)；定时触发见
[Automation](automation.md)。
