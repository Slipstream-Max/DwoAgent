# Model Client 与 Provider Catalog

赤铎的模型接入分两层：**provider catalog** 定义传输端点、请求行为和模型规格；
**profile 的 `model` 段**声明使用哪些 provider 实例和模型 alias。两者在 daemon
启动或 profile 热加载时合并解析为运行时配置，由统一的 client 按
OpenAI Responses 协议发送请求。

## 分层结构

| 层 | 位置 | 内容 |
| --- | --- | --- |
| Provider catalog | 内置：`dwo-model-client/resources/providers/<type>.yaml`（编译进二进制）；自定义：profile 根目录 `resource/providers/<type>.yaml` | `endpoint`、`headers`、重试策略、请求体骨架、模型规格 |
| Profile model 段 | `profile.yaml` 的 `model.providers` / `model.models` | provider instance（`type` + 凭据 + 可选 `baseUrl` 覆盖）、模型 alias |
| 运行时配置 | 由 `ModelCatalog` + `AgentModelConfig` resolve 生成 | endpoint、Authorization、超时/重试、每个模型的 token 上限与能力 |
| 传输 client | `dwo-model-client` 的 `BaseClient` | 统一 `open_ai_responses` 协议（Responses API + SSE 流式） |

配置示例见 [Profile 配置指南](profile.md#model)。

## Catalog 文件格式

每个文件只定义一个 provider，文件名（不含扩展名）即 `type`。字段均使用
camelCase，未知字段会报错。

```yaml
protocol: open_ai_responses        # 当前唯一取值，可省略
endpoint: https://api.example.com/v1/responses   # 必填，http/https
headers:                            # 可选，附加到每次请求
  x-custom: value
request:                            # 可选，超时与重试策略
  requestTimeoutMs: 300000
  streamIdleTimeoutMs: 300000
  maxRetries: 4
  retryBaseDelayMs: 200
body:                               # 可选，provider 级请求体骨架
  extra_field: value
models:                             # 必填，至少一个
  my-model:
    contextWindowTokens: 200000     # 必填
    maxOutputTokens: 32000          # 必填
    compactThreshold: 0.8           # 默认 0.8，取值 (0, 1]
    temperature: 0.7                # 可选
    topP: 1.0                       # 可选
    body:                           # 可选，模型级请求体覆盖
      extra_field: value
    hostedTools:                    # 可选，provider 托管的服务端工具
      - type: web_search
    defaultReasoningMode: medium    # 默认 auto
    reasoning:                      # 可选，mode -> 请求体覆盖
      low: {reasoning: {effort: low}}
      medium: {reasoning: {effort: medium}}
    capabilities:                   # 默认均为 false
      imageInput: false
      toolCalls: true
```

### ProviderSpec 字段

| 字段 | 说明 |
| --- | --- |
| `protocol` | 传输协议，当前仅 `open_ai_responses`（默认值，可省略） |
| `endpoint` | 完整 Responses URL，必填，必须为 http/https |
| `headers` | 附加请求头；`Authorization: Bearer <key>` 由 client 在最后注入，优先于 catalog 中的同名头 |
| `request` | 超时与重试策略，见下表 |
| `body` | provider 级请求体骨架，与模型级 `body` 深合并 |
| `models` | `modelId -> ModelSpec` 映射，必填且非空 |

### request 策略

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `requestTimeoutMs` | 300000 | 单次请求超时（毫秒），必须为正 |
| `streamIdleTimeoutMs` | 300000 | 流式响应相邻事件的最大空闲间隔 |
| `maxRetries` | 4 | 最大重试次数（不含首次） |
| `retryBaseDelayMs` | 200 | 指数退避基数：`base × 2^(attempt-1)`，封顶 30 秒 |

可重试：HTTP 408/409/425/429/5xx，以及连接失败、超时等网络错误。其余错误立即
返回并按错误分类处理（见[错误分类](#错误分类)）。

### ModelSpec 字段

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `contextWindowTokens` | — | 必填，上下文窗口（token） |
| `maxOutputTokens` | — | 必填，最大输出（token） |
| `compactThreshold` | 0.8 | 触发上下文压缩的比例，取值 (0, 1] |
| `temperature` / `topP` | 无 | 设置后写入请求体 |
| `body` | 无 | 模型级请求体覆盖，与 provider `body` 深合并 |
| `hostedTools` | 空 | 服务端托管工具声明，请求时与本地 function tools 合并 |
| `reasoning` | 空 | `mode -> body 覆盖` 的有序映射，mode 名为任意标识符 |
| `defaultReasoningMode` | `auto` | 新 session 默认 reasoning mode；非 `auto` 时必须存在于 `reasoning` |
| `capabilities.imageInput` | false | 模型是否接受图片输入 |
| `capabilities.toolCalls` | false | 模型是否支持工具调用；工具非空而该值为 false 时请求报错 |

校验规则：

- `contextWindowTokens > 0`、`maxOutputTokens > 0`，且必须留出输入空间
  （`contextWindowTokens > maxOutputTokens`）；
- `compactThreshold` 必须在 (0, 1]；
- 任何 `body`（provider 级、模型级、reasoning 覆盖）都不得覆盖保留字段：
  `model`、`input`、`instructions`、`previous_response_id`、`tools`、`stream`、
  `stream_options`、`max_tokens`、`max_completion_tokens`、`max_output_tokens`。

## 内置 Provider

内置 catalog 按 provider 分文件维护在
`dwo-model-client/resources/providers/`，编译进二进制，改动需重新编译。

| type | endpoint | 模型 | 能力 |
| --- | --- | --- | --- |
| `openai` | `https://api.openai.com/v1/responses` | `gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.5`、`gpt-5.4`（1,050,000 ctx / 128,000 输出） | imageInput、toolCalls；hosted `web_search`；reasoning Low/Medium/High/XHigh/Max，默认 Medium |
| `deepseek` | `https://api.deepseek.com/responses` | `deepseek-v4-pro`、`deepseek-v4-flash`（1,000,000 ctx / 384,000 输出） | toolCalls（无图片输入）；hosted `web_search`；reasoning Auto/Off/Low/High/Max，默认 High |
| `grok` | `https://api.x.ai/v1/responses` | `grok-4.5`、`grok-4.6`（500,000 ctx / 128,000 输出） | imageInput、toolCalls；hosted `web_search` + `x_search`；reasoning Low/Medium/High/XHigh，默认 High |

## Profile 侧的 Provider instance

`model.providers` 中的每个实例引用 catalog 中的 `type`，并可选覆盖地址和凭据：

| 字段 | 说明 |
| --- | --- |
| `type` | catalog 中的 provider 类型，必填 |
| `baseUrl` | 可选，覆盖 catalog `endpoint`（OpenAI-compatible 网关可继承 `openai` 只改 URL） |
| `apiKeyEnv` | API key 环境变量名；变量缺失或为空时报 `MissingApiKey` |
| `apiKey` | 直接填写 key，优先于 `apiKeyEnv` |

`headers`、`request` 策略和 `body` 骨架只来自 catalog，profile 不覆盖。
`model.models` 中的 alias 通过 `modelId` 指向 catalog 模型，可覆盖
`contextWindowTokens`、`maxOutputTokens`、`compactThreshold`、
`defaultReasoningMode`；`temperature`/`topP`/`hostedTools`/`reasoning`/
`capabilities` 只来自 catalog。

## 请求构造

每次请求的 body 按以下顺序合并：

1. provider `body` 骨架；
2. 模型级 `body`（同键均为 object 时递归深合并，否则覆盖）；
3. `temperature`、`top_p`（catalog 中设置了才写入）；
4. `max_output_tokens`；
5. reasoning mode 覆盖：mode 为本次调用传入值或模型
   `defaultReasoningMode`；`auto` 表示不附加覆盖，否则深合并
   `reasoning[mode]` 对应的 body（mode 未配置则报配置错误）；
6. `model` = modelId、`input` = 消息数组；
7. `tools`（非空时）、`stream`。

## 错误分类

| 错误 | 触发条件 |
| --- | --- |
| `Authentication` | HTTP 401/403 |
| `RateLimited` | HTTP 429（超出 `maxRetries` 后） |
| `ContextLengthExceeded` | 响应体匹配上下文超限特征（如 `context_length_exceeded`、`maximum context length`），触发压缩恢复 |
| `InvalidRequest` | 其他 4xx |
| `ProviderStatus` | 其他状态码（如 5xx 超出重试次数） |
| `StreamInterrupted` | 流中断或空闲超时，保留已累积文本/工具调用 |
| `Cancelled` | 会话取消 |

## 自定义 Provider

自定义文件放在 profile 根目录的 `resource/providers/<type>.yaml`，文件名（不含
扩展名）就是 `type`。文件内容只定义一个 provider，不含顶层 `providers` map；
格式见[Catalog 文件格式](#catalog-文件格式)。

最短步骤（第三方接口兼容 OpenAI Responses 时）：

1. 在 `resource/providers/<type>.yaml` 定义 `endpoint` 和 `models`；
2. 在 `profile.yaml` 的 `model.providers` 中增加 instance，`type` 指向该文件名；
3. 在 `model.models` 中增加 model alias，并设置 `modelId`；
4. 通过 `apiKeyEnv` 指定密钥环境变量。

约束：

- 自定义文件名不能与内置 type（`openai`、`deepseek`、`grok`）冲突；需要修改
  内置定义时应使用新文件名，再在 profile instance 中引用；
- 无效文件会使整次 profile reload 失败，daemon 保留上一份有效配置；
- 配置式 catalog 的 transport 固定为 `open_ai_responses`。若 provider 使用完全
  不同的协议，需要在 Rust 中实现 `ModelClient`，再通过 `AgentService` 注入
  自定义 client，不能仅靠 profile YAML 动态加载任意协议。
