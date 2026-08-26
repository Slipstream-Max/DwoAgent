# Project 与看板

Project 是 Host 中工作目录、看板和 Automation 的聚合根。看板用于把持续工作组织成分区和话题；它不替代 Session，Session 通过 Topic 归类。

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
|- labelIds[]
|- overview.md
`- AGENTS.md
```

Session 归属由 Topic 的 `sessionIds[]` 保存；Automation Job 自己保存可选的 `topicId`，因为任务配置和运行历史属于 Project 的 automation 目录。

每个 Project 创建时都有一个“未分类”分区和话题。调用 `session.new` 时可以省略 `topic_id`，Host 会使用该 Project 的未分类话题；完全省略 `project_id` 时，Host 按 canonical cwd 查找已有 Project，找不到才创建，没有 `cwd` 则生成 Project workspace。显式 `project.create` 不允许两个 Project 使用同一个 canonical pwd。

ACP 的标准 `new_session` 只有 cwd，没有 Project/Topic 字段。ACP adapter 只需把 cwd 转发给 `session.new`；上述 Host 解析会复用对应 Project，并把新 Session 放入未分类话题。ACP fork 继续继承源 Session 的 Topic。

## 持久化

```text
runtime/projects/<project-id>/
|- project.json
|- workspace/                 # 创建 Project 时没有提供 pwd 才生成
|- automation/
|  |- config.yaml
|  `- history.yaml
`- topics/<topic-id>/
   |- overview.md
   `- AGENTS.md
```

`project.json` 保存 Board 元数据和 Session 关联。Project 的 workspace、Automation 配置和运行历史不属于某个 Session，因此删除 Session 不删除这些 Project 资源。

## Topic Knowledge

Desktop 将 Topic 的 `AGENTS.md` 显示为 Knowledge。Host 创建 Topic Session 时，将该文件注册到
SessionService 的 per-session external rule file registry：

```text
ExternalRuleFile
|- path = runtime/projects/<project-id>/topics/<topic-id>/AGENTS.md
`- pwd  = Project.pwd
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

Automation 执行 Session 时使用 `Project.pwd`、加入 `Topic.sessionIds`，并加载同一份 Topic `AGENTS.md`。删除 Topic 会把关联 Session 和 Job 移回未分类 Topic。

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
