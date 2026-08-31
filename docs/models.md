# 模型与 Provider

本文回答一个问题：配置好 Provider 以后，怎样让 Dwo 认识一个模型。

Profile 中的 model.default、model.compactionTriggerRatio 和 model.providers 是运行配置，
字段说明在 [Profile 配置](profile.md#model)。本文只讲 Model List、模型能力和新模型接入。

## 内置模型

Dwo 使用 Responses API。当前内置 Model List：

| Family | Model ID | 图片 | Hosted Tool | Reasoning |
| --- | --- | --- | --- | --- |
| deepseek | deepseek-v4-pro | 否 | web_search | off、auto、low、high、max |
| deepseek | deepseek-v4-flash | 否 | web_search | off、auto、low、high、max |
| deepseek | deepseek-v4-flash-vision-exp | 是 | web_search | off、auto、low、high、max |
| openai | gpt-5.6-sol | 是 | web_search | low、medium、high、xhigh、max |
| openai | gpt-5.6-terra | 是 | web_search | low、medium、high、xhigh、max |
| openai | gpt-5.5 | 是 | web_search | low、medium、high、xhigh |
| openai | gpt-5.4 | 是 | web_search | low、medium、high、xhigh |
| grok | grok-4.5 | 是 | web_search、x_search | low、medium、high |
| grok | grok-4.6 | 是 | web_search、x_search | low、medium、high、xhigh |
| qwen | qwen3.8-max | 是 | web_search、web_extractor | auto、off、low、medium、xhigh |
| qwen | qwen3.8-flash | 是 | web_search、web_extractor | auto、off、low、medium、xhigh |
| qwen | qwen3.8-27b | 是 | web_search、web_extractor | auto、off、low、medium、xhigh |

所有内置模型都声明支持本地 Tool Call。实际启用项以 dwo model list 为准。Provider 名称是模型
引用的前缀，例如 deepseek/deepseek-v4-pro。能力来自 Model List，不根据模型名称推测。

## Model List 文件

把自定义 family 或模型放在：

    ~/.dwoagent/resource/models/<family>.yaml

示例：

~~~yaml
baseUrl: https://api.example.com/v1
models:
  my-model:
    contextWindowTokens: 128000
    maxOutputTokens: 8192
    defaultReasoningEffort: auto
    reasoningEfforts: [off, low, medium, high]
    reasoningSummary: auto
    capabilities:
      imageInput: true
      toolCalls: true
    hostedTools: [web_search]
~~~

字段：

| 字段 | 必填 | 作用 |
| --- | --- | --- |
| baseUrl | 否 | 该 family 的默认 API root；Provider 未覆盖时使用 |
| models | 是 | model id 到能力描述的映射 |
| contextWindowTokens | 是 | 上下文窗口大小 |
| maxOutputTokens | 是 | 单次最大输出；必须小于窗口 |
| defaultReasoningEffort | 否 | 默认 reasoning，默认 auto |
| reasoningEfforts | 否 | 允许的 reasoning 列表 |
| reasoningSummary | 否 | 是否请求 reasoning summary，目前为 auto 或省略 |
| capabilities.imageInput | 否 | 是否接受图片，默认 false |
| capabilities.toolCalls | 否 | 是否支持本地 function tool，默认 false |
| hostedTools | 否 | Provider 托管的工具类型，例如 web_search |
| temperature / topP | 否 | 请求采样参数 |
| extraBody | 否 | 额外 JSON 请求字段，不可覆盖 Dwo 保留字段 |

文件名决定 family 名称；同名文件会覆盖内置 family 的同名模型。解析失败会阻止整个 profile
加载，未知字段也会报错。

## 在 Provider 中映射部署模型

Provider 的 models map 允许把部署名称映射到目录中的能力 profile：

~~~yaml
model:
  default:
    model: gateway/ds-v4
    reasoning: high
  providers:
    gateway:
      baseUrl: https://gateway.example.com/v1
      apiKeyEnv: GATEWAY_API_KEY
      models:
        DeepSeek V4:
          modelId: ds-v4
          profile: deepseek/deepseek-v4-pro
~~~

- 外层 key 是给人看的显示名。
- modelId 是真实请求中的 model 值，也是稳定的 provider/model 标识。
- profile 指向 family/model id，复用它的上下文、reasoning、能力和 hosted tools。
- 自定义 Provider 必须有 baseUrl、非空 models，以及每个模型的 profile。
- 官方 family 可以省略 models，此时会启用该 family 的全部目录模型。
- Provider 的 extraBody、headers 和 request 会应用到该 Provider 的请求；模型 entry 的同名字段
  覆盖 Provider 默认。

完成后运行：

    dwo model list
    dwo model get-default

如果 default.model 不在解析后的模型列表中，daemon 不会启动。

## 能力和上下文

模型上下文由 system prompt、消息、reasoning、图片、tool call/result 和 tool schema 共同组成。
自动压缩触发点为：

    (contextWindowTokens - maxOutputTokens) * compactionTriggerRatio

compactionTriggerRatio 在 profile 的 model 段设置，单个 Provider 模型 entry 可以覆盖它。
模型切换到不支持图片的模型时，当前 model_context 会移除图片，但完整输入仍保存在 transcript。
不同 provider/family 间切换时，Dwo 会清理只对旧兼容域有效的原生 reasoning 和 hosted-tool item。

## 新增模型的检查清单

1. 确认服务是否提供 Responses API，API root 是否能访问 /responses。
2. 在 Model List 声明窗口、最大输出、reasoning 和能力。
3. 在 profile 的 providers 中配置 key、baseUrl 和模型映射。
4. 将 model.default 指向 provider/modelId，并选择支持的 reasoning。
5. 运行 dwo config-show 和 dwo model list。
6. 用一个新 Session 做文本、工具和图片（如果声明支持）测试。

Model List 是“模型是什么”，profile 是“当前部署如何连接它”。不要把 API key、token 或
部署地址中的私密信息写进共享的 Model List 文件。
