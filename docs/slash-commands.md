# Slash Commands 使用指南

以 `/` 开头的消息在赤铎里分成三类：**Prompt directives**（进入模型）、**Channel 命令**（消息渠道本地控制）和 **本地 session 命令**（ACP/编辑器入口）。同一段文本在不同入口可能被识别为不同类型的命令，本文统一说明它们的用途与边界。

## 入口 × 命令对照

| 命令 | ACP / 编辑器 | 消息 Channel | 说明 |
| --- | --- | --- | --- |
| `/skill` `/mcp` `/plan` | ✅ | ✅ | Prompt directive，进入模型 |
| `/compact` `/resume` `/fork` `/status` | ✅ | ✅ | 本地控制；ACP 与 channel 各自实现 |
| `/help` `/list` `/new` `/use` `/del` `/cancel` | — | ✅ | 消息渠道专用 |
| `/model` `/reasoning` `/policy` | — | ✅ | 消息渠道专用（修改 session 配置） |
| `/allow` `/deny` | — | ✅ | 消息渠道专用（处理权限请求） |

> WebSocket channel 不走消息渠道命令，直接使用 ACP，因此只有第一、二行的命令可用。

## Prompt directives

`/skill`、`/mcp` 和 `/plan` 不是本地控制命令，而是**提示指令**：匹配后整段被替换为 XML block 注入模型上下文，后面的文本作为 prompt 保留。它们可以出现在正文任意位置，同一条消息可以重复或混合使用。

daemon 只替换名称与当前有效 catalog 精确匹配的 directive；未知名称、单独的 `/skill`、`/mcp` 保持原文并作为普通 prompt 透传。

### `/skill <名称> [提示]`

要求 Agent 使用当前 session 可用的指定 skill。

```text
/skill <skill-name> 帮我看看这个仓库的结构
```

### `/mcp <名称> [提示]`

要求 Agent 使用 daemon catalog 中已配置的指定 MCP server。

```text
/mcp <server-name> 查一下北京的天气
```

### `/plan [上下文]`

暂停当前工作，先和用户一起规划：Agent 进入规划模式，通过「设计树」访谈逐轮提问（每个问题附推荐答案），达成共识前不写代码。裸 `/plan` 或 `/plan <主题>` 都会展开规划指令。

```text
/plan 给 CLI 增加一个导出功能
```

## Channel 命令

四个消息 channel（微信、Telegram、飞书/Lark、QQ Bot）共用同一套命令定义，因此参数校验和 `/help` 内容一致。

| 命令 | 说明 |
| --- | --- |
| `/help` | 显示当前支持的命令 |
| `/list` | 列出全局 session，`*` 表示当前 channel 选择的 session |
| `/new [名称] [--cwd <路径>]` | 创建并选择 session；名称可包含空格 |
| `/fork` | 把当前 session 复制为一个新 session，并返回新 ID；当前选择不变 |
| `/use <session>` | 选择已有 session，并回放最近 turn |
| `/status` | 显示当前 session 的 cwd、模型、reasoning、policy 和运行状态 |
| `/del <session>` | 删除指定 session |
| `/cancel` | 请求取消当前 turn |
| `/compact` | 手动压缩当前 session context；命令本身不进入模型上下文 |
| `/resume` | 仅在 session 空闲时继续上一项工作；运行中静默忽略 |
| `/model <名称>` | 切换当前 session 的模型 alias |
| `/reasoning <等级\|off>` | 修改 reasoning mode，或使用 `off` 关闭 |
| `/policy [full_access\|confirm\|watch]` | 不带参数查看 policy，带参数修改 |
| `/allow [ID]` | 允许当前 pending permission，或指定 request ID |
| `/deny [ID]` | 拒绝当前 pending permission，或指定 request ID |

消息渠道特有细节：

- 带空格的工作目录需要引号：`/new Project review --cwd "C:\Users\Example User\Documents\repo"`
- Telegram 发来的 `/status@bot_name` 也会正确识别
- `confirm` 模式下直接发送 `/allow` 或 `/deny` 即可处理当前权限请求，也可以附带 ID
- 仅发送媒体也是有效 prompt；`/cancel` 是显式中断入口，发送新的普通消息不会隐式取消正在运行的工具

## 本地 session 命令（ACP）

创建或恢复 session 后，Agent 通过 ACP v1/v2 `available_commands_update` 宣告以下命令：

| 命令 | 说明 |
| --- | --- |
| `/compact` | 手动压缩当前 session context；命令文本不进入模型上下文 |
| `/resume` | session 空闲时加入内部继续指令并启动新 turn；运行中静默忽略，不排队也不报错 |
| `/fork` | 复制当前 session 并显示副本 ID；当前 ACP session 不变 |
| `/status` | 本地查询当前 session，显示完整 session ID、模型、reasoning 强度和状态；不会调用模型 |

注意区分：ACP 协议自身的 `session/resume` 是重新接入已有 session、恢复 observer 和可选回放历史，不会启动模型；它与 `/resume` 命令不是同一功能。ACP 同时声明实验性原生 `session/fork`；它和 `/fork` 都返回副本 ID，但都不会切换当前 ACP session。

## 分组边界

为什么同一个 `/skill` 在有的入口报 "not a recognized command"？因为文本以 `/` 开头时，daemon 先尝试匹配 prompt directive（`/skill` `/mcp` `/plan`），匹配失败后文本再进入入口自己的命令解析：

- **消息 channel**：继续匹配 channel 命令（clap 解析），不在表内的命令报错；
- **ACP**：只在宣告的命令列表内识别（`/compact` `/resume` `/fork` `/status` `/plan` + 动态 `skill <name>` / `mcp <name>`），列表外的以 `/` 开头的文本报 not recognized。

所以消息 channel 里输入单独 `/skill`（没有名称）或近似拼写 `/skills`、`/mcpx`，都会落入命令解析并报错——directive 需要完整的精确名称。

