# dwoagent 命令参考

## 总览

```text
dwoagent <command>
```

核心命令分三类：

- agent core：运行单个 profile host，左侧 ingress 统一转发到 `AgentService`。
- supervisor：管理机器级桌面后端 daemon。
- create/doctor/channel：创建配置、检查环境、登录外部 channel。

## Agent core

```bash
dwoagent agent run --agent-profile <path>
```

运行单个 agent profile host。默认开启 stdio JSON-RPC，同时启动 `agent.yaml` 里启用的 Weixin/Feishu channels 和 automation triggers。

没有独立的 `dwoagent acp` 或 `dwoagent worker` 顶层命令。ACP 兼容入口统一放在 supervisor 下，避免为同一个 profile 启动第二个 agent core。

## Supervisor

```bash
dwoagent create supervisor --default
```

创建默认 supervisor 配置到 `~/.dwoagent/supervisor.yaml`。

```bash
dwoagent create supervisor --path <path>
```

交互式创建 supervisor 配置到指定路径。

```bash
dwoagent supervisor enable
```

注册当前用户登录自启动。作用域是 supervisor，不是某个 agent profile。

```bash
dwoagent supervisor start
```

后台启动 supervisor。Windows 下通过隐藏 launcher 启动，避免弹出命令行窗口。

```bash
dwoagent supervisor status
```

查看自启动注册状态和正在运行的 supervisor 进程。

```bash
dwoagent supervisor acp --agent-profile <path>
```

ACP stdio 兼容入口。给编辑器或 ACP 客户端填 command 时使用。该命令只作为 shim 连接 supervisor，并由 supervisor 转发到已注册的 profile host。

```bash
dwoagent supervisor stop
```

停止当前运行的 supervisor 进程。

```bash
dwoagent supervisor disable
```

取消自启动注册。

## Create

```bash
dwoagent create agent --name <name>
```

交互式创建 agent profile。默认路径为 `~/.dwoagent/profiles/<name>`。

```bash
dwoagent create agent --name <name> --path <path>
```

交互式创建 agent profile 到指定路径。

## Doctor

```bash
dwoagent doctor
```

默认执行环境检查，等同于 `dwoagent doctor --check`。

```bash
dwoagent doctor --check
```

检查本地环境依赖，例如 `mcporter` 和 `rg`。

```bash
dwoagent doctor --resolve
```

交互式安装缺失的环境依赖。当前通过 npm 安装 `mcporter` 和 `ripgrep` 包；如果缺 npm，会提示先安装 Node.js/npm。

```bash
dwoagent doctor --resolve --yes
```

不询问，直接执行可自动化的修复步骤。

`doctor` 不创建 agent profile，也不注册 daemon。

## Channel login

```bash
dwoagent channel login weixin --agent-profile <path>
```

登录 Weixin channel，凭据写入该 profile 的 `runtime/channel_secret/`。

```bash
dwoagent channel login feishu --agent-profile <path> --app-id <id> --app-secret <secret>
```

保存 Feishu app 凭据。也可以通过 `FEISHU_APP_ID` 和 `FEISHU_APP_SECRET` 环境变量提供。

## 推荐初始化流程

```bash
dwoagent doctor --check
dwoagent create agent --name coder
dwoagent create supervisor --default
# 编辑 ~/.dwoagent/supervisor.yaml，把 coder profile 加入 profiles
dwoagent supervisor enable
dwoagent supervisor start
dwoagent supervisor status
```
