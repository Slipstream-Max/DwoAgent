# Supervisor 配置

`supervisor.yaml` 是机器级配置，不属于任何一个 agent profile。它描述桌面/UI 后端怎么监听、有哪些 profile 可以被调度，以及 worker 池如何回收。

默认路径：

```text
~/.dwoagent/supervisor.yaml
```

创建默认配置：

```bash
dwoagent create supervisor --default
```

完整模板见 `docs/supervisor.full.yaml`。

## 最小配置

```yaml
version: 1
endpoint:
  websocketBindAddr: 127.0.0.1:8766
  secret: dwo_sup_xxx
profiles:
  - id: coder
    path: C:\Users\you\.dwoagent\profiles\coder
defaultProfile: coder
pool:
  maxWorkers: 3
  idleSeconds: 600
```

## 字段

- `version`：配置版本。当前为 `1`。
- `endpoint.websocketBindAddr`：supervisor WebSocket 监听地址。桌面 UI 和 shim 连接这里。
- `endpoint.secret`：请求鉴权 secret。空字符串表示不校验，推荐仅开发时使用。
- `profiles`：profile 注册表。supervisor 只按这里声明的 id/path 调度 profile host。
- `profiles[].id`：请求里的 profile id，例如 `coder`。
- `profiles[].path`：agent profile 目录，目录内应包含 `agent.yaml`。
- `defaultProfile`：请求未带 `profile` 时使用的 profile id。可为空，但此时请求必须显式带 `profile`。
- `pool.maxWorkers`：最多保留的 worker 数量。超过时按最久未使用 worker 做 LRU 回收。
- `pool.idleSeconds`：worker 空闲超过该秒数后回收。

## 运行关系

```text
Desktop UI / shim
  -> supervisor WebSocket endpoint
  -> profiles[id].path
  -> dwoagent agent run --agent-profile <path>
  -> stdio RPC / channels / automation
```

supervisor 是 OS 自启动 daemon 的作用域；agent profile 不是 daemon 作用域。一个 supervisor 可以懒加载多个 profile host。

## WebSocket 消息

所有请求都应该带 `secret`，除非配置里的 secret 为空。

```json
{"id":1,"type":"profiles.list","secret":"dwo_sup_xxx"}
{"id":2,"type":"worker.request","secret":"dwo_sup_xxx","profile":"coder","method":"session/new","params":{"cwd":".","mcpServers":[]}}
{"id":3,"type":"worker.request","secret":"dwo_sup_xxx","profile":"coder","method":"session/prompt","params":{"sessionId":"...","prompt":[{"type":"text","text":"hello"}]}}
{"id":4,"type":"worker.request","secret":"dwo_sup_xxx","profile":"coder","method":"_dwo/session/context","params":{"sessionId":"..."}}
```

长任务会先推 `supervisor.event`，最后推 `supervisor.result`：

```json
{"id":3,"type":"supervisor.event","profile":"coder","event":{"method":"session/update","params":{}}}
{"id":3,"type":"supervisor.result","result":{"profile":"coder","result":{"stopReason":"end_turn"}}}
```

## 边界

- agent profile 的模型、工具、MCP、微信、飞书写在 `agent.yaml`。
- supervisor 的 endpoint、secret、profile registry、worker pool 写在 `supervisor.yaml`。
- ACP stdio 不写入 `supervisor.yaml`；`dwoagent supervisor acp --agent-profile <path>` 通过 supervisor 转发。
