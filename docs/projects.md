# Project 文件与行为

Project 是工作范围和看板的持久化单位。它保存默认工作路径、Repository/Worktree、Section、
Topic、Label 和 Session 归属；Session 本身仍由 Session repository 保存。

本文件同时说明磁盘字段和运行行为。CLI 语法见 [CLI 命令参考](cli.md)，Session 创建与
子 Agent 见 [Session 与子 Agent](session.md)，Project 下的定时任务见
[Automation](automation.md)。

## 目录结构

每个 Project 的目录为：

~~~text
runtime/projects/<project-id>/
|- project.json
|- automation/
|  |- config.yaml
|  `- history.yaml
`- topics/
   `- <topic-id>/
      |- overview.md
      `- AGENTS.md
~~~

Project 目录不存放工作文件。Managed workspace 在 runtime/workspaces/<session-id>/；
Shared Project 的工作文件位于 Project.pwd 或登记的 Git Worktree。

## project.json 完整结构

下面是包含所有可选字段的示例。实际序列化时，空的 repository、worktrees、defaultWorktreeId、
sessionIds、labelIds 和 description 会省略。

~~~json
{
  "id": "project-demo",
  "name": "Dwo Agent",
  "kind": "shared",
  "pwd": "C:\\repo\\dwoagent",
  "repository": {
    "root": "C:\\repo\\dwoagent",
    "commonDir": "C:\\repo\\dwoagent\\.git",
    "remoteUrl": "https://github.com/example/dwoagent.git"
  },
  "worktrees": [
    {
      "id": "worktree-main",
      "name": "main",
      "path": "C:\\repo\\dwoagent",
      "source": "primary",
      "createdAtMs": 1788100000000
    }
  ],
  "defaultWorktreeId": "worktree-main",
  "board": {
    "uncategorizedSectionId": "section-inbox",
    "uncategorizedTopicId": "topic-inbox",
    "sections": [
      {
        "id": "section-inbox",
        "name": "未分类",
        "order": 0
      }
    ],
    "topics": [
      {
        "id": "topic-inbox",
        "sectionId": "section-inbox",
        "title": "未分类",
        "order": 0,
        "sessionIds": ["session-example"],
        "labelIds": ["label-backend"]
      }
    ],
    "labels": [
      {
        "id": "label-backend",
        "name": "Backend",
        "color": "#388E3C",
        "description": "Backend work"
      }
    ]
  },
  "createdAtMs": 1788100000000,
  "updatedAtMs": 1788100000000
}
~~~

project.json 使用 camelCase；顶层未知字段会被拒绝。Dwo 写入这个文件；日常操作应使用 CLI
或 API，不要直接编辑。

## Project 字段

| 字段 | 必填 | 约束与含义 |
| --- | --- | --- |
| id | 是 | Project 唯一 ID，不能为空 |
| name | 是 | 显示名称，不能为空 |
| kind | 是 | shared 或 independent |
| pwd | shared 必填 | 绝对路径；independent 禁止出现 |
| repository | 否 | Git repository 记录；仅 shared 可用 |
| worktrees | 否，默认 [] | 已登记 Worktree；仅 shared 可用 |
| defaultWorktreeId | 否 | 必须引用 worktrees 中存在的 ID |
| board | 是 | Section、Topic、Label 和未分类 ID |
| createdAtMs | 是 | Unix 毫秒时间戳 |
| updatedAtMs | 是 | 最近一次 Project/Board 更新的 Unix 毫秒时间戳 |

Project 类型：

| kind | 用途 | Workspace 规则 |
| --- | --- | --- |
| shared | 多个 Session 处理同一个仓库 | 必须有 pwd，可登记 Repository 和 Worktree |
| independent | 相互隔离的任务 | 没有 pwd、Repository 或 Worktree；Session 使用 Managed 或 External |

固定 Project project-unassigned 名为“未分配会话”，始终是 independent。没有显式
projectId 的新 Session 都会进入它。

## repository 字段

| 字段 | 必填 | 作用 |
| --- | --- | --- |
| root | 是 | 当前主工作树的绝对根目录 |
| commonDir | 是 | Git common directory 的绝对路径 |
| remoteUrl | 否 | Repository 的远端 URL |

设置 repository 时会同时登记 primary Worktree，并把 Project.pwd 指向它。没有 repository
时 worktrees 必须为空。

## worktrees 字段

每项 Worktree：

| 字段 | 必填 | 约束与含义 |
| --- | --- | --- |
| id | 是 | Project 内唯一，不能为空 |
| name | 是 | 显示名称，不能为空 |
| path | 是 | 绝对路径；Project 内不能重复 |
| source | 是 | primary、managed 或 external |
| createdAtMs | 是 | 登记时间，Unix 毫秒 |

source 表示路径由哪里来：primary 是 Repository 的主工作树，managed 是 Dwo 创建的 Worktree，
external 是已有路径的登记记录。删除或 detach 的磁盘行为由对应 CLI 命令决定，不能只根据
source 猜测。

## board 字段

| 字段 | 必填 | 约束与含义 |
| --- | --- | --- |
| uncategorizedSectionId | 是 | 必须引用 sections 中存在的 Section |
| uncategorizedTopicId | 是 | 必须引用 topics 中存在的 Topic |
| sections | 是 | Section 数组，ID 不可重复 |
| topics | 是 | Topic 数组，ID 不可重复 |
| labels | 是 | Label 数组，ID 不可重复 |

创建 Project 时自动创建“未分类” Section 和 Topic。它们不能删除。删除普通 Topic 时，关联
Session 和 Automation Job 会移入未分类 Topic。

## Section 字段

| 字段 | 必填 | 作用 |
| --- | --- | --- |
| id | 是 | Project 内唯一 Section ID |
| name | 是 | 看板分区名 |
| order | 是 | 同一 Board 中的显示顺序，从 0 开始 |

Section 只负责看板分组，不直接保存 Topic ID；Topic 用 sectionId 指回所属 Section。移动和
重排后 Dwo 会重新规范化 order。

## Topic 字段

| 字段 | 必填 | 默认 | 约束与含义 |
| --- | --- | --- | --- |
| id | 是 | 无 | Project 内唯一 Topic ID |
| sectionId | 是 | 无 | 必须引用当前 Board 中存在的 Section |
| title | 是 | 无 | Topic 标题 |
| order | 是 | 无 | 所属 Section 内的显示顺序 |
| sessionIds | 否 | [] | 归属于该 Topic 的 Session ID |
| labelIds | 否 | [] | 必须引用当前 Board 中存在的 Label |

一个 Session 在整个 Host 中只归属于一个 Topic。assign 时 Dwo 会先从原 Topic 移除，再加入
目标 Topic。Topic 跨 Project 移动会迁移 overview.md、AGENTS.md、Session 引用和同名 Label；
有关联 Automation Job 时必须先处理 Job。

## Label 字段

| 字段 | 必填 | 作用 |
| --- | --- | --- |
| id | 是 | Project 内唯一 Label ID |
| name | 是 | 非空显示名称 |
| color | 是 | 非空颜色字符串；通常使用 #RRGGBB |
| description | 否 | 空字符串会规范化为省略 |

删除 Label 会同时清理所有 Topic 的对应 labelIds。

## Topic 文件

每个 Topic 还有两个不放在 project.json 中的 Markdown 文件：

| 文件 | 作用 |
| --- | --- |
| overview.md | 给用户和客户端显示的概述、背景或计划 |
| AGENTS.md | Topic Knowledge，作为规则注入关联 Session |

AGENTS.md 以 Session 当前 cwd 作为适用目录。新建 Session、上下文压缩和资源 watcher 都使用
同一规则源；移动 Session 到另一个 Topic 后，下一个 model step 使用新 Topic 的 Knowledge。

## Automation 文件

Project 的定时任务单独保存在：

| 文件 | 作用 |
| --- | --- |
| automation/config.yaml | Project 的 scheduler 开关、超时和 Job 定义 |
| automation/history.yaml | 最近运行、new + once 的 Session 绑定 |

Automation 字段不写入 project.json。Job 通过 topicId 引用 Topic。完整 schema 和行为见
[Automation](automation.md)。

## Session 文件和 Project 的关系

Session repository 使用按创建日期分层的目录：

~~~text
runtime/sessions/YYYY/MM/DD/<session-id>/
|- session.json
|- model_context.json
`- client_transcript.jsonl
~~~

session.json 保存 Session metadata，不保存解析后的 cwd。主要字段：

| 字段 | 作用 |
| --- | --- |
| info.id | Session ID |
| info.parentSessionId | 可选父 Session，即子 Agent 关系 |
| info.title | 标题 |
| info.workspace | project_default、worktree、managed 或 external 绑定 |
| info.mode | full_access、confirm 或 watch |
| info.createdAtMs / updatedAtMs | Unix 毫秒时间戳 |
| info.ephemeral / completed / deleteAfterMs | 临时 Session 生命周期 |
| llm.model | 当前 provider/modelId |
| llm.reasoning | 当前 reasoning |
| llm.reasoning_by_model | 每个模型最近一次 reasoning 选择 |
| current_plan | 可选执行计划 |

Topic 归属不在 session.json，而在 project.json 的 topic.sessionIds 中。model_context.json 是
当前发给模型的上下文；client_transcript.jsonl 是追加式完整记录，压缩只重建前者。

## Workspace 解析

Session 的 info.workspace 使用 tagged JSON：

~~~json
{"kind": "project_default"}
{"kind": "worktree", "worktreeId": "worktree-main"}
{"kind": "managed"}
{"kind": "external", "pwd": "C:\\repo\\other"}
~~~

Host 加载时解析实际 cwd：

| kind | cwd |
| --- | --- |
| project_default | Project.pwd |
| worktree | Project.worktrees 中对应记录的 path |
| managed | runtime/workspaces/<session-id>/ |
| external | workspace.pwd |

只有 Managed workspace 由 Session 生命周期删除。Project 默认路径、Worktree 和 External 路径
都不会随 Session 删除。

跨 Project 移动时，进入 shared 会改用目标 Project 默认路径；从 shared 进入 independent 会
复制当前工作内容到 Managed workspace；两个 independent Project 之间保留原有 Managed 或
External 绑定。Session repository 目录不会搬迁。

## CLI

~~~text
dwo project list
dwo project get <project-id>
dwo project create <name> [--kind shared|independent] [--cwd <path>]
dwo project repository get|clone|attach ...
dwo project worktree list|get|create|attach|rename|detach|remove ...
dwo section list|create|update|delete|reorder ...
dwo topic list|get|create|update|delete|move|reorder ...
dwo session move <session-id> --project <project-id> --topic <topic-id>
~~~

完整参数见 [CLI 命令参考](cli.md)。桌面客户端的 project.* API 见 [API 说明](api.md)。
