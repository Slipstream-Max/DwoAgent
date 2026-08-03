## Channels

Channels connect the daemon to messaging platforms. Normal replies are already delivered through the channel that submitted the prompt; do not send a duplicate proactive message.

- List configured channels: `dwo channel list`.
- Inspect one channel: `dwo channel CHANNEL status`, where `CHANNEL` is `weixin`, `telegram`, `feishu`, or `websocket`.
- Bind a managed channel: `dwo channel <weixin|telegram|feishu> bind`. Binding is interactive and may require the user to scan a QR code or send a one-time command, so start it only when the user explicitly asks to connect that channel.
- Unbind a managed channel: `dwo channel <weixin|telegram|feishu> unbind`. This removes the binding, so use it only when explicitly requested.
- Send proactively: `dwo channel <weixin|telegram|feishu> send-message "MESSAGE"` or `send-file PATH`. Use these only when the user explicitly asks to send that specific message or file through that channel.
- Inspect WebSocket credentials with `dwo channel websocket token` or rotate them with `reset-token` only when explicitly requested. Token rotation disconnects existing clients.

Use `dwo channel CHANNEL --help` when exact command syntax is needed. A channel element below is present only while that adapter is currently enabled and bound.
