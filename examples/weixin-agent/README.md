# Weixin Agent Example

This example runs a single-user Weixin assistant channel.

1. Set the model API key:

```powershell
$env:DEEPSEEK_API_KEY = "..."
```

2. Log in and bind Weixin:

```powershell
cargo run -- channel login weixin --agent-profile examples/weixin-agent
```

3. Start the long-lived channel host:

```powershell
cargo run -- agent run --agent-profile examples/weixin-agent
```

The Weixin channel routes messages to an ordinary session. When no
`defaultSessionId` is configured, the first inbound message creates a normal
session using the initial model settings from `agent.yaml`:

```yaml
channels:
  weixin:
    mediaInput: true
    mediaOutput: true
    responseDetail: response_only
    overrideModel: deepseek-v4-pro
    overrideReasoningMode: high
```

These overrides only apply when the channel creates a new default ordinary
session. After that, the session keeps its own model and reasoning mode.

When `mediaOutput: false`, `weixin_reply_media(path)` is not exposed on
Weixin-triggered turns and the channel will not attach the Weixin media sender
for tool calls.

Runtime files are created under:

- `runtime/channel_secret/weixin/`
- `runtime/channel_state/weixin/`
- `runtime/sessions/`

The checked-in example intentionally does not include real Weixin auth files.
