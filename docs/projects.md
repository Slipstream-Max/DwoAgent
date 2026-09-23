# Project、Section、Session 与 Worktree

Project 是工作目录的持久化索引。它记录项目根目录、可选名称、Section、Session 归属、Git
Repository 和 Worktree；Session 自己只保存对话与运行配置，归属关系由 Project 记录。

## 文件布局

~~~text
runtime/
|- projects/<project-id>/project.json
|- automations/<project-id>/config.yaml
|- automations/<project-id>/history.yaml
|- automations/global/config.yaml
|- automations/global/history.yaml
|- sessions/<session-id>/...
|- sessions/archive/<session-id>/...
`- workspaces/<session-id>/      # 没有 Project/cwd 时自动创建
~~~

Project 的规则文件不放在 runtime 中，而是放在项目根目录：

~~~text
<project.pwd>/AGENTS.md
~~~

它由 `project.rules.get/set` 管理。Session prompt 不再注入 Session 专属规则；模型
上下文使用 profile rules，以及当前 session cwd 向上查找得到的 `AGENTS.md`。

## project.json

~~~json
{
  "id": "project-example",
  "name": "dwoagent",
  "pwd": "C:/work/dwoagent",
  "sections": [
    {"id": "section-default", "name": "默认", "color": "#6b7280", "order": 0}
  ],
  "defaultSectionId": "section-default",
  "sessionAssignments": [
    {"sessionId": "session-example", "sectionId": "section-default", "worktreeId": null}
  ],
  "worktrees": [],
  "defaultWorktreeId": null,
  "repository": null,
  "createdAtMs": 0,
  "updatedAtMs": 0
}
~~~

`sections` 至少保留一个默认 Section。Section 只有 `id`、名称、颜色和显示顺序；它不包含额外
层级或文件。每个 Session 在一个 Project 中最多有一条 assignment，省略
`sectionId` 时使用 `defaultSectionId`。没有 assignment 的 Session 不属于任何 Project。

Project 创建时输入绝对 `pwd`，名称可省略，Host 会用目录名作为默认名称并创建默认 Section。
同一个 pwd 只能绑定一个 Project。

## Session 与工作目录

Session 的 `session.json` 不保存 `project`、`section`、`worktree` 或规则注入字段。Host 创建
Session 时根据调用参数和父 Session 解析 cwd，然后将归属写入 Project：

| 创建方式 | cwd | Project assignment |
| --- | --- | --- |
| `--project <id>` | Project root 或指定 Worktree 路径 | 写入指定 Section，省略时默认 Section |
| `--cwd <path>` | 调用者指定路径 | 不自动创建 Project，也不写 assignment |
| 既无 `--project` 也无 `--cwd` | `runtime/workspaces/<session-id>` | 无 Project |
| 子 Session 未覆盖 cwd/project | 继承父 Session 的 cwd 和 Project/Section/Worktree | 保持父 assignment |

固定 Automation Session 复用已有 Session 的 cwd 与 assignment。Project Automation 创建的新
Session 使用 Project root 或 Job 指定的 Worktree；Global Automation 可以指定任意 `cwd`，
也可以在没有 cwd 时使用 managed workspace。

## Section 与归属

~~~text
dwo section list <project-id>
dwo section create <project-id> <name>
dwo section update <project-id> <section-id> <name>
dwo section delete <project-id> <section-id>
dwo section reorder <project-id> <section-id> <position>
dwo session move <session-id> --project <project-id> --section <section-id>
~~~

不能删除默认 Section，也不能删除仍有 Session assignment 或 Automation Job 的 Section。
删除 Project 前由 Host 删除其 project.json；Session transcript 和工作目录不会被隐式移动。

## Rules

~~~text
project.rules.get { project_id }
project.rules.set { project_id, content }
~~~

读取不存在的 `AGENTS.md` 返回空内容；写入使用原子替换。Project rules 与 Session rules 是
同一个 cwd 规则体系，Session 创建不会复制、拼接或覆盖它们。

## Repository 与 Worktree

Project 可以登记 Git Repository 和 Worktree。Session assignment 的 `worktreeId` 决定该
Session 的实际 cwd：没有 worktree 时使用 `project.pwd`，有 worktree 时使用登记的 Worktree
路径。

`project.worktree.detach` 只解除登记，不删除磁盘目录；`project.worktree.remove` 会调用 Git
删除由 Host 管理的 Worktree。仍有 Session assignment 的 Worktree 不能移除，Project 的主
Worktree 也不能移除。

## Archive 与删除

Session 生命周期分为 active、archived 和 deleted：

~~~text
dwo session archive <session-id>
dwo session delete <session-id>
~~~

`archive` 将 Session 目录移动到 `runtime/sessions/archive/<session-id>`。Project assignment
保留到真正 delete 时才由 Host 清理；Archive 中的 Session 仍可查询，但不能继续执行 Prompt 或作为 Automation 的
fixed 目标。`delete` 只接受 archived Session，成功后才会真正删除 Session 文件和它拥有的
managed workspace。Project root、外部 cwd 和 Git Worktree 不会被删除。

Ephemeral Session 在 turn 结束后自动 archive，并按 Host 的清理策略自动删除；`session keep`
可以在清理前保留它。

## Automation 归属

Automation 是独立 runtime 模块。Project Job 存在 `runtime/automations/<project-id>/`，Global
Job 存在 `runtime/automations/global/`，两者都使用相同的 config/history 格式。Job 可以
指定 Section；Section 删除前必须没有引用它的 Job。具体 schema 和触发行为见
[Automation](automation.md)。
