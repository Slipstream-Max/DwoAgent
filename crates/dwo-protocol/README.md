# dwo-protocol API

`dwo-protocol` 是 DwoAgent 本地 IPC、远程 WebSocket、CLI 和 Flutter binding 共享的
Management RPC 线协议 crate。它只定义传输无关的 envelope、错误、能力发现、方法注册表
和少量共享 Session DTO，不负责打开 socket，也不执行 Host 业务。

聊天协议仍然是 ACP。`dwo-protocol` 中的 `acp` route 用于本地 IPC 分流以及 Host adapter
需要的共享 Session 方法，不是另一套 ACP 数据模型。

## 依赖

```toml
[dependencies]
dwo-protocol = { path = "../dwo-protocol" }
```

## Envelope

每个请求必须声明 JSON-RPC 版本、字符串 request id、逻辑 route、方法和参数：

```json
{"jsonrpc":"2.0","id":"req-42","route":"dwo","method":"config.snapshot","params":{}}
```

成功与失败响应互斥：

```json
{"jsonrpc":"2.0","id":"req-42","result":{"defaultModel":"gpt-5"}}
{"jsonrpc":"2.0","id":"req-42","error":{"code":"invalid_params","message":"model name is required"}}
```

事件没有 request id：

```json
{"jsonrpc":"2.0","route":"dwo","method":"config.changed","params":{"seq":18,"params":{}}}
```

Rust 客户端通常使用构造器生成 request id：

```rust
use dwo_protocol::{RpcRequest, RpcRoute};
use serde_json::json;

let request = RpcRequest::new(RpcRoute::Dwo, "config.snapshot", json!({}));
assert_eq!(request.jsonrpc, "2.0");
```

`RpcRequest::new` 使用 UUID 生成 id。需要在超时重试时保持幂等的客户端，应自行保存并复用
第一次请求的 id，而不是重新调用构造器。

## 公开类型

| 类型 | 用途 |
| --- | --- |
| `RpcRoute` | `Acp` / `Dwo` 逻辑路由及 `as_str()` |
| `RpcRequest` | 请求 envelope，`new()` 自动填充版本和 UUID |
| `RpcResponse` | 互斥的 `result` / `error` 响应，使用 `success()`、`failure()` 构造 |
| `RpcError` | 结构化错误及常用错误构造器 |
| `RpcEvent` | 无 id 的服务端事件 envelope |
| `ManagementCapabilities` | `dwo.capabilities` 的序列化结果 |
| `MethodSpec` | 方法名、route、操作类型、副作用和关联事件 |
| `MethodRoute` | Registry 中的 `Acp`、`Dwo`、`Both` |
| `MethodOperation` | `Query`、`Command`、`Subscription` |
| `SessionRecord` / `SessionSnapshot` | 客户端只读的精简 Session 投影 |
| `SessionOptions` | Session 当前配置和模型选项 |
| `PromptDirectiveOptions` | 当前 Session 可用的 Skill/MCP directive |

这些类型都从 crate 根导出。Management 各方法的业务参数和返回结构见
[Dwo API](https://github.com/Slipstream-Max/DwoAgent/blob/main/docs/api.md)；
ACP 的完整消息类型由 ACP crate/SDK 定义，不在这里重复维护。

## 方法注册与能力发现

注册表是所有 transport 的路由和副作用真相来源：

```rust
use dwo_protocol::{capabilities, is_side_effect_method, method_allowed, method_spec};

assert!(method_allowed("dwo", "model.list"));
assert!(!method_allowed("dwo", "session.prompt"));
assert!(is_side_effect_method("model.set_default"));

let spec = method_spec("channel.telegram.status").unwrap();
let advertised = capabilities();
assert_eq!(advertised.protocol_version, 3);
```

`channel.<kind>.*` 只匹配当前允许的 `weixin`、`telegram`、`feishu` 和 `qq`，未知 kind
不会因为字符串前缀相似而进入 Host。客户端启动时仍应调用 `dwo.capabilities`，不要把本地
crate 版本中的方法列表当作远程 Host 一定具备的能力。

注册表将方法分成：

- `Query`：只读，不进入 Host request cache；
- `Command`：有副作用，transport 应携带稳定 request id；
- `Subscription`：建立事件流，由 transport 管理连接生命周期。

`dwo-protocol` 只负责分类。实际去重由 `Host::handle_request(client_id, request_id, ...)`
执行，key 是 client id 与 request id 的组合。

## 错误码

`RpcError` 当前提供以下稳定类别：

| code | 客户端含义 |
| --- | --- |
| `invalid_request` | envelope、版本或 route 不合法 |
| `invalid_params` | 字段缺失、类型错误或业务输入非法 |
| `method_not_found` | 当前 Host 不支持该方法 |
| `not_found` | Session、Job、Provider 等资源不存在 |
| `conflict` | request id 被复用于不同请求等状态冲突 |
| `permission_denied` | 当前调用方无权执行 |
| `auth_required` | 需要登录或 OAuth 授权 |
| `unavailable` | 下游服务或连接当前不可用 |
| `timeout` | 操作超时 |
| `internal_error` | 未归入可操作类别的服务端错误 |

`RpcError::from_anyhow` 是 Host transport 边界的兜底映射。新的业务代码应尽量产生能够
稳定落入上述类别的错误；客户端必须根据 `code` 决策，把 `message` 用于展示和诊断。

## 增加协议方法

二开增加方法时需要同时完成以下工作：

1. 在 `dwo-protocol/src/registry.rs` 注册 route、operation、副作用和关联事件。
2. 在 Host 中实现业务行为，并接入 `Host::handle_method`。
3. 为合法输入、非法参数、route 隔离和副作用重试增加测试。
4. 更新 capabilities 消费端、Management RPC 文档和对应 Dart/其他 binding。

不要仅在 transport 中硬编码一个方法。否则 capabilities、route 校验、幂等分类和其他
transport 会产生不同语义。
