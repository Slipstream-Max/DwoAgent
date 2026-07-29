# Profile 配置指南

Profile 保存赤铎的配置、提示词、skills、MCP 和运行数据。默认目录是 `~/.dwoagent/`，入口文件是 `profile.yaml`。

安装和启动见根目录 [README](../README.md)，channel 部署见 [Channel 部署与使用](channels.md)，定时任务见 [Automation 使用指南](automation.md)，CLI 参数见 [命令参考](commands.md)。

## 目录结构

```text
<profile-root>/
|- profile.yaml
|- bin/
|  `- dwo
|- resource/
|  |- prompts/
|  |  |- System.md
|  |  `- AGENTS.md
|  |- skills/
|  |  `- <skill>/SKILL.md
|  `- mcp.json
|- runtime/
|  |- sessions/YYYY/MM/DD/<session-id>/
|  |  |- session.json
|  |  |- model_context.json
|  |  `- client_transcript.jsonl
|  |- workspaces/<session-id>/
|  |- attachments/<channel>/YYYY/MM/DD/<session-id>/
|  |- channel-capabilities/<channel>.md
|  |- mcp/
|  |  |- catalog.json
|  |  `- oauth/
|  `- logs/
`- channels/
   |- weixin/
   |  |- runtime.yaml
   |  `- secret.yaml
   |- telegram/
   |  |- runtime.yaml
   |  `- secret.yaml
   `- feishu/
      |- runtime.yaml
      `- secret.yaml
```

| 路径 | 是否手动编辑 | 内容 |
| --- | --- | --- |
| `profile.yaml` | 是 | 模型、默认权限、channels 和 automation。 |
| `resource/prompts/` | 是 | System prompt 和 profile 级规则。 |
| `resource/skills/` | 是 | 本地 skill。 |
| `resource/mcp.json` | 是 | MCP server。 |
| `runtime/` | 通常不需要 | Session、附件、catalog、OAuth 和日志。 |
| `channels/` | 由命令管理 | 绑定信息和当前选择的 session。 |

## 完整 profile.yaml

下面的配置包含所有顶层部分：

```yaml
name: coder
description: coding agent
policyMode: confirm

logging:
  level: info
  retentionDays: 14

channels:
  weixin:
    enabled: true
    replayTurns: 5
    markdownFilter: true
    mediaInput: true

  telegram:
    enabled: false
    replayTurns: 5
    botTokenEnv: TELEGRAM_BOT_TOKEN
    tgProxy: null
    mediaInput: true

  feishu:
    enabled: false
    replayTurns: 5
    appIdEnv: FEISHU_APP_ID
    appSecretEnv: FEISHU_APP_SECRET
    platform: feishu
    mediaInput: true

automation:
  enabled: false
  jobs: []

model:
  defaultModelId: deepseek-v4-pro
  providers:
    deepseek:
      type: deepseek
      apiKeyEnv: DEEPSEEK_API_KEY
  models:
    - modelName: deepseek-v4-pro
      provider: deepseek
      modelId: deepseek-v4-pro
```

Profile 使用严格 schema，未知字段会报错。旧的 `agent.yaml`、profile 级 `tools` 开关和额外 provider transport 配置不受支持。

## 顶层字段

| 字段 | 必需 | 说明 |
| --- | --- | --- |
| `name` | 是 | Profile 名称，不能为空。 |
| `description` | 是 | Profile 说明，不能为空。 |
| `policyMode` | 是 | 新 session 的默认权限：`full_access`、`confirm` 或 `watch`。 |
| `logging` | 否 | Daemon 文件日志级别和保留天数。 |
| `channels` | 否 | 微信、Telegram 和飞书/Lark adapter。 |
| `automation` | 否 | Cron 定时任务。 |
| `model` | 是 | Provider、模型 alias 和默认模型。 |

## 权限模式

| 模式 | 行为 |
| --- | --- |
| `full_access` | 终端和文件编辑直接执行，显式 deny rule 仍然生效。 |
| `confirm` | 简单只读命令直接执行，其他命令和文件编辑需要确认。 |
| `watch` | 只允许简单只读命令和 watch allow rule，文件编辑会被拒绝。 |

`policyMode` 只设置新 session 的默认值。已有 session 可以通过 ACP config、channel `/policy` 或 CLI 参数单独修改。

## Logging

```yaml
logging:
  level: info
  retentionDays: 14
```

Daemon 将结构化 JSONL 日志写入 `runtime/logs/`，按日轮转，不向 stdout 或 stderr 输出诊断日志。`level` 支持 `error`、`warn`、`info`、`debug` 和 `trace`；`retentionDays` 的有效范围是 1 到 365。

环境变量 `DWO_LOG` 可以临时覆盖配置级别，并接受 `tracing` filter directives，例如：

```text
DWO_LOG=dwo_agent_service=debug,dwo_mcp=trace
```

日志只记录控制流、标识符、耗时和错误，不记录 prompt、模型响应、tool 参数、授权头或 channel 消息正文。CLI 的用户输出和 ACP/IPC 协议流不会写入 daemon 日志。

## Model

```yaml
model:
  defaultModelId: deepseek-v4-pro
  providers:
    deepseek:
      type: deepseek
      baseUrl: null
      apiKeyEnv: DEEPSEEK_API_KEY
  models:
    - modelName: deepseek-v4-pro
      provider: deepseek
      modelId: deepseek-v4-pro
      contextWindowTokens: 1000000
      maxOutputTokens: 384000
      compactThreshold: 0.5
      defaultReasoningMode: high
```

### Provider

| 字段 | 说明 |
| --- | --- |
| `type` | 内置 model catalog 中的 provider 类型。 |
| `baseUrl` | 可选 API 地址覆盖。 |
| `apiKeyEnv` | API key 环境变量名。 |
| `apiKey` | 可直接填写 key，使用环境变量更方便管理。 |

请求 headers、retry、request body 和模型 capabilities 来自内置 catalog。Profile 可以覆盖 provider 地址和凭据。

### Model Alias

| 字段 | 说明 |
| --- | --- |
| `modelName` | 在 CLI、ACP 和 channel 中使用的模型名称。 |
| `provider` | 指向 `providers` 中的实例。 |
| `modelId` | 指向该 provider 类型内置 catalog 中的模型。 |
| `contextWindowTokens` | 可选 context window 覆盖。 |
| `maxOutputTokens` | 可选最大输出覆盖。 |
| `compactThreshold` | 触发上下文压缩的比例。 |
| `defaultReasoningMode` | 新 session 默认 reasoning mode。 |

`defaultModelId` 必须匹配一个 `models[].modelName`。

Context token 由 daemon 根据 system prompt、消息、reasoning、图片、tool call/result 和 tool schema 估算。压缩阈值为：

```text
(contextWindowTokens - maxOutputTokens) * compactThreshold
```

### 图片模型切换

从图片模型切换到纯文本模型时，daemon 会先用当前或最近成功的图片模型生成文字摘要。摘要成功后再保存目标模型和无图 context。摘要失败时，原模型和 context 保持不变。

完整 transcript 仍会保留原图。切回图片模型后，已经从 model context 压缩掉的图片不会自动恢复，需要重新附图。

## Channels

Channels 配置 adapter 是否启动，以及回放、凭据环境变量、代理和媒体输入等选项。

| Channel | 关键字段 |
| --- | --- |
| 微信 | `enabled`、`replayTurns`、`markdownFilter`、`mediaInput` |
| Telegram | `enabled`、`replayTurns`、`botTokenEnv`、`tgProxy`、`mediaInput` |
| 飞书/Lark | `enabled`、`replayTurns`、`appIdEnv`、`appSecretEnv`、`platform`、`mediaInput` |
| WebSocket | `enabled`、`port` |

`replayTurns` 最大为 10。`platform` 使用 `feishu` 或 `lark`。Token、App ID 和 App Secret 从环境变量读取。

WebSocket 固定监听 `0.0.0.0:<port>`，ACP 路径固定为 `/acp`。访问 token 自动生成并保存到 `channels/websocket/secret.yaml`，不需要写入 profile。

绑定命令、开放平台设置和 slash commands 见 [Channel 部署与使用](channels.md)。

## Automation

```yaml
automation:
  enabled: true
  jobs:
    - name: daily-report
      schedule:
        cron: "0 9 * * *"
        timezone: Asia/Shanghai
      session:
        mode: new
        cwd: projects/demo
      prompt: 检查项目状态。
```

Automation 支持每次新建 session 和投递固定 session。完整字段和运行行为见 [Automation 使用指南](automation.md)。

## Prompts 与 Skills

| 路径 | 说明 |
| --- | --- |
| `resource/prompts/System.md` | 必需的主 system prompt。 |
| `resource/prompts/AGENTS.md` | 可选 profile 级规则。 |
| `<session-cwd>/AGENTS.md` | 当前工作目录规则。 |
| `resource/skills/<name>/SKILL.md` | Profile 内可用的 skill。 |

Daemon 会监听这些固定资源和 session 初始工作目录中的规则变化，并在 agent loop 的边界通知现有 session。

## MCP

`resource/mcp.json`：

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "."]
    },
    "github": {
      "type": "streamableHttp",
      "url": "https://example.test/mcp",
      "headers": {
        "Authorization": "Bearer ${GITHUB_TOKEN}"
      }
    },
    "notion": {
      "type": "streamableHttp",
      "url": "https://example.test/mcp",
      "auth": {"type": "oauth"}
    }
  }
}
```

Daemon 启动时并发连接 MCP server，并持续复用成功的 stdio/HTTP 连接。配置变化会触发初始化。状态包括 `starting`、`ready`、`auth_required` 和 `failed`。

`runtime/mcp/catalog.json` 保存当前 catalog 的派生内容。连接和调用由 daemon 内的 MCP runtime 管理。

```text
dwo mcp search <query>
dwo mcp call <server.tool> --args '<json>'
dwo mcp auth <server> [--logout]
```

## Session 数据

每个 session 使用三个文件：

| 文件 | 内容 |
| --- | --- |
| `session.json` | ID、标题、cwd、父 session、模型、reasoning 和权限模式。 |
| `model_context.json` | 当前发送给模型的上下文和 usage。 |
| `client_transcript.jsonl` | 完整、追加式的客户端事件记录。 |

上下文压缩会重建 `model_context.json`，不会删除 `client_transcript.jsonl`。

没有显式 cwd 的 session 使用 `runtime/workspaces/<session-id>/`。删除这种 session 时，对应自动 workspace 会一起删除。手动指定的工作目录不会被删除。

## Channel 数据

| 文件 | 内容 |
| --- | --- |
| `channels/<channel>/runtime.yaml` | 当前选择的 session 等运行状态。 |
| `channels/<channel>/secret.yaml` | 绑定用户和私聊目标。 |
| `runtime/channel-capabilities/<channel>.md` | 已绑定 channel 提供给模型的能力说明，不含凭据。 |
| `runtime/attachments/<channel>/...` | 从 channel 下载的图片和文件。 |

Telegram token 和飞书 App ID/Secret 不写入 `secret.yaml`。它们始终从 `profile.yaml` 指定的环境变量读取。

## 使用其他 Profile

全局参数 `--config-path` 可以指定另一个 `profile.yaml`：

```text
dwo --config-path /path/to/profile.yaml daemon status
dwo --config-path /path/to/profile.yaml acp
```

同一组 CLI 命令需要使用相同的配置路径，才能连接对应 daemon 和 session 数据。
