# dwo-host API

`dwo-host` 是 DwoAgent 的长期运行应用层。一个 `Host` 拥有唯一的 AgentService、Session
repository、有效配置、MCP runtime、Channel runtime、Automation scheduler、事件历史和
shutdown 边界。IPC、WebSocket、ACP shim、CLI 和 Flutter 都是它外面的适配器。

二开有两种接入方式：

- 直接使用 typed Session API，把 Host 嵌入另一个 Rust 程序；
- 实现新的 transport/client，通过 `handle_request` 调用 Dwo Management RPC。

配置方法及 JSON 参数见
[Dwo Management RPC](https://github.com/Slipstream-Max/DwoAgent/blob/main/docs/management-api.md)，
共享 envelope 和 registry 见
[dwo-protocol API](https://github.com/Slipstream-Max/DwoAgent/blob/main/crates/dwo-protocol/README.md)。

## 生命周期

```rust,ignore
use std::path::Path;
use dwo_host::Host;

let host = Host::load(Path::new("C:/Users/me/.dwoagent/profile.yaml")).await?;
let shutdown = host.shutdown_token();

// 在这里启动自定义 transport，或直接调用 Session API。

shutdown.cancel();
host.shutdown().await;
```

`Host::load` 接受 profile 根目录或其中的配置文件路径，并完成：

- 解析严格的单 Host 配置；
- 启动和同步 MCP runtime；
- 创建 Session repository 与 AgentService；
- 恢复 Channel、Automation 和 ephemeral Session 状态；
- 启动配置、MCP watcher 和 Automation scheduler。

它不会自动打开 IPC 或 WebSocket listener。官方 binary 在 composition root 中并行调用
`dwo_ipc::serve` 和 `dwo_websocket::serve`。

| API | 说明 |
| --- | --- |
| `Host::load(path)` | 加载并启动 Host，返回 `Arc<Host>` |
| `Host::shutdown_token()` | transport 和后台任务共享的 cancellation token |
| `Host::shutdown()` | 停止 Channel、MCP、AgentService，并清理 ephemeral Session |
| `Host::profile_root_path()` | 返回规范化后的 profile 根目录 |
| `profile_root(path)` | 在创建 Host 前解析根目录 |
| `logging::init(path)` | 初始化 JSONL 文件日志并返回必须持有的 `LoggingGuard` |
| `logging::reload(config)` | 更新日志级别和保留天数 |

`LoggingGuard` 被丢弃时会停止日志维护任务，所以 binary 应让它至少与 Host 生命周期一样长。

## 直接 Session API

Session API 使用 `dwo-agent-service` 和 `dwo-context` 的领域类型，不需要构造 JSON：

```rust,ignore
use dwo_agent_service::{EndpointId, SessionId};
use dwo_context::MessageContent;

let snapshot = host.setup_session(Some("review".into()), None).await?;
let id: SessionId = snapshot.record.info.id.clone();
let endpoint = EndpointId::parse("my-client").map_err(anyhow::Error::msg)?;

let mut subscription = host.subscribe_session(&id, endpoint.clone(), None).await?;
let accepted = host
    .prompt_session(&id, endpoint, MessageContent::text("Review this workspace"))
    .await?;
```

实际构造函数以 `dwo-agent-service` / `dwo-context` 当前公开 API 为准；关键点是 endpoint
标识一个临时观察者，而 Session 和 turn 仍由 Host 持有。调用方断开不等于 cancel。

公开的 Session 方法分为：

| API | 行为 |
| --- | --- |
| `list_sessions(all, caller)` | 列出全部 Session 或某个 caller 的直接子 Session |
| `list_session_statuses(all, caller)` | 带运行状态的列表 |
| `session_status(id)` | 单个 Session 的状态摘要 |
| `session_snapshot(id)` | 一致的当前快照 |
| `setup_session(title, cwd)` | 按 Host 默认配置建立根 Session；无 cwd 时创建 runtime workspace |
| `create_session(title, cwd)` | 建立 Session 并返回 `Arc<SessionAgent>` |
| `fork_session(source)` | 从 idle Session 复制 context/transcript |
| `subscribe_session(id, endpoint, cursor)` | 先取得快照/回放，再接收 live Session 事件 |
| `prompt_session(id, endpoint, content)` | 展开有效 Skill/MCP directive 后提交 prompt |
| `compact_session(id, endpoint)` | 启动一次显式上下文压缩 |
| `resume_session_turn(id, endpoint)` | 在允许时继续 idle turn |
| `cancel_session(id, expected_turn)` | 取消当前或指定 turn |
| `set_session_config(id, update)` | 更新 model、reasoning 或 policy 等 Session 选择 |
| `resolve_session_permission(...)` | 回答待处理的工具权限请求 |
| `publish_session_notification(...)` | 向 Session 写入结构化 Host/adapter 通知 |
| `close_session(id)` | 关闭已加载 actor，不删除持久记录 |
| `delete_session(id)` | 关闭并删除 Session 记录及生成的 workspace |
| `watch(session_id, endpoint_id, cursor)` | 面向字符串 transport 参数的订阅便捷方法 |

`SessionSubscription` 提供的 snapshot/checkpoint 与 live event 是恢复边界。自定义客户端应保存
cursor，并在重连时传回；不要把 transport 连接本身当作 Session 生命周期。

## Management transport API

外部管理请求应走：

```rust,ignore
use serde_json::json;

let result = host
    .handle_request(
        "my-transport:connection-7",
        "req-42",
        "config.snapshot",
        json!({}),
    )
    .await?;
```

| API | 适用场景 |
| --- | --- |
| `handle_request(client_id, request_id, method, params)` | 正式 transport；对有副作用的方法执行客户端范围的重试去重 |
| `handle_method(method, params)` | 已校验的进程内调用；不提供 request-id 去重 |

`handle_request` 的副作用缓存最多 1024 项，成功结果保留 5 分钟。同一 client/request id
如果用于不同 method 或 params 会返回冲突，而不是误返回旧结果。

transport 在调用 Host 前仍必须：

1. 解析并验证 `RpcRequest` 的 `jsonrpc`、route 和消息大小；
2. 使用 `dwo_protocol::method_allowed` 拒绝跨 route 方法；
3. 为每个连接生成稳定且不会和其他客户端冲突的 client id；
4. 把 `anyhow::Error` 转成 `RpcError`，保证一次请求只有 result 或 error；
5. 单独实现 `event.subscribe` 的长连接发送循环。

Model、Provider、Prompt、Rule、Skill、MCP、Automation 和 Channel 的具体管理实现目前是
Host 内部 API。crate 外二开应通过 `handle_request` 使用这些域；不要绕过 ConfigManager
直接写内存状态。这样人工编辑、API 修改、原子校验、watcher 和运行事件才保持一致。

## Host 事件

```rust,ignore
let (replay, mut live) = host.subscribe_events(last_cursor, 100, None).await;
for event in replay.events {
    println!("{} {}", event.seq, event.event);
}
while let Ok(event) = live.recv().await {
    // 按 seq 去重并持久化最后处理位置。
}
```

`subscribe_events(cursor, limit, event_filter)` 先创建 broadcast receiver，再读取 replay，避免
订阅建立过程丢事件。replay 与 live 在边界处可能包含相同 seq，消费者应按 seq 去重。

当前约束：

- 历史最多保留 1024 条；
- 单次 replay limit 会收紧到 1–200；
- live broadcast capacity 为 256；
- cursor 早于现存历史时 `EventReadResult.truncated` 为 `true`；
- 可通过 event 名称过滤 replay，live receiver 仍会收到所有 HostEvent。

`HostEvent` 包含单调递增的 `seq`、事件名 `event` 和 JSON `params`。稳定事件名由
`dwo_protocol::capabilities().events` 公布。

## WebSocket transport 支持

`WebsocketRuntime` 包含已验证的 listener 配置以及彼此独立的 ACP/Management token。
`dwo-websocket` 使用以下 API 管理 listener：

| API | 说明 |
| --- | --- |
| `websocket_snapshot()` | 当前无 secret 的 listener 配置 |
| `websocket_runtime()` | 配置加两枚 token；仅 transport owner 使用 |
| `set_websocket_running(bool)` | transport 回报实际 listener 状态 |
| `websocket_status()` | 可公开的状态 JSON |
| `websocket_set_enabled(bool)` | 原子修改 Host 配置并发布状态事件 |
| `websocket_config(update)` | 读取或校验后替换 listener 配置 |
| `websocket_token()` | 返回连接凭据，属于敏感管理操作 |
| `websocket_reset_token()` | 轮换两枚 token 并触发 listener reconcile |

WebSocket 是 transport，不是 Channel。新的 transport 不应注册为 `channel.<kind>`，也不应
获得绑定用户或当前 Session 之类的消息平台状态。

## Automation 与 Channel

`automation` 模块公开配置、Job、Schedule、Session 策略、运行记录和 `AutomationRuntime`，
适合实现新的调度器或做领域测试。正常嵌入应让 Host 创建并持有唯一 runtime，并通过
`automation.*` Management 方法修改配置。

`Host::channels()` 返回当前 `ChannelManager` handle，主要供 Host 内部 adapter/composition
使用。配置、绑定、启停和 secret 清理应走 `channel.<kind>.*`，不能只修改 manager。

## 增加 Host 能力

二开增加一个管理域或 transport 时，边界应保持如下：

1. 领域行为放在 Host 或对应 runtime，不放进 IPC/WebSocket handler。
2. 配置写入经过 ConfigManager 的校验和原子提交。
3. 在线方法注册进 `dwo-protocol`，让 route、capabilities 和副作用分类一致。
4. transport 只做 framing、认证、连接限制、route 校验、错误映射和事件转发。
5. 测试合法行为、可定位的非法输入、断线恢复、重试去重和有界资源。

不要为旧 RPC 名称、旧配置层级或旧 crate 路径增加兼容转发层。
