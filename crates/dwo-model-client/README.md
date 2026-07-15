# dwo-model-client

Provider-configured model transport for the rewrite.

```text
ConfiguredModelClient
|- model alias registry
`- provider id -> BaseClient
   |- HTTP/auth/retry/cancellation
   |- OpenAI-compatible Chat Completions encoding
   |- SSE text/reasoning/tool-call assembly
   `- non-streaming response normalization
```

The public model boundary exposes model limits and two operations:

```text
model_limits(model_alias) -> context/output/input limits + compact trigger
stream_turn(selection, messages, tools, event_sender, cancellation) -> ModelReply
summarize(selection, compaction_view, cancellation)                  -> SummaryReply
```

User turns always request streaming output and may include tool schemas.
Compaction summaries always use a non-streaming request and never include
tools. Both paths resolve the session's model alias through the same model and
provider configuration.

The model context is one message sequence. Message index 0 is the system
message; there is no parallel `system_prompt` request field.

## Configuration

The built-in catalog is read from `resources/models.yaml`. It owns provider
transport policy, headers, request fields, model capabilities, and reasoning
request parameters. The profile selects provider/model entries, supplies
credentials, and may override a complete provider URL or model limits.

```yaml
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
  - modelName: deepseek-v4-flash
    provider: deepseek
    modelId: deepseek-v4-flash
```

One provider entry produces one shared `BaseClient`, so every model using that
provider shares its API key, endpoint, headers, HTTP pool, and retry policy.
Only `baseUrl`, `apiKeyEnv`, and `apiKey` are profile-level provider settings;
headers, retry policy, and provider request fields remain catalog-owned.
Catalog provider body, catalog model body, and the selected reasoning map are
deep-merged in that order. Reasoning never changes the model output limit.

The model input budget is derived rather than configured separately:

```text
max input = context window - max output - 10,000 tool-result headroom
compact trigger = max input * compact threshold
```

The 10,000-token headroom is an internal constant and is not exposed in YAML.
Transport-owned fields (`model`, `messages`, `tools`, `stream`, and
`stream_options`) cannot be overridden by configuration.

Provider response structure errors fail the model step. Malformed individual
tool arguments remain a raw string in the normalized tool call, allowing the
tool executor to return a per-call parse error without discarding the rest of
the model response.

HTTP/provider failures are classified into context-length, authentication,
rate-limit, invalid-request, provider-status, transport, and protocol errors.
The agent loop may compact and retry once only for the context-length class.
