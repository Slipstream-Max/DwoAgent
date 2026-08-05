# dwo-model-client

Provider-configured model transport for the rewrite.

```text
ConfiguredModelClient
|- model alias registry
`- provider id -> BaseClient
   |- HTTP/auth/retry/cancellation
   |- OpenAI-compatible Responses input/output items
   |- SSE text/reasoning/function/hosted-tool assembly
   `- non-streaming response normalization
```

The public model boundary exposes model limits, image capability, and the model
operations:

```text
model_limits(model_alias) -> context/output/input limits + compact trigger
supports_image_input(model_alias) -> bool
stream_turn(selection, messages, tools, event_sender, cancellation) -> ModelReply
summarize(selection, compaction_view, cancellation)                  -> SummaryReply
```

User turns always request streaming output and may include tool schemas.
Compaction summaries always use a non-streaming request and never include
tools. Both paths resolve the session's model alias through the same model and
provider configuration.

The session runtime consults `supports_image_input` before accepting an image
prompt or committing a model switch. Provider message shaping still validates
the capability as a final boundary check.

The model context is one canonical message sequence. The transport projects it
to Responses `input` items. Native response output items are persisted and
replayed verbatim so hosted-tool state, reasoning items, and function calls
remain valid across turns.

## Configuration

The built-in catalog is assembled from one file per provider under
`resources/providers/`. Each file owns one provider's transport policy,
headers, request fields, model capabilities, limits, and reasoning request
parameters. The profile selects provider/model entries, supplies credentials,
and may override a complete provider URL or model limits.

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
headers, retry policy, hosted tools, and provider request fields remain
catalog-owned. Responses requests always use `max_output_tokens`.
Catalog provider body, catalog model body, and the selected reasoning map are
deep-merged in that order. Reasoning never changes the model output limit.

The built-in `openai` preset uses the Responses transport. A NewAPI-compatible
gateway can inherit it by overriding the complete endpoint:

```yaml
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

Each catalog model may declare `hostedTools`. DeepSeek uses `web_search`; the
OpenAI/NewAPI preset uses `web_search_preview`. Local function schemas are
flattened to the Responses function-tool shape and appended to those hosted
tools at request time.

Only `function_call` items enter the local tool executor. Hosted calls such as
`web_search_call` are retained in native output items and emitted as completed
remote tool events. When a provider includes results on the item, the same
event exposes them as the remote tool output.

Profiles may add provider types as individual files under
`resource/providers/`. The filename stem becomes the provider type, so
`resource/providers/newapi.yaml` is selected with `type: newapi`. Each file has
the same shape as a built-in provider file and contains one provider only.
Custom provider names may not replace built-in names.

The model input budget is derived rather than configured separately:

```text
max input = context window - max output
compact trigger = max input * compact threshold
```

Context usage is estimated from the complete model request, including messages,
tool calls, tool results, and tool schemas. Provider response usage is optional
transport metadata and is not used for session context accounting.
Transport-owned fields (`model`, `input`, `instructions`, `tools`, `stream`,
and `max_output_tokens`) cannot be overridden by configuration.

Provider response structure errors fail the model step. Malformed individual
tool arguments remain a raw string in the normalized tool call, allowing the
tool executor to return a per-call parse error without discarding the rest of
the model response.

HTTP/provider failures are classified into context-length, authentication,
rate-limit, invalid-request, provider-status, transport, and protocol errors.
The agent loop may compact and retry once only for the context-length class.
