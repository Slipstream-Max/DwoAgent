# WebSocket 连接

WebSocket 是远程 transport，不是消息 channel。它提供两条互相隔离的入口：

- /acp：ACP v2，对话客户端使用；
- /dwo：Management RPC，桌面面板或运维脚本使用。

两条路径有独立 token，不能互相访问。

## 开启服务

在 [Profile 配置](profile.md#websocket) 中设置：

~~~yaml
websocket:
  enabled: true
  bind: 127.0.0.1
  port: 8787
~~~

重启或等待热加载后检查：

~~~text
dwo websocket status
dwo websocket token
~~~

token 保存在 runtime/websocket/secret.yaml，不会写入 profile。reset-token 会轮换两枚 token、
立即关闭旧连接。

## 连接地址

~~~text
ws://<bind>:<port>/acp?token=<acp-token>
ws://<bind>:<port>/dwo?token=<management-token>
~~~

每条 JSON 消息使用一个 WebSocket text frame。Binary frame 会被拒绝。/acp 固定使用 ACP v2；
/dwo 使用 JSON-RPC 2.0 envelope，具体方法见 [API 说明](api.md)。

JavaScript 示例：

~~~js
const socket = new WebSocket(
  'ws://127.0.0.1:8787/acp?token=' + encodeURIComponent(acpToken)
);
socket.onmessage = event => {
  const message = JSON.parse(event.data);
  console.log(message);
};
~~~

## 安全和部署

默认只监听 127.0.0.1。局域网使用时把 bind 改为局域网地址，并在防火墙放行对应 TCP 端口。
公网不要直接暴露明文 ws://；使用 Caddy、Nginx 等反向代理提供 TLS，再以 wss:// 连接。

query token 可能出现在代理访问日志中。请关闭完整 query string 记录，并把 token 当作密码管理。
连接关闭不会取消已经接受的 Session turn 或 Automation run。

## 诊断

~~~text
dwo websocket status
dwo websocket reset-token
dwo daemon status
~~~

如果 listener 未运行，先确认 enabled、bind 是有效 IP、port 大于 0，再查看 daemon 日志。
