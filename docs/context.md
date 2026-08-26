# 上下文压缩与 Handoff

长会话的模型上下文会持续增长，直到接近 context window。赤铎通过**上下文压缩**把旧历史浓缩为摘要，让模型始终在可控窗口内工作。压缩只重建发送给模型的 `model_context.json`；`client_transcript.jsonl` 保留原始记录，客户端回放不受影响。

## 三种触发方式

| 方式 | 谁触发 | 摘要来源 | 结果 |
| --- | --- | --- | --- |
| 自动压缩 | daemon | 模型 | 上下文达到阈值后，用当前/最近成功的模型摘要化旧历史 |
| `/compact` 命令 | 用户 | 模型 | 立即压缩，并返回压缩前后的估算 token |
| `handoff` 工具 | Agent（模型） | Agent 提供 | 压缩到 Agent 给出的交接文本，同一 turn 继续 |

## 自动压缩与 `/compact`

自动压缩在 `profile.yaml` 的 `model.compactionTriggerRatio`（或当前模型 entry
覆盖值）达到时触发，阈值公式：

```text
(contextWindowTokens - maxOutputTokens) * compactionTriggerRatio
```

压缩会保留约 20K 最新 token 的“保留区”（必要时在 turn 中间切断，保留当前用户问题），更早的历史由模型生成摘要。`/compact` 只是把同一流程提前到任意时刻手动执行。配置细节见 [Profile 配置指南](profile.md)。

## Handoff：Agent 主动重建上下文

`handoff` 是内置原生工具之一。Agent 判断当前上下文不再适合继续任务时（上下文接近上限、任务阶段切换、需要“压缩自己”），可以调用它并附上精确的交接摘要，让同一 turn 在重建后的上下文里继续。

执行流程：

1. Agent 调用 `handoff`，附上 `handoff_text`（目标、已完成工作、决定、未解决问题、下一步）。
2. daemon 从模型上下文移除这次 handoff 工具调用/结果。
3. 上下文按默认压缩计划重建，摘要直接使用 `handoff_text`（不再让模型重新生成摘要）。
4. 向上下文插入 `<handoff_continuation>` 内部消息，提示上下文已重建、继续原任务。
5. turn 不结束，Agent 在重建后的上下文中继续工作。

约束：

- `handoff` 必须是同一批次中唯一的工具调用；与其它工具混用会整批返回错误。
- `handoff_text` 必须非空，且不超过 32,000 UTF-8 字节。
- `handoff` 始终允许，不经过权限确认——它不执行任何外部操作，只是重建上下文。

### 与 Subsessions 的区别

| | `handoff` | Subsession |
| --- | --- | --- |
| 上下文 | 压缩当前 session 自身上下文，同一 turn 继续 | 子 session 拥有全新独立上下文 |
| 结果 | 交接摘要直接成为当前上下文的一部分 | `<subsession_result>` 内部消息送回父 session |
| 适用场景 | 当前任务线太长，精简后继续 | 边界清晰、适合独立/并行处理的任务 |

需要拆分并行任务时用 [Subsessions](subsessions.md)；需要精简当前上下文、继续同一任务时用 `handoff`。
