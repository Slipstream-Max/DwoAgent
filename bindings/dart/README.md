# Dwo RPC Dart binding

这个包提供 Flutter 本地/远程管理面板共用的 Dwo RPC envelope、错误、capabilities、事件
cursor 和管理 client。远程可直接使用 `DwoWebSocketTransport` 连接 `/dwo`；本地 Windows
named pipe transport 在 Flutter 应用层实现 `DwoTransport` 后复用同一个 `DwoRpcClient`。

聊天不走这个包。Flutter 的聊天连接使用 ACP v2：本地连接 IPC `route=acp`，远程连接
WebSocket `/acp`。Dwo RPC 只负责 Host 配置、Session 查询、Skill、MCP、Automation 和
Channel 管理。

WebSocket listener 使用独立的 `websocketStatus`、`websocketConfig`、
`updateWebsocketConfig`、`setWebsocketEnabled`、`websocketToken` 和
`resetWebsocketToken` 方法，不通过 `channel(kind, action)`。

```dart
final transport = await DwoWebSocketTransport.connect(
  Uri.parse('ws://127.0.0.1:19000/dwo'),
  managementToken: token,
);
final dwo = DwoRpcClient(transport);
final capabilities = await dwo.capabilities();
final subscription = await dwo.subscribeEvents(cursor: lastCursor);
```

所有 command 方法都允许传稳定的 `requestId`。客户端超时重试时必须复用同一个 id，Host
会按 `client + request id` 去重副作用操作。启动时先读取 `dwo.capabilities`，不要假设服务端
一定包含 binding 中的所有便捷方法。
