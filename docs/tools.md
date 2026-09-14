# Agent 工具

这些工具由 daemon 直接执行，不需要为基础能力单独配置 MCP。模型能看到的工具只有下面四个；
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

命令由统一的 shell 包装执行并强制 UTF-8 输出：Windows 只使用安装阶段准备好的
niubash（`niu.exe`）；Unix 使用 `sh`。环境快照中的 `shell` 字段与实际使用的 shell 一致。

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

操作按顺序应用；某项失败时，之前的操作保持已应用，之后的操作不会执行、也不会再尝试。
结果中的 summary、failed 和 skipped 会列出成功数量、失败的操作与原因、以及被跳过的操作，
便于模型只补发缺失的部分。

## plan

读取或完整替换当前 Session 的执行计划。它只更新计划，不会启动另一轮模型调用。turn 结束仍有
未完成计划时，daemon 会保存计划并等待新的 prompt 或 /resume。

计划条目包含 content、priority 和 status。status 可为 pending、in_progress、completed 或
cancelled；清空 entries 就是清除计划。

## 权限

三种 Session policy 的范围：

| policy | 行为 |
| --- | --- |
| full_access | 终端和文件操作直接执行，仍遵守显式 deny rule |
| confirm | 简单只读命令自动执行，其余终端和 file_edit 请求批准 |
| watch | 只允许简单只读命令和明确的 allow rule，拒绝写入 |

每个 terminal intent（运行、输入、终止）和 file_edit 都只经过一次授权。父 Session 创建子 Agent
时不能提高权限。
