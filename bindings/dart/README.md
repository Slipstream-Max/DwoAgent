# Dwo RPC Dart binding

这个包提供 Flutter 本地/远程管理面板共用的 Dwo RPC envelope、错误、capabilities、事件
cursor 和管理 client。远程使用 `DwoWebSocketTransport` 连接 `/dwo`；本地使用
`DwoStdioTransport` 启动 `dwo rpc` 桥接进程，通过 Windows named pipe / Unix socket
连接已经运行的 daemon。两个 transport 都直接由 `package:dwo_rpc/dwo_rpc.dart` 导出。

聊天不走这个包。Flutter 的聊天连接使用 ACP v2：本地使用 `dwo acp --protocol v2`
stdio adapter，远程连接 WebSocket `/acp`。内部 IPC `route=acp` 不是标准 ACP wire。
Dwo RPC 只负责 Host 配置、Session 查询、Skill、Automation 和
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

本地连接：

```dart
final transport = await DwoStdioTransport.connect(
  executable: r'C:\path\to\dwo.exe',
  timeout: const Duration(seconds: 30),
);
final dwo = DwoRpcClient(transport);
final sessions = await dwo.sessions(); // 自动读取所有分页
final page = await dwo.sessionsPage(all: true, cursor: 0, limit: 100);
final subscription = await dwo.subscribeEvents(cursor: lastCursor);
final listener = subscription.events.listen(onEvent, onError: onError);
// 退出 GUI 时仅释放客户端；不发送 daemon.shutdown 或 session/cancel。
await listener.cancel();
await dwo.close();
```

`DwoStdioTransport.connect({required String executable, Duration timeout = const
Duration(seconds: 30)})` 使用 `Process.start(executable, ['rpc'])`，连接时读取并校验
`dwo.capabilities`。启动/握手及每个请求都有超时；daemon 不存在时连接失败，绝不启动
daemon 或 Runtime。stderr 单独消费，stdout 仅接受 JSON lines；格式错误、进程退出或
断线会立即失败所有 pending 请求并关闭事件流。超时响应、重复响应不会破坏其他 pending
请求。`close()` 可重复调用，会关闭 stdin，并在子进程不退出时只终止自己的桥接子进程。

每个 stdio transport 支持一次 `subscribe()` / `subscribeEvents()`。事件 replay 及后续事件
通过独立 daemon IPC 连接传输，不阻塞普通管理请求。即使 replay 和事件连续到达、调用方
尚未监听，事件也会被缓存。取消事件 listener 只停止本地监听；该 IPC 连接在 transport
关闭时释放。恢复订阅时创建新 transport，使用 cursor 补读；不要在同一 transport 上重复
订阅。事件流尚未被监听时，`close()` 也不会挂起。

`sessions({bool all = true, int limit = 100})` 返回 `Future<List<JsonObject>>`，自动读取
`{sessions, nextCursor}` 的所有分页，拒绝循环/回退 cursor。需要逐页加载时使用
`sessionsPage({bool all = true, int? cursor, int limit = 100})`，返回
`Future<DwoSessionPage>`；其字段为 `List<JsonObject> sessions` 和 `int? nextCursor`。
请求 cursor 按 Host 协议编码为字符串，返回 cursor 为非负整数或 null。

所有 command 方法都允许传稳定的 `requestId`。Host 按物理 `client + request id` 去重
副作用操作。重连会改变 client identity，因此不能跨重连盲重试有副作用操作。超时不会
取消 daemon 中已提交的操作。启动时读取 `dwo.capabilities`，不要假设服务端一定包含
binding 中的所有便捷方法。

## 验证

在本目录运行（不访问运行中的 daemon）：

```sh
dart analyze
dart --enable-asserts run test/dwo_rpc_test.dart
dart run test/session_pagination_test.dart
dart compile exe test/fixtures/stdio_daemon.dart -o .dart_tool/stdio-fixture.exe
dart run test/stdio_transport_test.dart .dart_tool/stdio-fixture.exe
```

Transport 测试使用独立 fixture 子进程，覆盖真实启动参数、分片 UTF-8/CRLF、乱序及重复
响应、订阅即刻事件、订阅后普通请求、stderr 排空、结构化错误、请求/连接超时、畸形帧、
子进程退出、pending 失败和幂等关闭，并检查没有 shutdown/cancel 清理请求。
