# Agent 工具

这些工具由 daemon 直接执行，不需要为基础能力单独配置 MCP。模型能看到的工具只有下面五个；
MCP Server 的配置见 [Prompt、Skill 与 MCP](resources.md#mcp)。

## terminal

在 Session Workspace 中运行命令或管理交互式进程。

| 调用方式 | 作用 |
| --- | --- |
| 省略 terminal_id，提供 command | 新建终端并运行命令 |
| 提供 terminal_id 和 command | 向已有终端写入输入 |
| 只提供 terminal_id | 读取增量输出 |
| terminal_id + kill: true | 终止进程并读取尾部输出 |

新终端总是在当前 Session 的 Workspace 启动，工具没有 cwd 参数。单个终端输出最多保留 1 MiB，
返回给模型的结果最多 20,000 UTF-8 字节。timeout_ms 限制新终端的总时长，yield_ms 限制每次
等待输出的时间。

## read_file

读取文本或把图片加入模型上下文。

- 文本按连续行返回，一次最多 500 行。
- 使用 cursor 和 next_cursor 分页；offset 用于 Unicode 字符偏移。
- 支持 UTF-8 文本以及 PNG、JPEG、GIF、WebP。
- 图片只有在当前模型声明支持 imageInput 时才会加入上下文。
- 相对路径相对 Session Workspace 解析。

## file_edit

使用一个结构化 patch 新建、修改、移动或删除文件。一次 tool batch 最多一个 file_edit 调用，
但一个 patch 可以包含多个文件的相关操作。confirm 模式会请求批准，watch 模式拒绝写入。

## plan

读取或完整替换当前 Session 的执行计划。它只更新计划，不会启动另一轮模型调用。turn 结束仍有
未完成计划时，daemon 会保存计划并等待新的 prompt 或 /resume。

计划条目包含 content、priority 和 status。status 可为 pending、in_progress、completed 或
cancelled；清空 entries 就是清除计划。

## handoff

Agent 认为当前上下文不适合继续时，用 handoff_text 写下目标、已完成工作、决定、未解决问题
和下一步。daemon 会重建当前模型上下文，然后在同一个 turn 继续。

handoff 必须是 tool batch 中唯一的工具调用，文本不能为空且不超过 32,000 UTF-8 字节；它不
执行外部操作，始终不需要权限确认。需要独立并行工作的任务请创建子 Session，见 [Session 与子
Agent](session.md)。

## 权限

三种 Session policy 的范围：

| policy | 行为 |
| --- | --- |
| full_access | 终端和文件操作直接执行，仍遵守显式 deny rule |
| confirm | 简单只读命令自动执行，其余终端和 file_edit 请求批准 |
| watch | 只允许简单只读命令和明确的 allow rule，拒绝写入 |

每个 terminal intent（运行、输入、终止）和 file_edit 都只经过一次授权。父 Session 创建子 Agent
时不能提高权限。
