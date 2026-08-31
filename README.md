<p align="center">
  <img src="assets/welcome.svg" alt="赤铎 Dwo Agent">
</p>

<p align="center">
  <strong>一个常驻设备、支持多入口接力的 Agent Runtime。</strong><br>
  Rust 原生；CLI、IDE、微信、Telegram、飞书/Lark 和 QQ Bot 共用同一批 Session。
</p>

## 安装

需要 Rust 1.95 或更新版本，以及模型服务的 API Key。

~~~bash
git clone https://github.com/Slipstream-Max/dwoagent.git
cd dwoagent
cargo build --release -p dwo-agent
~~~

Windows PowerShell：

~~~powershell
$env:DEEPSEEK_API_KEY = "your-key"
.\target\release\dwo.exe install --start
.\target\release\dwo.exe daemon status
~~~

macOS / Linux：

~~~bash
export DEEPSEEK_API_KEY="your-key"
./target/release/dwo install --start
~/.dwoagent/bin/dwo daemon status
~~~

Windows ARM64 必须在 Visual Studio Developer PowerShell 中执行 Cargo，确保 ring 等原生依赖
能找到 ARM64 C 工具链。

install 会把程序和默认配置安装到 ~/.dwoagent/。Windows 会注册登录启动任务，macOS 使用
LaunchAgent；Linux 当前只安装文件，不注册系统服务。

## 最小配置

默认配置文件是 ~/.dwoagent/profile.yaml：

~~~yaml
policyMode: confirm
model:
  default:
    model: deepseek/deepseek-v4-pro
  providers:
    deepseek:
      apiKeyEnv: DEEPSEEK_API_KEY
~~~

修改后检查：

~~~text
dwo config-show
dwo daemon status
~~~

完整字段、默认值和目录结构见 [Profile 配置](docs/profile.md)。新增模型和声明能力见
[模型与 Provider](docs/models.md)。

## 开始使用

从终端提交任务：

~~~text
dwo session prompt "检查这个项目并说明结构" --cwd <path>
dwo session list
dwo session watch <session-id>
~~~

其他入口：

| 入口 | 文档 |
| --- | --- |
| IDE / ACP Client | [ACP 连接](docs/acp.md) |
| 微信、Telegram、飞书/Lark、QQ | [Channel 配置与行为](docs/channels.md) |
| 远程 ACP / Management RPC | [WebSocket 连接](docs/websocket.md) |
| 所有 CLI 命令 | [CLI 命令参考](docs/cli.md) |

完整文档目录见 [docs/README.md](docs/README.md)。

## 开发

~~~powershell
cargo fmt --all
cargo test --workspace
~~~

Windows ARM64 请在 Visual Studio Developer PowerShell 中运行以上命令。
