# Weixin Agent Example

This example runs a single-user Weixin assistant channel.

1. Set the model API key:

```powershell
$env:DEEPSEEK_API_KEY = "..."
```

2. Log in and bind Weixin:

```powershell
cargo run -- channel login weixin --agent-folder examples/weixin-agent
```

3. Start the long-lived channel host:

```powershell
cargo run -- serve --agent-folder examples/weixin-agent
```

The Weixin channel creates its own session. Its initial model settings come
from `channels.yaml`:

```yaml
weixin:
  media_input: true
  media_output: true
  override_model: deepseek-v4-pro
  override_reasoning_mode: high
```

These overrides only apply when the Weixin channel session is first created.
After `channel_sessions/weixin/session/session.json` exists, the session keeps
its own model and reasoning mode.

For a newly created Weixin channel session, `media_output: false` hides
`weixin_reply_media(path)` from the model and the channel will not attach the
Weixin media sender for tool calls.

Runtime files are created under:

- `channel_secret/weixin/`
- `channel_sessions/weixin/session/`

The checked-in example intentionally does not include real Weixin auth files.
