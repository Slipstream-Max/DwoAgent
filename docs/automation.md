# Automation

Automation 是一个独立的定时触发模块。它可以绑定 Project，也可以作为 Global Job 运行。
配置和运行历史不写入 project.json：

~~~text
runtime/automations/<project-id>/config.yaml
runtime/automations/<project-id>/history.yaml
runtime/automations/global/config.yaml
runtime/automations/global/history.yaml
~~~

Project Job 使用 Project root 或登记的 Worktree；Global Job 可以指定任意 cwd。Automation
本身是 trigger，执行时通过 Host 的统一 Session 创建逻辑得到 Session。

## config.yaml

~~~yaml
enabled: true
timeoutSeconds: 900
jobs:
  - name: daily-report
    enabled: true
    schedule:
      cron: "0 9 * * *"
      timezone: Asia/Shanghai
    session:
      mode: new
      behavior: every_time
      title: Daily report
    sectionId: section-default
    prompt: 检查项目状态并整理今天需要处理的事项。
    model: deepseek/deepseek-v4-pro
    reasoning: high
    policy: watch
~~~

配置使用 camelCase 并拒绝未知字段。

### 顶层字段

| 字段 | 默认值 | 作用 |
| --- | --- | --- |
| enabled | false | 是否启用当前 scope 的定时调度 |
| timeoutSeconds | 900 | 单次运行上限，范围 1..=86400 |
| jobs | [] | Job 数组，name 在当前 scope 内唯一 |

### Job 字段

| 字段 | 默认值 | 作用 |
| --- | --- | --- |
| name | 无 | ASCII 字母、数字、-、_；scope 内唯一 |
| enabled | true | 是否参加定时调度 |
| schedule | 无 | 五段 Cron 与时区 |
| session | 无 | new 或 fixed 策略 |
| prompt | 无 | 提交给 Agent 的非空内容 |
| sectionId | 无 | Project Job 的 Section；必须属于该 Project |
| cwd | 无 | Global Job 的工作目录；Project Job 不允许设置 |
| model | Profile 默认 | provider/modelId |
| reasoning | Profile 或模型默认 | 模型支持的 Reasoning |
| policy | Profile 默认 | full_access、confirm 或 watch |

Project Job 的 project scope 由 CLI 的 project 参数或调用者 Session 推导。Global Job 不需要
Project；从 Agent Session 触发时可以使用调用者的 project scope，否则按 global scope 保存。

### schedule

~~~yaml
schedule:
  cron: "0 9 * * *"
  timezone: Asia/Shanghai
~~~

cron 必须是五段：minute hour day month weekday。timezone 默认 local，也可用 IANA 时区。
调度器保存和比较 UTC 时间。

## Session 行为

每次都创建新 Session：

~~~yaml
session:
  mode: new
  behavior: every_time
~~~

第一次创建，之后复用同一个 Session：

~~~yaml
session:
  mode: new
  behavior: once
~~~

始终投递到已有 Session：

~~~yaml
session:
  mode: fixed
  sessionId: session-existing
~~~

| 配置 | 行为 |
| --- | --- |
| new + every_time | 每次 trigger 创建新 Session |
| new + once | 第一次创建，后续从 history.yaml 复用绑定的 Session |
| fixed | 始终向指定 Session 投递 |

new 的 title 可省略；默认使用 automation/<job-name>。fixed 不接受 behavior、title、cwd 或
sectionId，它继承目标 Session 的 cwd、Project、Section 和 Worktree。目标 Session 必须存在且
未 archived。Project Job 的 new Session 使用 Project root 或目标 Worktree；Global Job 的
new Session 使用 cwd，未提供 cwd 时使用 managed workspace。

同一个繁忙 Session 的多个 Job 按 Session 串行排队。Automation 不修改 Session 的
session.json，也不向 prompt 注入额外规则。

## 权限和失败

Automation 按无人值守方式运行。需要交互确认的 Tool Permission 会自动拒绝，不会无限等待。
timeoutSeconds 到达后，运行标记为 failed 并取消当前 Turn。new + once 和 fixed 已绑定
Session 后，实际模型与权限以 Session 当前配置为准。

## history.yaml

~~~yaml
runs:
  - runId: run-example
    projectId: project-example
    job: daily-report
    sessionId: session-example
    turnId: turn-example
    status: completed
    scheduled: true
    startedAt: "2026-08-31T01:00:00Z"
    finishedAt: "2026-08-31T01:00:42Z"
    response: 已完成检查。
    error: null
    finishReason: end_turn
onceSessions:
  daily-report: session-example
~~~

History 最多保留 100 次运行。daemon 重启时会把遗留的 queued/running 记录标记为中断状态。
Global history 的 projectId 为空；Project history 的 projectId 为所属 Project。

## CLI

~~~text
dwo automation --project <id> list [--json]
dwo automation --project <id> add <name> --cron <expr> --prompt <text> [options]
dwo automation --project <id> enable <job|--all>
dwo automation --project <id> disable <job|--all>
dwo automation --project <id> delete <job|--all> [--yes]
dwo automation --project <id> run <job> [--json]
dwo automation --global list [--json]
dwo automation --global add <name> --cron <expr> --prompt <text> [--cwd <path>]
~~~

Project scope 使用 project 参数；Agent Session 内省略时，Host 从 caller session 的
assignment 推导。Global scope 使用 global 参数。run 在解析 Session 并成功排队后返回，不等待
Agent 完成；从 Agent Session 发起时，最终结果会作为内部 automation_result 消息返回调用方。

排查时检查 scope、Job 名称、五段 Cron、时区、Section、固定 Session 和权限，然后查看
对应 scope 的 history.yaml 与 Host 日志。
