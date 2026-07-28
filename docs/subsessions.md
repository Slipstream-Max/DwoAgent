# Subsessions 使用指南

Subsession 是由当前 agent 创建的子 session，适合把边界清楚的工作交给另一个 agent 处理。父 session 可以继续当前工作，子 session 拥有独立的上下文和 transcript；子任务结束后，daemon 会把结果自动送回父 session。

常见用途包括检查不同模块、查找资料、运行一组独立测试，或者把较长的探索工作从主对话中分开。子 session 也能继续创建自己的子 session，因此可以组成多层任务树。

```text
父 session
   |
   | dwo session prompt "检查认证模块"
   v
子 session 独立运行
   |
   | 完成 / 失败 / 取消
   v
daemon 投递 <subsession_result>
   |
   v
父 session 读取结果并继续处理
```

## 创建和继续

Agent 进程中会设置 `DWO_SESSION_ID`，daemon 用它识别当前父 session。

创建一个直接子 session：

```text
dwo session prompt "检查认证模块并列出潜在问题" --title "auth review"
```

指定工作目录、权限或模型：

```text
dwo session prompt "运行相关测试" --cwd C:\path\to\project --policy watch --model <model>
```

继续已有的直接子 session：

```text
dwo session prompt "再检查一下错误处理" --to <session-id>
```

`--title` 和 `--cwd` 只在创建时使用，不能和 `--to` 一起使用。继续子 session 时可以调整 policy、model 和 reasoning，新的设置会从后续工具调用或模型请求开始生效。

从普通终端执行命令时没有 `DWO_SESSION_ID`，此时创建的是根 session，不会挂到某个现有 session 下。

## 配置继承

创建子 session 时，下面的设置默认从父 session 继承，也可以通过命令参数覆盖：

| 设置 | 默认行为 | 覆盖参数 |
| --- | --- | --- |
| 工作目录 | 继承父 session 的 `cwd` | `--cwd <path>` |
| 权限策略 | 继承父 session 的 policy | `--policy <policy>` |
| 模型 | 继承父 session 的 model | `--model <model>` |
| Reasoning | 继承父 session 的 reasoning | `--reasoning <mode>` |

子 session 的权限不能高于父 session，权限顺序为：

```text
watch < confirm < full_access
```

例如，父 session 使用 `confirm` 时，子 session 可以使用 `confirm` 或 `watch`，不能提升到 `full_access`。这个限制也会沿多层 subsession 继续生效。

## 查看和控制

| 命令 | 用途 |
| --- | --- |
| `dwo session list` | 在 agent 内列出当前 session 的直接子 session；在普通终端列出根 session |
| `dwo session list --all` | 列出当前 profile 的全部 session |
| `dwo session watch <session-id>` | 读取子 session 最近的内容事件 |
| `dwo session cancel <session-id>` | 取消子 session 当前正在运行的 turn |
| `dwo session prompt "..." --to <session-id>` | 向已有子 session 发送后续要求 |

`watch` 是分页读取命令，默认返回最近 3 个内容事件和一个 `next_cursor`，不会持续订阅实时输出。继续读取时传入 cursor：

```text
dwo session watch <session-id> --cursor <next-cursor> --limit 10
```

任务完成通知由 daemon 自动投递。只有需要了解中间进度或排查问题时才需要使用 `watch`。

## 结果如何回到父 session

子 session 的 turn 完成、失败或被取消后，daemon 会向直接父 session 写入一条 internal message：

```xml
<subsession_result>
{
  "session_id": "...",
  "status": "completed|failed|cancelled",
  "content": "...",
  "error": null
}
</subsession_result>
```

这条消息属于内部上下文，不会伪装成用户消息，也不会产生 `UserPromptSubmitted` 事件。

| 父 session 状态 | 结果处理方式 |
| --- | --- |
| 空闲 | 结果到达后立即启动父 session 处理 |
| 正在生成回复 | 等当前 model response 完成后写入上下文 |
| 正在执行工具 | 等当前 tool-call batch 完成后写入上下文 |

多层 subsession 按父子关系逐层返回结果。最下层先把结果交给直接父 session，父 session 完成自己的工作后，再把整理后的结果交给上一层。

## 一个完整例子

主 agent 可以先创建一个代码检查任务：

```text
dwo session prompt "检查 crates/dwo-agent 的 session 生命周期，给出文件位置和风险点" --title "session review" --policy watch
```

命令返回 session ID 后，可以查看中间进度：

```text
dwo session list
dwo session watch <session-id>
```

需要补充范围时，继续同一个子 session：

```text
dwo session prompt "补充检查取消 turn 后的队列处理" --to <session-id>
```

子任务结束后，结果会自动进入父 session。父 agent 可以直接引用结果、继续验证，或与其他子任务的结果一起整理。

完整参数见 [CLI 命令参考](commands.md#session)。
