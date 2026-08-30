# Automation 使用指南

Automation 是 Project 内的定时任务。每个 Job 必须属于一个 Project，可以进一步指定
Topic；Host 只保留一个 `AutomationRuntime`，统一负责调度、队列、运行状态和 Session 执行。

## 存储

```text
runtime/projects/<project-id>/automation/
|- config.yaml
`- history.yaml
```

`config.yaml` 是该 Project 的任务配置，`history.yaml` 保存最近 100 次运行状态、Session/Turn
ID、`new + once` 的 Session 绑定和最多 100 字符的回答预览。不存在全局
`runtime/automation-runs.yaml`，Automation 也不再属于 `profile.yaml`。

配置结构如下：

```yaml
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
    topicId: topic-replace-me
    prompt: 检查项目状态并整理今天需要处理的事项。
```

| 字段 | 说明 |
| --- | --- |
| `enabled` | 控制该 Project 的定时调度，默认 `false`。 |
| `timeoutSeconds` | 单次运行上限，默认 `900`，范围 `1..=86400`。 |
| `jobs[].name` | Project 内唯一，只能包含 ASCII 字母、数字、`-` 和 `_`。 |
| `jobs[].enabled` | 控制单个 Job 的定时调度，默认 `true`。 |
| `jobs[].schedule.cron` | 五段 cron：分钟、小时、日期、月份、星期。 |
| `jobs[].schedule.timezone` | IANA 时区或 `local`，默认 `local`。 |
| `jobs[].topicId` | 可选；省略时使用 Project 的未分类 Topic。 |
| `jobs[].session` | `new` 或 `fixed`；`new` 必须设置 `behavior`。 |
| `jobs[].prompt` | 每次触发时提交给 Agent 的内容，不能为空。 |

六段 cron 会被拒绝。`topicId` 不属于当前 Project 也会被拒绝。

## Session 模式

`new + every_time` 每次运行创建独立 Session；`new + once` 首次创建后持续复用，绑定记录在
当前 Project 的 `history.yaml`。Automation 创建的 Session 在 Project 有显式 `pwd` 时使用该
路径，否则分配独立的 DWO Workspace；随后加入 Job 指定的 Topic，并加载该 Topic 的
`AGENTS.md`。Automation 不单独接受 cwd。

```yaml
session:
  mode: new
  behavior: once
  title: Daily report
```

`fixed` 投递到已有 Session，执行时会归入 Job 指定的 Topic，并保留该 Session 的 Workspace。
Session 忙时，任务在 AutomationRuntime 内按 Session 串行排队。

```yaml
session:
  mode: fixed
  sessionId: session-replace-me
```

## CLI

外部 shell 必须用 `--project <id>` 指定 Project。Agent Session 内省略 `--project` 时，CLI
根据 `DWO_SESSION_ID` 找到当前 Session 所属 Project。

```text
dwo automation --project <id> list [--json]
dwo automation --project <id> status <job> [--json]
dwo automation --project <id> add <job> --cron <expr> --prompt <text> [--topic <id>] [--session every-time|once|fixed] [--session-id <id>] [--title <title>] [--disabled] [--json]
dwo automation --project <id> enable <job|--all>
dwo automation --project <id> disable <job|--all>
dwo automation --project <id> delete <job>
dwo automation --project <id> delete --all --yes
dwo automation --project <id> run <job> [--json]
```

`add` 默认创建启用的 `new + every_time` Job，并原子更新 Project 的 `config.yaml`。手动
`run` 可以执行 disabled Job。为了防止 Agent 删除自己所在 Project 的任务，存在
`DWO_SESSION_ID` 时 CLI 拒绝 `automation delete`。

`run` 在 Session 创建或解析并成功排队后返回，不等待 Agent 完成。由 Agent Session 发起时，
最终结果或错误会作为内部 `<automation_result>` 消息送回调用方上下文。

## 权限与排查

Automation 按无人值守方式运行；工具权限确认会自动拒绝。模型、reasoning 和 policy 没有在
Job 中覆盖时，使用当前 profile 默认值。

排查时依次检查 Project ID、Job 的 `topicId`、五段 cron、时区和固定 Session 是否存在，
再查看 `runtime/projects/<project-id>/automation/history.yaml` 与 daemon 日志。
