# Profile 配置指南

Profile 保存赤铎的配置、提示词、skills、MCP 和运行数据。默认目录是 `~/.dwoagent/`，入口文件是 `profile.yaml`。

安装和启动见 [README](https://github.com/Slipstream-Max/DwoAgent/blob/main/README.md)，channel 部署见 [Channel 部署与使用](https://github.com/Slipstream-Max/DwoAgent/blob/main/docs/channels.md)，定时任务见 [Automation 使用指南](https://github.com/Slipstream-Max/DwoAgent/blob/main/docs/automation.md)，CLI 参数见 [命令参考](https://github.com/Slipstream-Max/DwoAgent/blob/main/docs/commands.md)。

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
|  |- providers/
|  |  `- <provider-type>.yaml
|  |- skills/
|  |  `- <skill>/SKILL.md
|  `- mcp/
|     |- mcp.json
|     `- oauth/
|- runtime/
|  |- sessions/YYYY/MM/DD/<session-id>/
|  |  |- session.json
|  |  |- model_context.json
|  |  `- client_transcript.jsonl
|  |- workspaces/<session-id>/
|  |- attachments/<channel>/YYYY/MM/DD/<session-id>/
|- logs/
`- channels/
   |- weixin/
   |  |- runtime.yaml
   |  `- secret.yaml
   |- telegram/
   |  |- runtime.yaml
   |  `- secret.yaml
   |- feishu/
   |  |- runtime.yaml
   |  `- secret.yaml
   |- qq/
   |  |- runtime.yaml
   |  `- secret.yaml
   `- websocket/
      `- secret.yaml
```

| 路径 | 是否手动编辑 | 内容 |
| --- | --- | --- |
| `profile.yaml` | 是 | 模型、默认权限、channels 和 automation。 |
| `resource/prompts/` | 是 | System prompt 和 profile 级规则。 |
| `resource/providers/` | 是 | 每个文件定义一个自定义模型 provider type。 |
| `resource/skills/` | 是 | 本地 skill。 |
| `resource/mcp/mcp.json` | 是 | MCP server 配置。 |
| `runtime/` | 通常不需要 | Session、附件和 OAuth。 |
| `logs/` | 通常不需要 | Daemon 结构化诊断日志。 |
| `channels/` | 由命令管理 | 绑定信息和当前选择的 session。 |

## 完整 profile.yaml

下面的配置包含所有顶层部分：

```yaml
name: coder
description: coding agent
policyMode: confirm
maxModelSteps: 100
externalSkillsDirs: []

logging:
  level: info
  retentionDays: 14

channels:
  weixin:
    enabled: true
    replayTurns: 5
    replayMode: response
    markdownFilter: true
    mediaInput: true

  telegram:
    enabled: false
    replayTurns: 5
    replayMode: response
    botTokenEnv: TELEGRAM_BOT_TOKEN
    tgProxy: null
    mediaInput: true

  feishu:
    enabled: false
    replayTurns: 5
    replayMode: response
    appIdEnv: FEISHU_APP_ID
    appSecretEnv: FEISHU_APP_SECRET
    platform: feishu
    mediaInput: true
  qq:
    enabled: false
    replayTurns: 5
    replayMode: response
    mediaInput: true
  websocket:
    enabled: false
    port: 8765

automation:
  enabled: false
  timeoutSeconds: 900
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

## 热加载

Daemon 每秒检查 `profile.yaml`，完整解析并校验成功后应用整份配置，不需要重启。无效或写入中的配置不会覆盖当前运行态，修正文件后会在下一次检查自动生效。

- `name`、`description` 和模型选项会立即反映到 `profile-list`、ACP 和后续配置查询。
- provider、模型地址、凭据、能力和限制从已有 session 的下一次模型请求起生效。删除已有 session 正在使用的模型 alias 会使该 session 的后续请求报配置错误，直到切换到有效模型。
- `policyMode`、默认模型和 `maxModelSteps` 是创建新 session、subsession 或 automation session 时使用的默认值，不会改写已有 session 自己的配置。
- `channels` 变化会重新构造 channel manager，并短暂停止和重启已连接且仍启用的 channel。
- `externalSkillsDirs` 变化会立即更新所有 session（含已有 session）可用的技能目录。
- `automation` 会重新计算任务调度；`logging.level` 和 `logging.retentionDays` 也会立即更新。设置了 `DWO_LOG` 时，环境变量仍优先于 profile 日志级别。

`resource/prompts/`、`resource/skills/`、`resource/mcp/mcp.json` 和运行时 channel capability 仍由各自 watcher 热加载。channel capability 只存在于 daemon 进程内，不写入 runtime；已有 session 会在模型步骤边界收到环境变更消息；发生 compaction 时，system prompt 会从当前资源重新构建。

## 顶层字段

| 字段 | 必需 | 说明 |
| --- | --- | --- |
| `name` | 是 | Profile 名称，不能为空。 |
| `description` | 是 | Profile 说明，不能为空。 |
| `policyMode` | 是 | 新 session 的默认权限：`full_access`、`confirm` 或 `watch`。 |
| `maxModelSteps` | 否 | 单回合 agent 循环的最大模型步数：`0`（无限）或 `5`–`200`，默认 `100`。 |
| `logging` | 否 | Daemon 文件日志级别和保留天数。 |
| `externalSkillsDirs` | 否 | 额外 skills 目录列表，可挂载他人的 skill；相对路径相对 profile 根目录解析。 |
| `channels` | 否 | 微信、Telegram、飞书/Lark 和 QQ Bot adapter。 |
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

Daemon 将结构化 JSONL 日志写入 profile 根目录的 `logs/`，按日轮转，不向 stdout 或 stderr 输出诊断日志。`level` 支持 `error`、`warn`、`info`、`debug` 和 `trace`；`retentionDays` 的有效范围是 1 到 365。

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

内置 catalog 按 provider 分文件维护在
`dwo-model-client/resources/providers/`。OpenAI-compatible 网关可以继承
`openai`，只覆盖完整的 Responses URL：

```yaml
model:
  defaultModelId: gpt-5.6-terra
  providers:
    newapi:
      type: openai
      baseUrl: https://gateway.example.com/v1/responses
      apiKeyEnv: NEW_API_KEY
  models:
    - modelName: gpt-5.6-terra
      provider: newapi
      modelId: gpt-5.6-terra
```

`openai` 与 `deepseek` 都使用 Responses transport，并统一发送
`input`、`max_output_tokens` 和 Responses SSE 事件。Provider catalog 可以通过
模型级 `hostedTools` 声明服务端工具；本地 function tools 会在请求时与其合并。

### Responses 上下文与 provider 切换

Responses 返回的不是一整块 assistant message。赤铎会按原始顺序保存 reasoning、assistant message、本地 `function_call`/output 和 provider 托管的 tool call。下一次请求仍按这个顺序回放，不会把它们重新揉成一条消息；压缩和 usage 估算也使用同一套结构。

其中 reasoning 和托管工具调用可能包含只对当前 provider instance 有效的状态，因此带有 provider 归属。切换 provider instance 时，daemon 会在下一次模型请求前永久移除这些私有项，同时保留用户与 assistant 可见消息，以及本地工具的 call/result。只在同一个 provider 下切换模型不会触发这项清理。

这里改变的是 `model_context.json`，不是 `client_transcript.jsonl`。客户端回放仍能看到完整的 reasoning、远端工具事件和原始消息。类似地，切换到纯文本模型时，图片只会从模型上下文移除，transcript 仍保留原始输入。

用户自定义 provider 放在 `resource/providers/<type>.yaml`。文件名（不含扩展名）
就是 `profile.yaml` 中引用的 `type`；文件内容只定义一个 provider，不再包含顶层
`providers` map。例如：

```yaml
# resource/providers/newapi.yaml
protocol: open_ai_responses
endpoint: https://gateway.example.com/v1/responses
models:
  custom-model:
    contextWindowTokens: 200000
    maxOutputTokens: 32000
    capabilities:
      imageInput: true
      toolCalls: true
    hostedTools:
      - type: web_search_preview
    defaultReasoningMode: medium
    reasoning:
      low: {reasoning: {effort: low}}
      medium: {reasoning: {effort: medium}}
      high: {reasoning: {effort: high}}
```

自定义文件不能覆盖 `openai`、`deepseek` 等内置 type；需要修改时应使用新的文件名，
再在 profile provider instance 中引用它。无效文件会使整次 profile reload 失败，
daemon 会继续保留上一份有效配置。

#### 添加第三方 provider 的最短步骤

如果第三方接口兼容 OpenAI Responses，只需：

1. 在 `resource/providers/<type>.yaml` 定义 `endpoint` 和 `models`；
2. 在 `profile.yaml` 的 `model.providers` 中增加一个 instance，`type` 指向该文件名；
3. 在 `model.models` 中增加 model alias，并设置 `modelId`；
4. 通过 `apiKeyEnv` 指定密钥环境变量。

当前配置式 catalog 的 transport 是 `open_ai_responses`。如果 provider 使用完全不同的
协议，需要在 Rust 中实现 `ModelClient`，再通过 `AgentService` 注入自定义 client；不能仅靠
profile YAML 动态加载任意协议实现。

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
| 微信 | `enabled`、`replayTurns`、`replayMode`、`markdownFilter`、`mediaInput` |
| Telegram | `enabled`、`replayTurns`、`replayMode`、`botTokenEnv`、`tgProxy`、`mediaInput` |
| 飞书/Lark | `enabled`、`replayTurns`、`replayMode`、`appIdEnv`、`appSecretEnv`、`platform`、`mediaInput` |
| QQ Bot | `enabled`、`replayTurns`、`replayMode`、`mediaInput` |
| WebSocket | `enabled`、`port` |

### Channel 字段

| Channel | 字段 | 默认值/限制 | 作用 |
| --- | --- | --- | --- |
| 全部消息 channel | `enabled` | 必填，`false` 或 `true` | 是否启动该 channel。修改后会热重启对应 adapter。 |
| 全部消息 channel | `replayTurns` | 必填，范围 `0..=10`；安装模板为 `5` | `/use` 切换 session 后回放的最近完成 turn 数。 |
| 微信、Telegram、飞书、QQ | `replayMode` | 默认 `response`；`response` 或 `full` | `response` 只发送最终回答；`full` 还发送 reasoning 和 tool-call 阶段。微信只允许 `response`。 |
| 微信、Telegram、飞书、QQ | `mediaInput` | 默认 `true` | 是否接收图片和文件；关闭后只处理文本。 |
| 微信 | `markdownFilter` | 必填 | 是否将 assistant Markdown 转成微信兼容文本。 |
| Telegram | `botTokenEnv` | 非空环境变量名 | BotFather token 所在的环境变量。token 不写入 profile 或 secret。 |
| Telegram | `tgProxy` | 默认 `null`；必须是 `http://` 或 `https://` | 仅代理 Telegram Bot API 和媒体下载。 |
| 飞书/Lark | `appIdEnv`、`appSecretEnv` | 非空环境变量名 | 企业自建应用凭据所在的环境变量。 |
| 飞书/Lark | `platform` | `feishu` 或 `lark` | 选择国内飞书或海外 Lark 的 API 地址。 |
| QQ Bot | 无凭据字段 | 通过二维码绑定 | 运行 `dwo channel qq bind` 后写入 `channels/qq/secret.yaml`，仅支持单用户 C2C 私聊。 |
| WebSocket | `port` | 默认 `8765`，必须大于 `0` | ACP WebSocket 监听端口，固定路径为 `/acp`。 |

`replayTurns` 最大为 10。Token、App ID 和 App Secret 从环境变量读取。QQ Bot 通过 `dwo channel qq bind` 扫码绑定，不在 `profile.yaml` 中填写凭据。

WebSocket 固定监听 `0.0.0.0:<port>`，ACP 路径固定为 `/acp`。访问 token 自动生成并保存到 `channels/websocket/secret.yaml`，不需要写入 profile。

绑定命令、开放平台设置和 slash commands 见 [Channel 部署与使用](https://github.com/Slipstream-Max/DwoAgent/blob/main/docs/channels.md)。

## Automation

```yaml
automation:
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
        cwd: projects/demo
      prompt: 检查项目状态。
```

| 字段 | 默认值/限制 | 说明 |
| --- | --- | --- |
| `automation.enabled` | 默认 `false` | 是否按照 cron 自动调度；手动 `dwo automation run` 不受此开关影响。 |
| `automation.timeoutSeconds` | 默认 `900`，范围 `1..=86400` | 单次 automation turn 的最长运行时间。到时会要求 agent 停止使用工具并回答。 |
| `automation.jobs` | 默认 `[]` | 定时任务列表；每个任务的 `name` 必须唯一。 |
| `jobs[].enabled` | 默认 `true` | 是否参与调度。 |
| `jobs[].schedule.cron` | 必填 | 标准五字段 cron：分 时 日 月 周。 |
| `jobs[].schedule.timezone` | 默认 `local` | `local`、`UTC` 或有效 IANA 时区，例如 `Asia/Shanghai`。 |
| `jobs[].session` | 必填 | `mode: new` 创建新 session，`mode: fixed` 投递到指定 `sessionId`。 |
| `jobs[].prompt` | 必填 | 每次触发提交给 agent 的文本。 |

`mode: new` 还需要 `behavior: every_time` 或 `once`；`cwd` 和 `title` 可选。`mode: fixed` 需要已有的 `sessionId`。完整字段和运行行为见 [Automation 使用指南](https://github.com/Slipstream-Max/DwoAgent/blob/main/docs/automation.md)。

## Prompts 与 Skills

| 路径 | 说明 |
| --- | --- |
| `resource/prompts/System.md` | 必需的主 system prompt。 |
| `resource/prompts/AGENTS.md` | 可选 profile 级规则。 |
| `<session-cwd>/AGENTS.md` | 当前工作目录规则。 |
| `<session-cwd>/.agents/AGENTS.md` | 项目级规则。 |
| `resource/skills/<name>/SKILL.md` | Profile 内可用的 skill。 |
| `<session-cwd>/.agents/skills/<name>/SKILL.md` | 项目级 skill，与 profile 同名时项目级生效。 |
| `externalSkillsDirs` 指定的目录 | 外部 skill；同名时优先级为 profile < 外部 < 项目。 |

Daemon 会监听这些固定资源、session 初始工作目录、`.agents/` 和 `externalSkillsDirs` 中的规则与技能变化，并在 agent loop 的边界通知现有 session。

## MCP

`resource/mcp/mcp.json`：

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

MCP catalog 只保存在 daemon 内存中，daemon 每次启动都会从 `resource/mcp/mcp.json` 重新建立；OAuth 凭据保存在同目录的 `resource/mcp/oauth/`；连接和调用由 daemon 内的 MCP runtime 管理。

```text
dwo mcp list
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
| `runtime/automation-runs.yaml` | 最近 100 次 automation run 的有界状态与回答预览。 |
| `channels/<channel>/secret.yaml` | 绑定用户和私聊目标。 |
| `runtime/attachments/<channel>/...` | 从 channel 下载的图片和文件。 |

Telegram token 和飞书 App ID/Secret 不写入 `secret.yaml`，始终从 `profile.yaml` 指定的环境变量读取。QQ Bot 例外：二维码绑定返回的 AppID/AppSecret 会写入 `channels/qq/secret.yaml`，profile.yaml 不填写 QQ 凭据。
