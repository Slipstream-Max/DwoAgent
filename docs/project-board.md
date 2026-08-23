# Project 与看板

Project 是 Host 中工作目录和看板的聚合根。看板用于把持续工作组织成分区和话题；它不替代 Session 或 Automation，而是保存对这些对象的 ID 引用。

## 结构

```text
Project
|- id / name / pwd
`- Board
   |- Sections
   |- Topics
   `- Labels

Topic
|- sectionId / title / order
|- sessionIds[]
|- taskIds[]
|- labelIds[]
|- overview.md
`- AGENTS.md
```

关系只由 Topic 保存：Session 不含 `topicId`，Automation Job 也不含 `topicId`。Host 根据 Topic 中的 ID 向 AgentService 和 Automation 查询详情。

每个 Project 创建时都有一个“未分类”分区和话题。调用 `session.new` 时可以省略 `topic_id`，Host 会使用该 Project 的未分类话题；完全省略 `project_id` 时，Host 先根据 `cwd` 创建 Project，没有 `cwd` 则生成 Project workspace。

## 持久化

```text
runtime/projects/<project-id>/
|- project.json
|- workspace/                 # 创建 Project 时没有提供 pwd 才生成
`- topics/<topic-id>/
   |- overview.md
   `- AGENTS.md
```

`project.json` 保存 Board 元数据和 ID 关联。Project 的 workspace 不属于某个 Session，因此删除 Session 不删除 Project workspace。

## Topic Knowledge

Desktop 将 Topic 的 `AGENTS.md` 显示为 Knowledge。Host 创建 Topic Session 时，将该文件注册为额外 `RuleSource`：

```text
RuleSource
|- path = runtime/projects/<project-id>/topics/<topic-id>/AGENTS.md
`- pwd  = Project.pwd
```

ContextManager 自己读取文件。初始 prompt、上下文压缩重建和环境 watcher 使用同一规则源；Host 不读取内容后拼接 prompt。每份规则快照都带 `source`、`pwd` 和内容。未分类话题的空 `AGENTS.md` 不增加规则，但文件以后写入内容时 watcher 仍会发现。

移动 Session 到另一个 Topic 时，Host 同时更新 Topic 的 `sessionIds` 和 Session 的通用 RuleSource。该操作要求 Session 当前 idle，避免正在运行的 turn 被中途换规则。

## Topic 详情

`project.topic.get` 由 Host 组合：

```text
dwo-project       -> Topic、Markdown、Label
AgentService      -> sessionIds 对应的 Session 状态
AutomationRuntime -> taskIds 对应的任务状态
```

Topic 中新建自动任务时，Host 先通过现有 Automation 配置创建 Job，再把 Job name 作为 `taskId` 关联到 Topic。全局 Tasks 仍读取全部 Automation Job；Topic 详情只显示自己的 `taskIds`。

Topic Task 创建或选择 Session 执行时，AutomationRuntime 根据 Topic 的 `taskIds` 找回 Project 和 Topic，使执行 Session 使用 `Project.pwd`、加入 `Topic.sessionIds`，并加载同一份 Topic `AGENTS.md`。Automation Job 自身仍不保存 `topicId`。

## Inbox

Inbox 没有后端实体。`session.status-list` 和 `session.status` 返回运行阶段，以及最近一次终态的 `lastTurnStatus`、`lastTurnFinishedAtMs`。Desktop 按终态和完成时间筛选、排序即可生成 Inbox。

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
| `project.topic.task.create/assign/unassign` | 自动任务创建和归类 |
| `project.label.create/update/delete/assign/unassign` | 看板标签管理 |

所有看板变更发布 `project.changed`。删除 Label 会清理所有 Topic 的对应 `labelId`；删除普通 Topic 会把 Session 和 Task 移回未分类话题。
