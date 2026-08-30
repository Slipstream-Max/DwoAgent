# Project 与看板

Project 是 Host 中项目路径、可选仓库/Worktree、看板和 Automation 的聚合根。看板用于把持续工作组织成分区和话题；它不替代 Session，Session 通过 Topic 归类。

## 结构

```text
Project
|- id / name / kind
|- pwd                       # shared 必填，independent 禁止
|- optional repository
|- worktrees[]               # 仅 shared
`- Board
   |- Sections
   |- Topics
   `- Labels

Topic
|- sectionId / title / order
|- sessionIds[]
|- labelIds[]
|- overview.md
`- AGENTS.md
```

Session 归属只由 Topic 的 `sessionIds[]` 保存。Session 自己保存 Workspace 绑定，Project 没有 `workspaces[]`；Automation Job 自己保存可选的 `topicId`，因为任务配置和运行历史属于 Project 的 automation 目录。

Project 的 `kind` 有两种：`shared` 代表所有 Session 默认使用同一个必填 `pwd`，也可以选择该 Project 登记的 Git Worktree；`independent` 不允许 `pwd`、Repository 或 Worktree，每个 Session 使用自己的 Managed 或 External 路径。固定的 `project-unassigned` 始终是 `independent`。

每个 Project 创建时都有一个“未分类”分区和话题。调用 `session.new` 时可以省略 `topic_id`，Host 会使用该 Project 的未分类话题。完全省略 `project_id` 时，无论是否提供外部 cwd，Host 都把 Session 放入固定 ID 为 `project-unassigned`、名称为“未分配会话”的 Project；不会再为每个对话创建一个 Project。

ACP 的标准 `new_session` 只有 cwd，没有 Project/Topic 字段。ACP adapter 只需把 cwd 转发给 `session.new`；Host 将它放入“未分配会话”的未分类 Topic。ACP fork 继续继承源 Session 的 Topic：Managed Workspace 会复制到新 Session 的目录，External 路径则继续引用原路径。

## 持久化

```text
runtime/projects/<project-id>/
|- project.json
|- automation/
|  |- config.yaml
|  `- history.yaml
`- topics/<topic-id>/
   |- overview.md
   `- AGENTS.md

runtime/workspaces/<session-id>/
`- Session 工作文件
```

`project.json` 保存 `id`、`name`、`kind`、Project 默认 `pwd`、可选 Repository、Worktree 和 Board，不保存 Workspace 目录或记录。Topic 的 `sessionIds[]` 可以反查 Project/Topic 归属。`session.json` 保存 `workspace` 绑定但不保存解析后的 `cwd`；Host 加载时按下面规则解析：

```text
project_default -> Project.pwd
worktree        -> Project.worktrees[worktreeId].path
managed         -> runtime/workspaces/<session-id>/
external        -> workspace.pwd
```

只有 Managed Workspace 由 DWO 创建，并在对应 Session 删除或离开该 Workspace 后删除。External、Project 默认路径和 Git Worktree 都不由 Session 生命周期删除。不符合当前字段或约束的数据会直接报错，不执行旧格式迁移。

## Topic Knowledge

Desktop 将 Topic 的 `AGENTS.md` 显示为 Knowledge。Host 创建 Topic Session 时，将该文件注册到
SessionService 的 per-session external rule file registry：

```text
ExternalRuleFile
|- path = runtime/projects/<project-id>/topics/<topic-id>/AGENTS.md
`- pwd  = Session.cwd
```

SystemPromptBuilder 自己读取文件。初始 prompt、上下文压缩重建和 environment watcher 使用同一规则源；Host 不读取内容后拼接 prompt。每份规则快照都带 `source`、`pwd` 和内容。未分类话题的空 `AGENTS.md` 不增加规则，但文件以后写入内容时 watcher 仍会发现。

移动 Session 到另一个 Topic 时，Host 同时更新 Topic 的 `sessionIds` 和 SessionService 的
per-session external rule file registry。Actor/Handle 不接收规则更新请求；active turn 在下一
model step 扫描共享 registry，并通过既有 EnvironmentWatcher 注入变化。

## Topic 详情

`project.topic.get` 由 Host 组合：

```text
dwo-project       -> Topic、Markdown、Label
SessionService    -> sessionIds 对应的 Session 状态
AutomationRuntime -> Project automation 中 topicId 匹配的任务状态
```

Automation Job 通过 `automation.add` 写入 Project 的 `automation/config.yaml`，需要归类时设置 `job.topicId`。全局 AutomationRuntime 负责调度，Topic 详情筛选当前 Project 中 `topicId` 匹配的 Job。

Automation 在 `shared` Project 中使用 `Project.pwd`，在 `independent` Project 中为 Session 分配独立 Managed Workspace。Session 加入 `Topic.sessionIds`，并加载同一份 Topic `AGENTS.md`。删除 Topic 会把关联 Session 和 Job 移回未分类 Topic。

## Inbox

Inbox 没有后端实体。`session.list` 返回运行阶段、模型、思维链模式和 policy；需要最近一次终态时按需调用 `session.status`。Desktop 按这些字段筛选、排序即可生成 Inbox。

## Management API

| 方法组 | 操作 |
| --- | --- |
| `project.list/get/create/update` | Project 查询和编辑 |
| `project.board` | 完整 Board 快照 |
| `project.section.create/update/delete/reorder` | 分区管理 |
| `project.topic.get/create/update/delete/move/reorder` | 话题和 Topic 聚合详情 |
| `project.topic.overview.get/set` | 概述与计划 Markdown |
| `project.topic.agents.get/set` | Knowledge Markdown |
| `project.topic.session.assign/unassign` | Session 归类；unassign 移回未分类话题 |
| `automation.list/status/history` | 查询指定 Project 的任务和运行历史 |
| `automation.add/update/enable/disable/delete/run` | 修改或执行指定 Project 的任务 |
| `project.label.create/update/delete/assign/unassign` | 看板标签管理 |

所有看板变更发布 `project.changed`。删除 Label 会清理所有 Topic 的对应 `labelId`；删除普通 Topic 会把 Session 和 Job 移回未分类话题。

## CLI

CLI 将 Project、Section、Topic 和 Session 作为四类顶层资源命令：

```text
dwo project list
dwo project get <project-id>
dwo project create <name> [--kind <shared|independent>] [--cwd <path>] [--from-session <session-id>]
dwo project update <project-id> <name>
dwo project repository get <project-id>
dwo project repository clone <project-id> <url> <path> [--branch <branch>]
dwo project repository attach <project-id> <path> [--name <name>]
dwo project worktree list <project-id>
dwo project worktree get <project-id> <worktree-id>
dwo project worktree create <project-id> <branch> <path> [--start-point <ref>] [--name <name>]
dwo project worktree attach <project-id> <path> [--name <name>]
dwo project worktree rename <project-id> <worktree-id> <name>
dwo project worktree detach <project-id> <worktree-id>
dwo project worktree remove <project-id> <worktree-id>

dwo section list <project-id>
dwo section create <project-id> <name>
dwo section update <project-id> <section-id> <name>
dwo section delete <project-id> <section-id>
dwo section reorder <project-id> <section-id> <position>

dwo topic list <project-id>
dwo topic get <project-id> <topic-id>
dwo topic create <project-id> <section-id> <title>
dwo topic update <project-id> <topic-id> <title>
dwo topic delete <project-id> <topic-id>
dwo topic move <project-id> <topic-id> <section-id> [--to-project <project-id>] [--position <n>]
dwo topic reorder <project-id> <topic-id> <section-id> <position>

dwo session move <session-id> --project <project-id> --topic <topic-id>
```

这些命令对应的 Management RPC 仍使用 `project.section.*`、`project.topic.*` 等路由；RPC 命名空间不代表 CLI 的嵌套命令层级。同一 Project 内移动 Session 只更新 Topic 归属；跨 Project 时还会按目标 `kind` 重绑 Workspace：进入 `shared` 使用目标 Project 默认路径，进入 `independent` 则保留已有独立绑定，或从 `shared` 路径复制为该 Session 的 Managed Workspace。Session 持久化目录始终不搬迁。

`project create` 默认 `--kind shared`；Shared Project 的 `--cwd` 省略时使用当前 shell 路径，Independent Project 禁止 `--cwd`。`project create --from-session` 用于由当前会话创建项目并把自己移动到新项目的未分类 Topic；创建 Shared Project 且省略 `--cwd` 时使用来源 Session 的 cwd。带 `DWO_SESSION_ID` 调用时，Host 只允许该会话移动自己；普通外部管理终端可以按 session ID 管理已有会话。

Topic 跨项目移动会迁移 Topic 文件、会话引用和同名标签；由于 Automation 配置属于 Project，带有关联 Job 的 Topic 需要先处理 Job 后再移动。移动和 Session 归类都会发布 `project.changed`。
