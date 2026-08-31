# Slash Commands

以 / 开头的输入分为两类：

- Prompt Directive：转换后交给模型，ACP 和消息 Channel 都可用；
- 本地命令：由 ACP Adapter 或 Channel Adapter 直接执行，不进入模型。

WebSocket /acp 与 stdio ACP 使用相同命令。WebSocket /dwo 是 Management RPC，不使用 Slash
Command。

## 入口对照

| 命令 | ACP | 微信/Telegram/飞书/QQ |
| --- | --- | --- |
| /skill、/mcp、/plan | 支持 | 支持 |
| /compact、/resume、/fork、/status | 支持 | 支持 |
| /help、/list、/new、/use、/del、/cancel | 不支持 | 支持 |
| /model、/reasoning、/policy | 不支持 | 支持 |
| /allow、/deny | 不支持 | 支持 |

## Prompt Directive

### /skill

~~~text
/skill <name> [prompt]
~~~

要求 Agent 使用当前 Session 可用的指定 Skill。name 必须与合并后的 Skill Catalog 精确匹配。
Skill 的目录格式和覆盖优先级见 [Prompt、Skill 与 MCP](resources.md#skill)。

### /mcp

~~~text
/mcp <server> [prompt]
~~~

要求 Agent 使用 daemon 中指定的 MCP Server。server 必须与当前 MCP Catalog 精确匹配。Server
配置和连接状态见 [Prompt、Skill 与 MCP](resources.md#mcp)。

### /plan

~~~text
/plan [topic]
~~~

让 Agent 先规划和确认需求，再执行工作。它是 Prompt Directive，不等于 plan 工具；Directive
告诉模型进入规划方式，plan 工具负责保存 Session 的执行清单。

Directive 可以出现在正文中，也可以在同一条消息中多次出现。未知 name、单独的 /skill 或
/mcp 保持原文，不会伪造一个 Catalog 条目。

## ACP 本地命令

| 命令 | 行为 |
| --- | --- |
| /compact | 立即压缩当前模型上下文，不把命令文本加入上下文 |
| /resume | 仅在 Session Idle 时启动继续 Turn；运行中忽略 |
| /fork | 复制当前 Session，返回新 ID，不切换当前连接 |
| /status | 显示当前 Session ID、模型、Reasoning 和状态 |

这四个命令不接受参数。ACP 协议的 session/resume 是重新连接已有 Session，不调用模型；不要和
/resume 混淆。ACP 原生 session/fork 和 /fork 都复制 Session，也都不会自动切换当前连接。

ACP 还会把当前可用的 skill <name> 和 mcp <name> 发布为补全候选。名称包含空格时不会发布为
候选，因为 Directive 的 name 是单个 Token。

## 消息 Channel 命令

四个消息平台使用同一套解析和帮助文本。

| 命令 | 参数和行为 |
| --- | --- |
| /help | 显示命令列表 |
| /list | 列出全局 Session；星号表示当前选中 |
| /new [name] [--cwd <path>] | 创建并选中 Session |
| /fork | 复制当前 Session，不切换 |
| /use <session> | 按序号、短 ID 或完整 ID 切换，并回放最近 Turn |
| /status | 显示 cwd、模型、Reasoning、Policy 和运行状态 |
| /del <session> | 删除指定 Session |
| /cancel | 取消当前 Active Turn |
| /compact | 压缩当前 Session 上下文 |
| /resume | Idle 时继续上一项工作 |
| /model <name> | 切换当前 Session 模型 |
| /reasoning <level|off> | 修改或关闭 Reasoning |
| /policy [full_access|confirm|watch] | 查询或修改权限 |
| /allow [id] | 批准当前或指定 Permission Request |
| /deny [id] | 拒绝当前或指定 Permission Request |

带空格的路径必须加引号：

~~~text
/new Project review --cwd "C:\Users\Example User\Documents\repo"
~~~

Telegram 的 /status@bot_name 会规范化为 /status。普通新消息只会排队，不会取消工具；需要中断
必须使用 /cancel。在 confirm 模式中，不带 ID 的 /allow 或 /deny 处理当前等待中的请求。

## 未识别命令

daemon 先解析 Prompt Directive，再交给入口的本地命令解析器。消息 Channel 中未知的 /command
会返回命令错误；ACP 只识别它发布的命令。要发送一个以斜杠开头的普通 Prompt，请避免让第一个
Token 与已发布命令同名。
