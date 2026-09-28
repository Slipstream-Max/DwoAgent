# Slash Commands

ACP（stdio 和 WebSocket /acp）和消息 Channel 共用以下命令。WebSocket /dwo 使用 Management RPC。

| 命令 | ACP | 微信/Telegram/飞书/QQ |
| --- | --- | --- |
| /cancel、/compact、/resume、/fork、/status | 支持 | 支持 |
| /skill <name> [prompt] | 支持 | 支持 |
| /help、/list、/new、/use、/del | 不支持 | 支持 |
| /model、/reasoning、/policy | 使用客户端配置选项 | 支持 |
| /allow、/deny | 使用客户端权限交互 | 支持 |

## 共同命令

| 命令 | 行为 |
| --- | --- |
| /cancel | 取消当前执行；重新加载 ACP v1 会话后也可用，不依赖 Cancel 按钮 |
| /compact | 压缩模型上下文；忙碌时返回错误，不取消当前任务 |
| /resume | 空闲时直接基于已有上下文启动新 Turn，不追加“继续”或空消息；运行中静默忽略 |
| /fork | 复制当前 Session，返回新 ID，不自动切换 |
| /status | 显示会话配置与状态，不取消任务，也不向模型追加消息 |

这些本地命令不接受参数。ACP v1 Send now 的配对 cancel 不会因为查询状态或执行其他本地命令而被转发为取消；显式 `/cancel` 仍会取消。
ACP v1 客户端仍可能在本地命令返回后把按钮显示为发送，后台任务状态不受影响。
ACP 协议的 `session/resume` 只重新连接会话，不等同于 `/resume`。

`/skill <name> [prompt]` 是唯一的 Prompt Directive，会展开为指定 Skill 的引用并交给模型。
它支持出现在正文中和重复出现；未知 Skill 或单独的 `/skill` 保持原文。ACP 发布可用 Skill 补全。

## Channel 会话管理

```text
/list
/list 2
/list "我的项目"
/list "我的项目" 2

/new
/new "我的项目"
/new "我的项目" "开发"

/use 短ID
/use 1
/use "我的项目" 1
/del 会话
```

`/list` 默认显示 Inbox：所有未归档且未归属项目的顶层会话，按最近一次 Turn 结束时间倒序。
尚未执行的会话排在后面，同时间按更新时间倒序。项目列表按分区组织，分区内使用相同排序。
每页 20 条，编号跨页连续；第 1 页刷新列表，后续页与 `/use` 复用该范围的列表快照，防止后台排序变化导致选错。
Inbox 和各项目的编号互相独立；`/use 1` 始终使用最近的 Inbox 列表。纯数字项目名可用 `/list "123" 1` 指定。

`/new` 创建无项目归属的 work（Managed Workspace）会话；指定项目时使用项目的默认未分类分区，
第二个参数指定分区。创建后自动选中。项目/分区名必须精确匹配；不存在或重名会报错，不自动创建。
名称带空格时用引号包起来。

`/del` 按 Inbox 序号、短 ID 或完整 ID 归档会话，保留历史，不永久删除。
运行中的会话需要先 `/cancel`，待空闲后再归档。归档当前会话后清除 Channel 的选中状态。
`/help` 显示命令列表。

## Channel 配置与权限

| 命令 | 行为 |
| --- | --- |
| /model | 按 provider 分组列出可用模型，显示当前模型 |
| /model model | 使用唯一的模型名称或模型 ID 切换；重名时报错 |
| /model provider/model | 按 provider 与模型 ID/名称精确切换 |
| /reasoning | 列出当前模型支持的强度和当前有效值；未显式设置时显示模型默认值 |
| /reasoning level | 校验并设置支持的强度；off 仅在模型支持时可用 |
| /policy | 列出 full_access、confirm、watch，并显示当前值 |
| /policy mode | 修改权限模式 |
| /allow [id] | 批准当前或指定权限请求 |
| /deny [id] | 拒绝当前或指定权限请求 |

Telegram 的 `/status@bot_name` 会规范化为 `/status`。Channel 中未知 `/command` 返回命令错误；
ACP 中未识别的命令通常作为普通 Prompt 传给模型。普通消息排队，不取消工具；需要中断使用 `/cancel`。
