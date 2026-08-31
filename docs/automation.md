# Automation

Automation 是 Project 内的定时任务。每个 Project 有独立配置和历史，Host 只运行一个调度器：

~~~text
runtime/projects/<project-id>/automation/
|- config.yaml
`- history.yaml
~~~

Automation 不属于 profile.yaml。CLI 命令见 [CLI 命令参考](cli.md)，Project 和 Topic 字段见
[Project 文件与行为](projects.md)。

## config.yaml 完整结构

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
    topicId: topic-daily
    prompt: 检查项目状态并整理今天需要处理的事项。
    model: deepseek/deepseek-v4-pro
    reasoning: high
    policy: watch
~~~

配置使用 camelCase 并拒绝未知字段。

### 顶层字段

| 字段 | 必填 | 默认值 | 约束与作用 |
| --- | --- | --- | --- |
| enabled | 否 | false | 是否启用当前 Project 的定时调度 |
| timeoutSeconds | 否 | 900 | 单次运行上限，范围 1..=86400 |
| jobs | 否 | [] | Job 数组，name 在 Project 内唯一 |

disabled Project 仍可通过 automation run 手动执行 Job。

### Job 字段

| 字段 | 必填 | 默认值 | 约束与作用 |
| --- | --- | --- | --- |
| name | 是 | 无 | 仅 ASCII 字母、数字、- 和 _；Project 内唯一 |
| enabled | 否 | true | 是否参加定时调度 |
| schedule | 是 | 无 | Cron 和时区 |
| session | 是 | 无 | new 或 fixed 策略 |
| prompt | 是 | 无 | 提交给 Agent 的非空内容 |
| topicId | 否 | 未分类 Topic | 必须引用当前 Project 的 Topic |
| model | 否 | Profile 默认 | provider/modelId |
| reasoning | 否 | Profile 或模型默认 | 模型支持的 Reasoning |
| policy | 否 | Profile 默认 | full_access、confirm 或 watch |

### schedule 字段

~~~yaml
schedule:
  cron: "0 9 * * *"
  timezone: Asia/Shanghai
~~~

cron 必须是五段：minute hour day month weekday。六段表达式会拒绝。timezone 默认 local，也可用
IANA 时区，例如 Asia/Shanghai、Europe/Berlin。调度器保存和比较 UTC 时间。

### session 字段

每次新建 Session：

~~~yaml
session:
  mode: new
  behavior: every_time
  title: Daily report
~~~

首次创建后持续复用：

~~~yaml
session:
  mode: new
  behavior: once
  title: Daily report
~~~

投递到固定 Session：

~~~yaml
session:
  mode: fixed
  sessionId: session-existing
~~~

| 模式 | 行为 |
| --- | --- |
| new + every_time | 每次运行创建新的 Session |
| new + once | 第一次创建，以后复用 history.yaml 中绑定的 Session |
| fixed | 始终投递到指定 Session，Session ID 必须有效 |

new 的 title 可省略；默认使用 automation/<job-name>。fixed 不接受 behavior 或 title。

## Project、Topic 和 Workspace

Automation 创建的 Session 归入当前 Project 和 Job 的 topicId。Shared Project 使用
Project.pwd；Independent Project 分配独立 Managed Workspace。Session 加载相同 Topic 的
AGENTS.md。

fixed 保留已有 Session 的 Workspace，但运行时会把 Session 归入 Job 指定 Topic。多个 Job
投递到同一个繁忙 Session 时，AutomationRuntime 按 Session 串行排队。

## 权限和无人值守

Automation 按无人值守方式运行。需要交互确认的 Tool Permission 会自动拒绝，不会无限等待。
因此定时写操作应使用经过评估的 full_access，或为目标命令配置明确规则；只读巡检优先 watch。

Job 未设置 model、reasoning 或 policy 时，使用当前 Profile 默认值。new + once 和 fixed 已绑定
Session 后，实际模型以 Session 当前配置为准。

timeoutSeconds 到达后，运行标记为 Failed 并取消当前 Turn。

## history.yaml

history.yaml 由 daemon 写入：

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

Run 字段：

| 字段 | 作用 |
| --- | --- |
| runId | Automation Run ID |
| projectId / job | 所属 Project 和 Job |
| sessionId / turnId | 实际执行对象，排队前可为空 |
| status | queued、running、completed、failed 或 cancelled |
| scheduled | true 表示 Cron 触发，false 表示手动 run |
| startedAt / finishedAt | RFC 3339 时间 |
| response | 最终回答预览，规范化后最多 100 字符 |
| error | 可选错误 |
| finishReason | 可选 Turn 结束原因 |
| onceSessions | new + once 的 Job 名到 Session ID 绑定 |

History 最多保留 100 次运行。daemon 重启时会把遗留的 queued/running 记录标记为中断状态。

## CLI

~~~text
dwo automation --project <id> list [--json]
dwo automation --project <id> status <job> [--json]
dwo automation --project <id> add <name> --cron <expr> --prompt <text> [options]
dwo automation --project <id> enable <job|--all>
dwo automation --project <id> disable <job|--all>
dwo automation --project <id> delete <job>
dwo automation --project <id> delete --all --yes
dwo automation --project <id> run <job> [--json]
~~~

外部 Shell 必须传 --project。Agent Session 内省略时，Host 从 DWO_SESSION_ID 找到当前 Project。
add 默认创建已启用的 new + every_time Job。run 在创建/解析 Session 并成功排队后返回，不等待
Agent 完成；从 Agent Session 发起时，最终结果会作为内部 automation_result 消息返回调用方。

## 排查

依次检查 Project ID、topicId、五段 Cron、时区、模型、固定 Session 和权限。然后查看
history.yaml、dwo automation status <job> 和 ~/.dwoagent/logs/。
