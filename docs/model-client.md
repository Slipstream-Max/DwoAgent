# Model Client、Provider 与 Model List

Model Client 固定使用 OpenAI Responses API。配置中没有 `protocol`，也没有 Provider
继承或 Provider type。运行时分成三个独立概念：

| 概念 | 负责内容 |
| --- | --- |
| Profile model 配置 | 默认模型、默认 reasoning、压缩策略和已启用 Provider |
| Provider | `baseUrl`、凭据、headers、超时和该端点的模型映射 |
| Model List | 模型限制、能力、reasoning 请求参数和 hosted tools |

每个 Provider 创建一个共享 `BaseClient`。同一中转站下的 GPT、Grok 和 DeepSeek
共享地址、凭据、HTTP pool，但分别从自己的 Model List profile 继承模型能力。

## Profile schema

```yaml
model:
  default:
    # 稳定身份始终是 provider/modelId，不使用显示名称。
    model: newapi/ds-v4-pro
    reasoning: High # 可选

  # Agent 上下文策略，不属于模型能力。
  compactionTriggerRatio: 0.8

  providers:
    newapi:
      baseUrl: https://gateway.example.com/v1
      apiKeyEnv: NEW_API_KEY
      apiKey: null

      headers:
        X-Client-Name: dwoagent

      request:
        requestTimeoutMs: 300000
        streamIdleTimeoutMs: 300000

      extraBody: {}

      models:
        # Map key 是客户端显示名称。
        "5.6 Terra":
          # modelId 是请求 body 中的 model，也是 session/default 的稳定 ID。
          modelId: gpt-5.6-terra
          profile: openai/gpt-5.6-terra

        "Grok 4.6":
          modelId: grok-4.6
          profile: grok/grok-4.6
          # 可选；省略时使用上面的 profile 默认值。
          compactionTriggerRatio: 0.5
          hostedTools: [webSearch, xSearch]

        "DeepSeek V4 Pro":
          modelId: ds-v4-pro
          profile: deepseek/deepseek-v4-pro
```

模型部署可以覆盖 `contextWindowTokens`、`maxOutputTokens`、
`defaultReasoningMode`、`capabilities`、`reasoning`、`hostedTools`、
`temperature`、`topP`、`compactionTriggerRatio` 和 `extraBody`。
`compactionTriggerRatio` 省略时使用 `model.compactionTriggerRatio`；
`modelId` 省略时使用外层显示名称。

同一 Provider 下解析后的 `modelId` 必须唯一。外层显示名称可以修改，不影响已有
session；改变 `modelId` 会改变稳定身份。

## 官方 Provider 简写

`openai`、`grok`、`deepseek` 等名称命中内置 Model List family。省略 `baseUrl` 和
`models` 时，自动使用 family 的官方地址并启用它的全部模型：

```yaml
model:
  default:
    model: deepseek/deepseek-v4-pro
    reasoning: High
  providers:
    deepseek:
      apiKeyEnv: DEEPSEEK_API_KEY
```

可以设置 `baseUrl` 覆盖官方地址而不重复模型列表：

```yaml
providers:
  openai:
    baseUrl: https://compatible.example.com/v1
    apiKeyEnv: CUSTOM_API_KEY
```

官方 Provider 显式配置 `models` 时，它成为 allowlist，只启用列出的模型；`profile`
省略时按 `providerName/modelId` 推导。

自定义 Provider 必须配置 `baseUrl` 和非空 `models`，每个模型必须配置 `profile`。

`baseUrl` 是 API root。Client 固定请求 `POST {baseUrl}/responses`，不要在配置中附加
`/responses`。

## Model List

内置文件位于 `dwo-model-client/resources/models/<family>.yaml`。用户扩展位于 profile
根目录的 `resource/models/<family>.yaml`。文件名构成 profile 引用的 family；用户文件
与同名内置 family 合并，同 ID 模型由用户定义覆盖。

```yaml
# resource/models/minimax.yaml
baseUrl: https://api.minimax.example/v1 # 可选，仅用于同名官方 Provider 简写

models:
  minimax-m2.5:
    contextWindowTokens: 200000
    maxOutputTokens: 32000

    capabilities:
      imageInput: false
      toolCalls: true

    hostedTools:
      webSearch:
        type: web_search

    defaultReasoningMode: High
    reasoning:
      Off:
        reasoning:
          effort: none
      High:
        reasoning:
          effort: high

    temperature: null
    topP: null
    extraBody: {}
```

引用为 `minimax/minimax-m2.5`。Model List 不保存凭据、Provider 实例、显示名称或
`compactionTriggerRatio`；该值由 profile model 配置（全局默认或单个部署模型
override）提供。

## Hosted tools

Model List 的 `hostedTools` 是名称到 Responses 原生工具对象的映射。部署模型省略
`hostedTools` 时启用 profile 中的全部 hosted tools；配置名称列表时只启用选中项；
配置 `[]` 时全部关闭。DWO 本地 function tools 不在这里配置，只要求
`capabilities.toolCalls: true`。

## 上下文身份

运行时维护两个独立身份：

```text
connection id     = newapi
context owner id  = newapi/grok
```

连接 ID 用于复用 URL、Key 和 HTTP pool。上下文 owner 使用 `provider/family`，用于标记
Responses 原生 reasoning 和 hosted-tool item。同一中转站从 GPT 切到 Grok 时会清除
不兼容的原生 item；同一中转站同 family 模型切换时保留兼容 item。可见消息和本地
function call/result 不受影响。

## 请求构造

请求 body 按以下顺序合并：

1. Provider `extraBody`；
2. Model List `extraBody`；
3. 部署模型 `extraBody`；
4. temperature、top_p、max_output_tokens；
5. 选中 reasoning mode 的请求参数；
6. 固定的 model、input、tools 和 stream。

`model`、`input`、`tools`、`stream` 和 token limit 等 transport-owned 字段禁止通过
`extraBody` 覆盖。

模型输入上限和自动压缩触发点为：

```text
max input = contextWindowTokens - maxOutputTokens
compact trigger = max input * model.compactionTriggerRatio
```

Token 估算包括 system prompt、消息、reasoning、图片、本地/远端工具 call/result 和
tool schemas。不额外预留固定 token。

## 错误与重试

HTTP 401/403、429、上下文超限、其他 4xx、Provider 状态、transport、无效响应、流中断
和取消保持独立错误分类。瞬态失败使用 1/2/4/8/16 秒指数退避和 jitter；中途断流会先
持久化可见的部分输出，再在同一 turn 重试。上下文超限走独立的 compaction recovery。
