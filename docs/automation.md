# Automation 使用指南

Automation 用 cron 定时向 Agent 提交 prompt。任务由 daemon 调度，可以为每次运行创建新 session，也可以持续使用一个固定 session。

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
| `session` | 选择 `new` 或 `fixed` 模式。 |
| `prompt` | 提交给 Agent 的内容，不能为空。 |

六段 cron 会被拒绝。下面是有效例子：

```text
0 9 * * *       每天 09:00
*/30 * * * *    每 30 分钟
0 18 * * 1-5    工作日 18:00
```

## 新建 Session

`mode: new` 每次运行创建独立 session：

```yaml
session:
  mode: new
  cwd: projects/demo
  title: Daily report
```

相对 `cwd` 从 profile 根目录 `~/.dwoagent/` 开始解析，默认值是 `.`。没有设置 `title` 时，session 名称使用 `automation/<job-name>`。模型和权限模式使用 profile 默认值。

这种模式适合日报、巡检和一次性整理任务。每次运行的上下文互不影响，结果保存在新 session 的 transcript 中。

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
dwo automation status
dwo automation run <job>
```

`list` 和 `status` 显示任务配置、下一次执行时间和当前运行记录。`run` 立即执行指定任务并等待结果，方便测试 prompt、工作目录和权限。

需要机器读取时可以加 `--json`：

```text
dwo automation status --json
dwo automation run daily-report --json
```

`automation.enabled` 和 job 的 `enabled` 控制定时调度。手动 `run` 仍可执行已经写在配置中的任务，方便在启用 schedule 前进行测试。

## 权限与记录

Automation 按无人值守方式运行。出现工具权限确认时，daemon 会自动拒绝该请求，任务可以继续处理拒绝结果，不会一直等待人工输入。

需要写文件或执行命令的任务，应提前选择合适的 profile 默认 policy，并确认对应命令能按该 policy 执行。`watch` 只允许简单只读命令；`confirm` 中需要确认的操作会被拒绝；`full_access` 仍会应用显式 deny rule。

Automation 不使用单独的历史目录。新建模式的结果保存在新 session 中，固定模式的结果保存在目标 session 中。当前运行状态保存在 daemon 内存里，daemon 重启后不会保留已完成的 run record。

## 排查

1. 运行 `dwo automation status`，检查任务和下一次执行时间。
2. 用 `dwo automation run <job>` 手动测试。
3. 确认 cron 只有五个字段，时区名称有效。
4. 新建模式检查 `cwd` 是否存在；固定模式检查 `sessionId` 是否存在。
5. 检查 profile 默认 policy 是否允许任务所需的工具。
6. 查看 `~/.dwoagent/runtime/logs/`。
