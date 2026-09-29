# ACP 连接

ACP（Agent Client Protocol）用于把 IDE 或其他 Agent Client 接到已运行的 Dwo daemon。
dwo acp 是 stdio 与本地 IPC 之间的 Adapter，不会启动第二套 Agent Runtime。

## 前置条件

~~~text
dwo daemon start
dwo daemon status
~~~

status 应显示 healthy: true。模型、Session 和工具都由 daemon 管理；关闭 IDE 只会断开
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
Prompt 时，cancel 正常生效；如果连接在窗口内断开（关闭窗口或客户端），待生效的 cancel 会随
连接一起丢弃，不会中断 daemon 中仍在运行的 Turn。本地命令（例如 /status、/resume）会消耗配对 cancel 而不取消 Turn；显式 /cancel 除外。

## Session

ACP 可以新建、列出、加载、继续和删除 daemon 中的 Session；`session/close` 用于关闭客户端
侧的对话视图。Client 创建 Session 时提供的 cwd 会成为 External Workspace，不会创建隐藏
Project。Project assignment 和 Workspace 类型见
[Project 文件与行为](projects.md)。

`session/close` 只断开当前连接对该 Session 的事件订阅，不会取消正在运行的 turn，也不会卸载
daemon 中的 Session。关闭对话、窗口或整个客户端后，Session 保持原样继续运行，之后可以随时
重新 load。中断运行用 `session/cancel`，删除用 `session/delete`。

加载 Session 后，ACP 会持续接收其他入口提交的 Prompt、Tool Event、权限请求和状态变化。
Model、Reasoning 和 Policy 作为 Config Option 显示，客户端修改后会写回 Session。

ACP 不接受非空的 session mcpServers 或 additionalDirectories。外部工具通过
[Skill](resources.md#skill) 使用。

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

### GUI 的 ACP v2 更新与回放

Adapter 使用本机 ACP SDK 2.0.0 / schema 1.5.0 的标准 v2 类型；没有新增私有
`sessionUpdate` 或 Tool Content 类型。GUI 应按 SDK 的 patch/append 语义处理：

- `tool_call_update.content` 是整个数组的替换；`tool_call_content_chunk` 追加单个内容项。
  文件变更、终端引用和完成结果以追加事件发送；后续工具状态或输入刷新不会清掉它们。
  一次 file_edit 中的多个文件（add、modify、delete、move）保留在同一个 Diff 的 `changes`
  中，多次变更事件也保留。Diff 在 daemon 提供 patch 时包含 patch，否则仅包含变更列表。
- `terminal_output_chunk.data` 是每个 chunk 独立编码的 base64 原始字节；解码后按顺序追加，
  不应把每个 chunk 单独按 UTF-8 解码。`terminal_update.output` 是输出的替换快照。
  terminal 工具的完成结果只是本次 poll 的未读输出，可能被裁剪或经过有损解码，因此不会
  用它替换或重复追加实时终端字节。退出信息由 `terminal_update.exitStatus` 发送。
- poll/input/kill 工具卡片通过标准 `terminal` content 引用原有 terminalId。
- `session/resume` 带 `replayFrom: {"type":"start"}` 时，工具和文件事件按记录顺序回放，终端输出
  优先从 daemon snapshot 的 `terminal_views` 恢复为标准 `terminal_update.output` 替换快照，
  包含关闭 GUI 期间尚未被工具 poll 的原始字节；随后通过标准 `terminal_output_chunk` 追加
  watermark 之后的输出。缓存为空的终端也发送空替换快照，不混入旧 poll 文本。
  旧 host 缺少该字段、actor 重新加载或单个终端缓存被淘汰时，按 terminalId 回退到 transcript
  重建：优先原始 terminal_output 事件，否则拼接连续 poll 文本。没有 replayFrom 时仅恢复
  当前状态，不发送历史终端输出。
- 当前计划恢复为 `plan_update`；历史计划按原 planId 清空，含 null 的当前计划也如此，避免
  GUI 按 ID 存储时留下旧任务。在途 assistant/reasoning
  从 daemon 的 active_step 恢复为同 messageId 的替换内容，后续继续接受实时更新。

**Snapshot schema 与边界：** daemon 的 `SessionSnapshot` 新增带 serde default 的
`terminal_views` 数组（旧数据缺省为 `[]`）。每项包含 `terminal_id`、`tool_call_id`、`command`、
`cwd`、`output`（base64 原始字节）、`dropped_bytes`、`status` 和可空 `exit_code`。actor 在广播
terminal telemetry 前更新缓存；订阅 receiver 和 snapshot 在同一个 actor 请求中获取，
snapshot 的 `seq` 是共同 watermark，只转发 `seq > watermark` 的实时事件。重复请求完整回放
也重新获取这对 snapshot/订阅。无需新增 ACP 方法或 CLI 命令。

**保留上限与实际限制：** 每个已加载 session 最多缓存 64 个终端，每个终端保留末尾
256 KiB，全部终端合计最多 1 MiB 原始输出（base64 约 1.34 MiB）。超出字节预算时从最久未更新终端的头部裁剪，
累计到 `dropped_bytes`；超出数量时优先淘汰最久未更新的已退出终端，否则淘汰最久未更新终端。
单个终端的 ID、tool call ID、command 和 cwd 合计超过 16 KiB 时不缓存该终端，使用 transcript
回退；这些 metadata 的总预算为 64 KiB，超出时按同样顺序淘汰。终端视图序列化预算低于
2 MiB，为 WebSocket 的 4 MiB 单帧上限留出空间；整个 snapshot 的 transcript 等其他字段
仍会占用空间，因此这不是整个 snapshot 大小的保证。ACP 在标准 output 的可选 `_meta.dwo` 中发送 `truncated`、`droppedBytes` 和
`snapshotSeq`，不把截断提示插入字节流。裁剪点可能落在 UTF-8 或 ANSI 序列中间；客户端应在
替换快照时重置解码器，并按顺序处理快照与后续 chunk，不能逐 chunk 有损解码。

缓存独立于模型 terminal poll 的未读游标，读取 snapshot 不会执行工具或消耗未读输出。
它只覆盖当前 actor 生命周期内收到的 telemetry，不持久化：daemon 重启或 session 卸载后，
以及被淘汰终端，只能回退恢复 transcript 保存的内容；poll 文本无法还原被裁剪或有损解码的
原始字节。慢订阅者广播溢出时会断开，需要重新完整回放；没有终端专用补齐游标。

v1 沿用兼容转换和文本工具结果，不承诺 v2 的多文件 Diff、原始终端流或状态恢复语义。

### 子 Agent

ACP v2 `session/list` 包含子会话；每个子会话都是可独立 resume、prompt、cancel 的标准
Session。可选 `_meta.dwo.parentSessionId` 和 `_meta.dwo.forkedFrom` 保留父子关系与 fork
来源，便于 Dwo GUI 分组；其他 ACP Client 可以忽略该 metadata。SDK 没有标准 child-agent
专用更新类型，Adapter 不会编造一种，也不会把子会话的完整事件流混入父会话。GUI 需分别
订阅子会话；子任务结果仍按 daemon 原有机制作为父会话消息交付。

## Slash Commands 和计划

ACP 会发布 /cancel、/compact、/resume、/fork、/status，以及当前 Catalog 中的
/skill <name>。语法和消息平台差异见 [Slash Commands](slash-commands.md)。

todo 工具的普通 Tool Lifecycle 不直接显示；ACP v2 使用 plan_update，v1 使用兼容的 plan
Session Update。计划不会自动启动下一 Turn，只有新 Prompt 或 /resume 会继续。

ACP 协议自己的 session/resume 是重新连接已有 Session，不会调用模型；它和 /resume 不是一件事。
/resume 在空闲时直接使用已有上下文启动 Turn，不追加提示消息；运行中静默忽略。

v1 重新 load 可以恢复消息和工具事件，但没有 v2 的 state_update，客户端可能不恢复 Cancel 按钮。
此时可发送 /cancel 停止后台任务。v2 会同步运行状态；按钮表现仍取决于客户端实现。

v2 resume 正在运行的 Session 后，`session/cancel` 或 `/cancel` 控制同一个 daemon turn。
此时发送普通 `session/prompt` 会进入该 turn 的 steering 队列，响应表示已接受，实际消费由
daemon 决定。`/status` 不发起模型请求；`/resume` 不重启当前 turn；运行中的 `/compact`
和 `/fork` 遵循 daemon 的空闲限制。恢复等待权限的 Session 会恢复 requires_action 状态，
但当前 adapter 不会从 snapshot 重新发送已经错过的 permission 请求。

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
