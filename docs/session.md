# Session 与子 Agent

Session 是一次持久的 Agent 对话。它保存自己的模型上下文、完整 transcript、权限设置和
Workspace 绑定。Project/Topic 负责归类，真正执行 prompt 的始终是 Session。

## 创建 Session

最短方式：

~~~text
dwo session prompt "检查这个项目的结构" --cwd <path> --title "项目检查"
~~~

在已有 Project 中创建：

~~~text
dwo session prompt "实现登录页" --project <project-id> --topic <topic-id>
~~~

选项：

| 选项 | 作用 |
| --- | --- |
| --cwd | 新 Session 使用的外部工作目录 |
| --project | 放入指定 Project；省略时进入 project-unassigned |
| --topic | 放入指定 Topic；省略时使用未分类 Topic |
| --title | Session 标题 |
| --model / --reasoning | 覆盖默认模型和 reasoning |
| --policy | 使用 full_access、confirm 或 watch |

cwd 与 project 不能同时指定。外部终端创建的是根 Session；Agent Session 中创建的是当前
Session 的直接子 Session。

## 继续、Fork 和临时子 Agent

继续一个直接子 Session：

~~~text
dwo session prompt "补充检查错误处理" --to <session-id>
~~~

从已有子 Session 复制出一个新上下文：

~~~text
dwo session prompt "换一种方案验证" --from <session-id> --title "方案 B"
~~~

模型需要把一项边界清楚的工作交给另一个 Agent 时，可以创建临时子 Agent：

~~~text
dwo session prompt "只检查 crates/dwo-auth，列出风险和测试建议" --policy watch --title "auth review"
~~~

子 Agent 有独立 context 和 transcript，可以继续创建自己的子 Agent。父 Session 不需要轮询：
子 turn 结束后 daemon 会投递一条内部 subsession_result 消息；父 Session 空闲时会立即处理，
正在运行时则在当前 model response 或 tool-call batch 结束后接收。

不显式传 --cwd 或 --project 时，子 Agent 继承父 Session 的 Project、Topic 和 Workspace，因此
模型可以围绕当前 Topic 创建相关的临时工作 Session，而不会另造一套 Project。一次性检查可加
--ephemeral：

~~~text
dwo session prompt "临时验证这个修复，只返回测试结果" --ephemeral --policy watch
~~~

Ephemeral Session 的 Turn 结束后保留 5 分钟再自动删除，期间可查看结果。需要保留时运行
dwo session keep <session-id>；完成后的 Ephemeral Session 在 keep 之前不再接受新 Prompt。

子 Session 适合代码检查、资料整理、运行测试等边界明确的任务。需要当前 Agent 压缩自己并在
同一 turn 继续时，用 handoff，而不是创建子 Session；见 [Agent 工具](tools.md#handoff)。

--to 和 --from 互斥，Fork 来源必须 idle。Fork 会复制来源的 context、transcript、父子关系和
配置，但返回的新 Session 不会自动成为当前入口所选 Session。

## 配置继承和权限

新建或 Fork 时默认继承父 Session 的 cwd、policy、model 和 reasoning，也可以用命令选项覆盖。
权限只能收紧，不能放宽：

~~~text
watch < confirm < full_access
~~~

例如父 Session 为 confirm，子 Session 只能使用 confirm 或 watch。这个限制会沿多层子 Agent
继续生效。

## 运行中的消息

同一个 Session 可以同时被 CLI、ACP 和消息 channel 使用。运行中的新 prompt 会按 FIFO 排队，
在模型响应或 tool-call batch 的边界进入下一步；它不会隐式取消当前工具。需要中断时使用：

~~~text
dwo session cancel <session-id>
~~~

取消会清理排队的用户 prompt；已经到达的内部 watcher 或子 Agent 结果仍会写入 context，但不会
因此自动启动另一轮模型调用。

## 查看和删除

~~~text
dwo session list [--all]
dwo session status <session-id>
dwo session watch <session-id> [--cursor <cursor>] [--limit <count>]
dwo session set <session-id> [--title <title>] [--policy <policy>] [--model <model>] [--reasoning <mode>]
dwo session delete <session-id>
~~~

在 Agent Session 中，list 默认只显示当前 Session 的直接子 Agent；外部 Shell 默认显示根 Session。
--all 显示 Host 中的全部 Session。status 返回配置、Usage、Active Turn 和最近一次结果；完整事件用
watch 按 next_cursor 继续读取。

watch 是分页读取，首次返回 next_cursor。Session 删除不会删除 Project 默认路径、Git Worktree
或 External workspace；只会清理 Dwo 拥有的 Managed workspace。

## ACP 和 Channel 中的 Session

ACP 的 new_session 通常只提供 cwd，Host 会将它放入未分配 Project。消息 channel 额外维护
一个当前选择的 Session，/new 创建、/use 切换、/fork 复制但不切换。入口差异见
[ACP 连接](acp.md)、[Channel 配置与行为](channels.md) 和 [Slash Commands](slash-commands.md)。

Session 的 Project、Topic 与 Workspace 关系见 [Project、Topic 与 Workspace](projects.md)；
持久化文件和规则来源见 [Profile 配置](profile.md)。
