# Profile 配置指南

单一 Host 配置保存赤铎的模型、提示词、skills、MCP 和运行数据。默认目录是 `~/.dwoagent/`，入口文件是 `profile.yaml`；这里没有可选择的多 profile 身份。

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
|  |- models/
|  |  `- <family>.yaml
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
|  |- projects/<project-id>/
|  |  |- project.json
|  |  |- workspace/
|  |  `- topics/<topic-id>/{overview.md,AGENTS.md}
|  |- attachments/<channel>/YYYY/MM/DD/<session-id>/
|  `- websocket/
|     `- secret.yaml
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
```

| 路径 | 是否手动编辑 | 内容 |
| --- | --- | --- |
| `profile.yaml` | 是 | 模型、默认权限、channels 和 automation。 |
| `resource/prompts/` | 是 | System prompt 和 profile 级规则。 |
| `resource/models/` | 是 | 扩展或覆盖可引用的 Model List family。 |
| `resource/skills/` | 是 | 本地 skill。 |
| `resource/mcp/mcp.json` | 是 | MCP server 配置。 |
| `runtime/` | 通常不需要 | Session、附件和 OAuth。 |
| `logs/` | 通常不需要 | Daemon 结构化诊断日志。 |
| `channels/` | 由命令管理 | 绑定信息和当前选择的 session。 |

## 完整 profile.yaml

下面的配置包含所有顶层部分：

```yaml
policyMode: confirm
maxModelSteps: 100
externalSkillsDirs: []
externalRuleFiles: []

logging:
  level: info
  retentionDays: 14

channels:
  weixin:
    enabled: true
    replayTurns: 5
    outputMode: final
    markdownFilter: true
    mediaInput: true

  telegram:
    enabled: false
    replayTurns: 5
    outputMode: final
    botTokenEnv: TELEGRAM_BOT_TOKEN
    tgProxy: null
    mediaInput: true

  feishu:
    enabled: false
    replayTurns: 5
    outputMode: final
    appIdEnv: FEISHU_APP_ID
    appSecretEnv: FEISHU_APP_SECRET
    platform: feishu
    mediaInput: true
  qq:
    enabled: false
    replayTurns: 5
    outputMode: final
    mediaInput: true

websocket:
  enabled: false
  bind: 127.0.0.1
  port: 8787

automation:
  enabled: false
  timeoutSeconds: 900
  jobs: []

model:
  default:
    model: deepseek/deepseek-v4-pro
    reasoning: High
  compactionTriggerRatio: 0.8
  providers:
    deepseek:
      apiKeyEnv: DEEPSEEK_API_KEY
```

Profile 使用严格 schema，未知字段会报错。旧的 `agent.yaml`、profile 级 `tools` 开关和额外 provider transport 配置不受支持。

## 热加载

Daemon 每秒检查 `profile.yaml`，完整解析并校验成功后应用整份配置，不需要重启。无效或写入中的配置不会覆盖当前运行态，修正文件后会在下一次检查自动生效。

- 模型选项会立即反映到 ACP 和后续配置查询。
- provider、模型地址、凭据、能力和限制从已有 session 的下一次模型请求起生效。删除已有 session 正在使用的稳定模型 ID 会使该 session 的后续请求报配置错误，直到切换到有效模型。
- `policyMode`、默认模型和 `maxModelSteps` 是创建新 session、subsession 或 automation session 时使用的默认值，不会改写已有 session 自己的配置。
- `channels` 变化会重新构造 channel manager，并短暂停止和重启已连接且仍启用的 channel。
- `externalSkillsDirs` 变化会立即更新所有 session（含已有 session）可用的技能目录。
- `externalRuleFiles` 变化会立即更新所有 session 的额外规则文件列表；文件内容或列表变化在
  下一 model step 由 EnvironmentWatcher 注入。
- `automation` 会重新计算任务调度；`logging.level` 和 `logging.retentionDays` 也会立即更新。设置了 `DWO_LOG` 时，环境变量仍优先于 profile 日志级别。

`resource/prompts/`、`resource/skills/`、`resource/mcp/mcp.json` 和运行时 channel capability 仍由各自 watcher 热加载。channel capability 只存在于 daemon 进程内，不写入 runtime；已有 session 会在模型步骤边界收到环境变更消息；发生 compaction 时，system prompt 会从当前资源重新构建。

## 顶层字段

| 字段 | 必需 | 说明 |
| --- | --- | --- |
| `policyMode` | 是 | 新 session 的默认权限：`full_access`、`confirm` 或 `watch`。 |
| `maxModelSteps` | 否 | 单回合 agent 循环的最大模型步数：`0`（无限）或 `5`–`200`，默认 `100`。 |
| `logging` | 否 | Daemon 文件日志级别和保留天数。 |
| `externalSkillsDirs` | 否 | 额外 skills 目录列表，可挂载他人的 skill；相对路径相对 profile 根目录解析。 |
| `externalRuleFiles` | 否 | 额外规则文件列表；相对路径相对 profile 根目录解析，规则 pwd 为 profile 根目录。 |
| `channels` | 否 | 微信、Telegram、飞书/Lark 和 QQ Bot adapter。 |
| `automation` | 否 | Cron 定时任务。 |
| `model` | 是 | Provider、模型部署和默认模型。 |

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

最小官方配置：

```yaml
model:
  default:
    model: deepseek/deepseek-v4-pro
    reasoning: High
  compactionTriggerRatio: 0.8
  providers:
    deepseek:
      apiKeyEnv: DEEPSEEK_API_KEY
```

`default.model` 使用稳定的 `provider/modelId`。Provider 名称命中内置 family 且省略
`models` 时，使用官方地址并启用 family 的全部模型；`baseUrl` 可选覆盖官方地址。
`default.reasoning` 省略时使用 Model List 中模型的 `defaultReasoningMode`。

第三方中转站显式声明地址和模型映射：

```yaml
model:
  default:
    model: newapi/ds-v4-pro
    reasoning: High
  compactionTriggerRatio: 0.8
  providers:
    newapi:
      baseUrl: https://gateway.example.com/v1
      apiKeyEnv: NEW_API_KEY
      headers: {}
      request:
        requestTimeoutMs: 300000
        streamIdleTimeoutMs: 300000
      extraBody: {}
      models:
        "5.6 Terra":
          modelId: gpt-5.6-terra
          profile: openai/gpt-5.6-terra
        "Grok 4.6":
          modelId: grok-4.6
          profile: grok/grok-4.6
        "DeepSeek V4 Pro":
          modelId: ds-v4-pro
          profile: deepseek/deepseek-v4-pro
```

外层 map key 是显示名称；`modelId` 是请求参数和 session 稳定身份；`profile` 是
`family/catalogModelId`，提供上下文长度、最大输出、reasoning、能力和 hosted tools。
自定义 Provider 必须配置 `baseUrl`、非空 `models` 和每个模型的 `profile`。
`baseUrl` 是 API root，Client 请求 `{baseUrl}/responses`。

部署模型可以覆盖 `contextWindowTokens`、`maxOutputTokens`、
`defaultReasoningMode`、`capabilities`、`reasoning`、`hostedTools`、
`temperature`、`topP` 和 `extraBody`。显式 `models` 是 allowlist。

用户可在 `resource/models/<family>.yaml` 添加 Model List。文件与同名内置 family
合并，同 ID 定义由用户文件覆盖。完整格式见
[Model Client、Provider 与 Model List](model-client.md)。

Context token 根据 system prompt、消息、reasoning、图片、tool call/result 和 tool schema
估算。压缩触发点为：

```text
(contextWindowTokens - maxOutputTokens) * compactionTriggerRatio
```

不额外预留固定 token。压缩比例属于 Agent profile，不写进 Model List。

### Responses 上下文与模型切换

Responses 原生 reasoning 和 hosted-tool item 使用 `provider/family` 作为兼容域。
同一中转站从 GPT 切换到 Grok 会清理这些不兼容原生项；同一中转站同 family 模型共享
兼容域。用户/assistant 可见消息和本地 function call/result 始终保留。

切换到不支持图片的模型时，图片会从 `model_context.json` 移除；完整输入仍保留在
`client_transcript.jsonl`。

## Channels

Channels 配置 adapter 是否启动，以及回放、凭据环境变量、代理和媒体输入等选项。

| Channel | 关键字段 |
| --- | --- |
| 微信 | `enabled`、`replayTurns`、`outputMode`、`markdownFilter`、`mediaInput` |
| Telegram | `enabled`、`replayTurns`、`outputMode`、`botTokenEnv`、`tgProxy`、`mediaInput` |
| 飞书/Lark | `enabled`、`replayTurns`、`outputMode`、`appIdEnv`、`appSecretEnv`、`platform`、`mediaInput` |
| QQ Bot | `enabled`、`replayTurns`、`outputMode`、`mediaInput` |

### Channel 字段

| Channel | 字段 | 默认值/限制 | 作用 |
| --- | --- | --- | --- |
| 全部消息 channel | `enabled` | 必填，`false` 或 `true` | 是否启动该 channel。修改后会热重启对应 adapter。 |
| 全部消息 channel | `replayTurns` | 必填，范围 `0..=10`；安装模板为 `5` | `/use` 切换 session 后回放的最近完成 turn 数。 |
| 微信、Telegram、飞书、QQ | `outputMode` | 默认 `final`；`final` 或 `full` | `final` 只发送最终回答；`full` 按顺序发送 thinking、tool-call 和每个阶段的回答。微信只允许 `final`。 |
| 微信、Telegram、飞书、QQ | `mediaInput` | 默认 `true` | 是否接收图片和文件；关闭后只处理文本。 |
| 微信 | `markdownFilter` | 必填 | 是否将 assistant Markdown 转成微信兼容文本。 |
| Telegram | `botTokenEnv` | 非空环境变量名 | BotFather token 所在的环境变量。token 不写入 profile 或 secret。 |
| Telegram | `tgProxy` | 默认 `null`；必须是 `http://` 或 `https://` | 仅代理 Telegram Bot API 和媒体下载。 |
| 飞书/Lark | `appIdEnv`、`appSecretEnv` | 非空环境变量名 | 企业自建应用凭据所在的环境变量。 |
| 飞书/Lark | `platform` | `feishu` 或 `lark` | 选择国内飞书或海外 Lark 的 API 地址。 |
| QQ Bot | 无凭据字段 | 通过二维码绑定 | 运行 `dwo channel qq bind` 后写入 `channels/qq/secret.yaml`，仅支持单用户 C2C 私聊。 |

`replayTurns` 最大为 10。Token、App ID 和 App Secret 从环境变量读取。QQ Bot 通过 `dwo channel qq bind` 扫码绑定，不在 `profile.yaml` 中填写凭据。

## WebSocket Transport

`websocket` 是独立顶层配置，不属于 `channels`：

```yaml
websocket:
  enabled: false
  bind: 127.0.0.1
  port: 8787
```

`bind` 必须是 IP 地址，`port` 必须大于 0。`/acp` 提供 ACP v2，`/dwo` 提供 Management RPC。两枚访问 token 自动生成并保存到 `runtime/websocket/secret.yaml`，不写入 profile。

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
| `runtime/projects/<project-id>/topics/<topic-id>/AGENTS.md` | 看板 Topic 的 Knowledge；以 `Project.pwd` 作为规则 pwd。 |
| `resource/skills/<name>/SKILL.md` | Profile 内可用的 skill。 |
| `<session-cwd>/.agents/skills/<name>/SKILL.md` | 项目级 skill，与 profile 同名时项目级生效。 |
| `externalSkillsDirs` 指定的目录 | 外部 skill；同名时优先级为 profile < 外部 < 项目。 |
| `externalRuleFiles` 指定的文件 | Profile 配置的额外规则文件；相对路径相对 profile 根目录解析。 |

SessionService 在创建 `SystemPromptBuilder` 时同时装配 external skill dirs 和 external rule files。
Daemon 会监听这些固定资源、session 初始工作目录、Topic external rule file、`.agents/`、
`externalSkillsDirs` 和 `externalRuleFiles` 中的规则与技能变化，并在 model step 边界通知现有
session。每份规则同时向模型提供来源路径和适用的 pwd。

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

Session 由 Host 放入 Project：Project 可以使用显式 pwd，也可以使用 `runtime/projects/<project-id>/workspace/`。workspace 属于 Project，不属于 Session；删除 Session 不删除 Project workspace。Topic 归属由 `project.json` 中的 `sessionIds` 保存，不写入 Session metadata。

## Channel 数据

| 文件 | 内容 |
| --- | --- |
| `channels/<channel>/runtime.yaml` | 当前选择的 session 等运行状态。 |
| `runtime/automation-runs.yaml` | 最近 100 次 automation run 的有界状态与回答预览。 |
| `channels/<channel>/secret.yaml` | 绑定用户和私聊目标。 |
| `runtime/attachments/<channel>/...` | 从 channel 下载的图片和文件。 |

Telegram token 和飞书 App ID/Secret 不写入 `secret.yaml`，始终从 `profile.yaml` 指定的环境变量读取。QQ Bot 例外：二维码绑定返回的 AppID/AppSecret 会写入 `channels/qq/secret.yaml`，profile.yaml 不填写 QQ 凭据。
