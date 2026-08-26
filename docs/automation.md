# Automation 使用指南

Automation 用 cron 定时向 Agent 提交 prompt。任务由 daemon 调度，可以每次创建新 session、首次创建后持续复用，或投递到显式指定的固定 session。

## 快速配置

编辑 `~/.dwoagent/profile.yaml`：

```yaml
automation:
  enabled: true
  jobs:
    - name: daily-report
      enabled: true
      schedule:
        cron: "0 9 * * *"
        timezone: Asia/Shanghai
      session:
        mode: new
        behavior: every_time
        cwd: projects/demo
        title: Daily report
      prompt: 检查项目状态并整理今天需要处理的事项。
```

Daemon 会监听 `profile.yaml`。配置通过校验后，执行时间会自动更新。

## 配置字段

| 字段 | 说明 |
| --- | --- |
| `automation.enabled` | 控制定时调度，默认 `false`。 |
| `jobs[].name` | 任务名，只能包含 ASCII 字母、数字、`-` 和 `_`，并且不能重复。 |
| `jobs[].enabled` | 控制单个任务的定时调度，默认 `true`。 |
| `schedule.cron` | 五段 cron：分钟、小时、日期、月份、星期。 |
| `schedule.timezone` | IANA 时区名称，例如 `Asia/Shanghai`；默认 `local`。 |
| `session` | 选择 `new` 或 `fixed` 模式。`new` 必须设置 `behavior`。 |
| `prompt` | 提交给 Agent 的内容，不能为空。 |

六段 cron 会被拒绝。下面是有效例子：

```text
0 9 * * *       每天 09:00
*/30 * * * *    每 30 分钟
0 18 * * 1-5    工作日 18:00
```

## 新建 Session

`mode: new` 根据必填的 `behavior` 决定创建策略：

```yaml
session:
  mode: new
  behavior: every_time
  cwd: projects/demo
  title: Daily report
```

`behavior: every_time` 每次运行创建独立 session。`behavior: once` 第一次运行时创建并由
AutomationRuntime 在自己的持久化状态中保存 job 到 session 的绑定；daemon 重启后按该绑定
继续复用。SessionRecord 不保存 automation owner 字段。若绑定的 session 已删除，下一次运行
会重新创建并更新绑定。

相对 `cwd` 从 profile 根目录 `~/.dwoagent/` 开始解析，默认值是 `.`。没有设置 `title` 时，session 名称使用 `automation/<job-name>`。模型和权限模式使用 profile 默认值。`behavior` 没有默认值，省略会使 profile 校验失败。

`every_time` 适合互相独立的日报、巡检和一次性整理任务；`once` 适合需要持续积累上下文、但不想手动管理 session ID 的任务。结果都保存在所使用 session 的 transcript 中。

## 固定 Session

`mode: fixed` 把 prompt 投递给已有 session：

```yaml
session:
  mode: fixed
  sessionId: session-replace-me
```

这种模式适合持续跟进同一个项目。Session 正在运行时，Automation prompt 会进入 FIFO 队列，并在模型响应或工具调用的边界加入当前 turn。

可以先创建 session，再从以下命令获取 ID：

```text
dwo session list --all
```

## 查看与手动运行

```text
dwo automation list
dwo automation status <job>
dwo automation add <job> --cron <expr> --prompt <text>
dwo automation enable <job|--all>
dwo automation disable <job|--all>
dwo automation delete <job>
dwo automation delete --all --yes
dwo automation run <job>
```

`list` 显示任务总数、启用/禁用数量、运行数量和每个任务的下一次执行时间。`status <job>` 显示完整任务配置、当前有效模型、sticky/fixed session、active runs 和最近 10 次 run。最近回答会折叠空白并限制在 100 个字符内；完整回答保存在对应 session transcript 中。

`add` 默认使用 `--session every-time`。可改用 `--session once`，或者使用 `--session fixed --session-id <id>`。`enable` 和 `disable` 接受单个任务名或 `--all`。CLI 会原子更新并校验 `profile.yaml`，daemon 随即热加载；任务定义不存在第二套 runtime 配置源。

`run` 会等待 session 创建或解析以及 prompt 提交成功，然后返回 `run_id`、`session_id` 和 `turn_id`，但不会等待 Agent 完成。启动阶段失败会直接使命令报错，不会先返回一个无法执行的 `running` 记录。

当 `run` 由 Agent session 内的终端调用时，CLI 会自动携带当前 session ID。任务完成、失败或取消后，daemon 会把 `<automation_result>` 内部消息投递回调用方上下文；调用方不需要轮询。外部 shell 没有调用方 session，因此只负责异步启动任务，可用 `automation status` 查看仍在运行的任务。

需要机器读取时可以加 `--json`：

```text
dwo automation status daily-report --json
dwo automation run daily-report --json
```

job 的 `enabled` 控制定时调度。为了兼容已有 profile，底层仍读取 `automation.enabled`；CLI 的 `add` 和 `enable` 会自动打开该兼容开关。手动 `run` 始终可以执行 disabled 任务，方便启用 schedule 前测试。

## 权限与记录

Automation 按无人值守方式运行。出现工具权限确认时，daemon 会自动拒绝该请求，任务可以继续处理拒绝结果，不会一直等待人工输入。

需要写文件或执行命令的任务，应提前选择合适的 profile 默认 policy，并确认对应命令能按该 policy 执行。`watch` 只允许简单只读命令；`confirm` 中需要确认的操作会被拒绝；`full_access` 仍会应用显式 deny rule。

Automation 不复制完整 session 历史。运行结果保存在所使用 session 中；`new + once` 的归属标记保存在该 session 自己的 metadata 中，不再维护单独的 binding 文件。最近 100 次 run 的状态、session/turn ID 和最多 100 字符的回答预览保存在 `runtime/automation-runs.yaml`，用于 `automation status`，不会无限增长。

## 排查

1. 运行 `dwo automation list`，再用 `dwo automation status <job>` 检查任务和下一次执行时间。
2. 用 `dwo automation run <job>` 手动启动测试，再检查对应 session 或运行状态。
3. 确认 cron 只有五个字段，时区名称有效。
4. 新建模式检查 `cwd` 是否存在；固定模式检查 `sessionId` 是否存在。
5. 检查 profile 默认 policy 是否允许任务所需的工具。
6. 查看 `~/.dwoagent/logs/`。
