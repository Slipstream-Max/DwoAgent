# dwo-model-client

OpenAI Responses model transport with separate connection and model-profile identities.

```text
ConfiguredModelClient
|- provider id -> BaseClient (URL, key, headers, timeouts, HTTP pool)
`- provider/modelId -> ModelConfig (family profile, limits, capabilities, reasoning)
```

The transport is fixed to OpenAI Responses. Provider configuration has no protocol or type.
One provider can expose models from multiple families while sharing one `BaseClient`.

```yaml
default:
  model: newapi/ds-v4-pro
  reasoning: high
compactionTriggerRatio: 0.8
providers:
  newapi:
    baseUrl: https://gateway.example.com/v1
    apiKeyEnv: NEW_API_KEY
    models:
      "5.6 Terra":
        modelId: gpt-5.6-terra
        profile: openai/gpt-5.6-terra
      "DeepSeek V4 Pro":
        modelId: ds-v4-pro
        profile: deepseek/deepseek-v4-pro
        compactionTriggerRatio: 0.6 # optional per-model override
```

The map key is the display name. `modelId` is sent upstream and forms the stable selection
`provider/modelId`. `profile` selects model metadata from the built-in catalog or
`resource/models/<family>.yaml`.
`compactionTriggerRatio` may be set per model; when omitted, the profile-level
`compactionTriggerRatio` is used.

Official family names provide their base URL and full model list, so direct configuration only
needs credentials:

```yaml
default:
  model: deepseek/deepseek-v4-pro
providers:
  deepseek:
    apiKeyEnv: DEEPSEEK_API_KEY
```

Provider-native reasoning and hosted-tool items are owned by `provider/family`, not just the
connection. Switching families behind one gateway removes incompatible native items while
retaining visible messages and local function call/results.

See [docs/model-client.md](../../docs/model-client.md) for the complete schema, merge rules,
request construction, retry behavior, and custom Model List format.
