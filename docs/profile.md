# Profile 配置

Profile 是 daemon 的运行配置，默认位置是 ~/.dwoagent/profile.yaml。它决定默认权限、模型、
日志、资源目录、消息 Channel 和 WebSocket listener。Automation 不在 Profile 中，按 Project
保存；Session 和 Project 数据也在 runtime 目录中。

## 完整模板

~~~yaml
policyMode: confirm
maxModelSteps: 100
externalSkillsDirs: []
externalRuleFiles: []

logging:
  level: info
  retentionDays: 14

model:
  default:
    model: deepseek/deepseek-v4-pro
    reasoning: auto
  compactionTriggerRatio: 0.8
  providers:
    deepseek:
      apiKeyEnv: DEEPSEEK_API_KEY

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
~~~

Profile 使用 camelCase 字段名并拒绝未知字段。daemon 修改配置时先校验完整文件；校验失败会
继续使用旧配置。

## 顶层字段

| 字段 | 必填 | 默认值 | 作用 |
| --- | --- | --- | --- |
| policyMode | 是 | 无 | 新 Session 的权限：full_access、confirm 或 watch |
| maxModelSteps | 否 | 100 | 单个 turn 最多执行的 model step；0 表示不限，或使用 5..=200 |
| logging | 否 | info / 14 天 | JSONL 日志配置 |
| externalSkillsDirs | 否 | [] | 额外 Skill 目录；相对路径相对 Profile 根目录 |
| externalRuleFiles | 否 | [] | 额外规则文件；相对路径相对 Profile 根目录 |
| model | 是 | 无 | 默认模型和 Provider |
| channels | 否 | {} | 消息平台配置 |
| websocket | 否 | disabled / 127.0.0.1:8787 | 远程 ACP 和 Management RPC |

policyMode 只影响新 Session。已有 Session 的权限可独立修改。权限如何控制工具见
[Agent 工具](tools.md)。

## logging

~~~yaml
logging:
  level: info
  retentionDays: 14
~~~

| 字段 | 默认值 | 约束 |
| --- | --- | --- |
| level | info | error、warn、info、debug 或 trace |
| retentionDays | 14 | 1..=365 |

日志写入 Profile 根目录的 logs/ 并按日轮转。日志记录控制流、ID、耗时和错误，不记录 prompt、
模型回答、tool 参数、授权头或 Channel 正文。DWO_LOG 可临时覆盖级别，例如：

    DWO_LOG=dwo_agent_service=debug,dwo_mcp=trace

## model

Model 段负责“连接哪个 Provider，并选择哪个部署模型”。怎样新增 Model List、声明模型能力和
支持范围，见 [模型与 Provider](models.md)。

### default

~~~yaml
model:
  default:
    model: deepseek/deepseek-v4-pro
    reasoning: high
  compactionTriggerRatio: 0.8
~~~

| 字段 | 必填 | 默认值 | 作用 |
| --- | --- | --- | --- |
| default.model | 是 | 无 | provider/modelId，必须存在于解析后的模型列表 |
| default.reasoning | 否 | 模型默认值 | off、auto、low、medium、high、xhigh 或 max，以模型声明为准 |
| compactionTriggerRatio | 否 | 0.8 | 自动压缩比例，范围 (0, 1] |

压缩阈值为：

    (contextWindowTokens - maxOutputTokens) * compactionTriggerRatio

单个部署模型可以覆盖 compactionTriggerRatio。

### providers

官方 Family 可以只写凭据：

~~~yaml
providers:
  deepseek:
    apiKeyEnv: DEEPSEEK_API_KEY
~~~

自定义网关需要 baseUrl 和非空 models：

~~~yaml
providers:
  gateway:
    baseUrl: https://gateway.example.com/v1
    apiKeyEnv: GATEWAY_API_KEY
    headers:
      X-Client: dwo
    request:
      requestTimeoutMs: 300000
      streamIdleTimeoutMs: 300000
    extraBody: {}
    models:
      DeepSeek V4:
        modelId: ds-v4
        profile: deepseek/deepseek-v4-pro
~~~

| Provider 字段 | 默认/限制 | 作用 |
| --- | --- | --- |
| baseUrl | 官方 Family 可省略；自定义必填 | API root，实际请求 baseUrl/responses |
| apiKeyEnv | 可选 | API key 所在的环境变量 |
| apiKey | 可选，不建议 | 直接保存 key；非空时优先于 apiKeyEnv |
| headers | {} | 额外 HTTP Header |
| request.requestTimeoutMs | 300000，必须 >0 | 单次请求总超时 |
| request.streamIdleTimeoutMs | 300000，必须 >0 | 流式响应空闲超时 |
| extraBody | {} | Provider 级额外 JSON Body |
| models | 官方 Family 可省略；自定义必填且非空 | 部署模型映射 |

models 的外层 Key 是显示名称；modelId 是请求参数和稳定身份；profile 是
family/catalogModelId，用于复用 Model List 中的上下文窗口、Reasoning、能力和 Hosted Tool。
部署模型还可覆盖 contextWindowTokens、maxOutputTokens、defaultReasoningEffort、
reasoningEfforts、reasoningSummary、capabilities、hostedTools、temperature、topP、
compactionTriggerRatio 和 extraBody。完整字段见 [模型与 Provider](models.md)。

## channels

Profile 在这里列出所有字段；平台绑定、收发行为和故障排查见
[Channel 配置与行为](channels.md)。

| Channel | 字段 | 必填/默认 | 作用 |
| --- | --- | --- | --- |
| 全部 | enabled | 必填 | 是否启动 Adapter |
| 全部 | replayTurns | 必填，0..=10 | /use 后回放的完成 Turn 数 |
| 全部 | outputMode | 默认 final | final 或 full；微信只能 final |
| 全部 | mediaInput | 默认 true | 是否接收图片和文件 |
| 微信 | markdownFilter | 必填 | 是否转换 Assistant Markdown |
| Telegram | botTokenEnv | 必填且非空 | Bot Token 的环境变量名 |
| Telegram | tgProxy | 默认 null | http:// 或 https:// 代理 |
| 飞书/Lark | appIdEnv | 必填且非空 | App ID 的环境变量名 |
| 飞书/Lark | appSecretEnv | 必填且非空 | App Secret 的环境变量名 |
| 飞书/Lark | platform | 必填 | feishu 或 lark |
| QQ | 无凭据字段 | - | 凭据通过二维码绑定后写入 Secret 文件 |

Token、App ID 和 App Secret 不应直接写入 Profile。QQ 例外：二维码绑定返回的 AppID/AppSecret
由 daemon 保存到私有的 channels/qq/secret.yaml。

## websocket

~~~yaml
websocket:
  enabled: false
  bind: 127.0.0.1
  port: 8787
~~~

| 字段 | 默认值 | 约束与作用 |
| --- | --- | --- |
| enabled | false | 是否启动 listener |
| bind | 127.0.0.1 | 必须是 IP 地址 |
| port | 8787 | 必须大于 0 |

/acp 和 /dwo 使用两枚独立 Token，保存于 runtime/websocket/secret.yaml。连接地址、Token 轮换
和 TLS 部署见 [WebSocket 连接](websocket.md)。

## resources

Profile 只声明两类外部资源路径：externalSkillsDirs 和 externalRuleFiles。System.md、AGENTS.md、
Skill 目录、MCP JSON、加载优先级、环境变量和 OAuth 的配置统一见
[Prompt、Skill 与 MCP](resources.md)。

## Profile 根目录

~~~text
~/.dwoagent/
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
|  |- projects/<project-id>/
|  |- workspaces/<session-id>/
|  |- attachments/<channel>/
|  `- websocket/secret.yaml
|- channels/<channel>/
`- logs/
~~~

Project 和 Session 文件的完整字段见 [Project 文件与行为](projects.md) 和
[Session 与子 Agent](session.md)。上下文压缩只重建 model_context.json，不会删除完整
client_transcript.jsonl。

## 热加载与校验

daemon 监视 Profile、Prompt、Skill、Model List、MCP 和规则文件。可热加载的配置会在完整校验
通过后应用；MCP、Channel 和 WebSocket 可能重启相应连接。修改后运行：

~~~text
dwo config-show
dwo daemon status
~~~

失败时旧配置继续运行，具体错误见 logs/。
