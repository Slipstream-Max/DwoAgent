<p align="center">
  <img src="assets/welcome.svg" alt="赤铎 Dwo Agent">
</p>

<p align="center">
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-000000?style=for-the-badge&amp;logo=rust&amp;logoColor=white" alt="Rust"></a>
  <a href="https://github.com/Slipstream-Max"><img src="https://img.shields.io/badge/Built%20by-Slipstream__Max-B7410E?style=for-the-badge&amp;logo=github&amp;logoColor=white" alt="Built by Slipstream_Max"></a>
  <a href="https://github.com/Slipstream-Max/DwoAgent"><img src="https://img.shields.io/badge/Agents-Dwo-D4A017?style=for-the-badge&amp;logo=dependabot&amp;logoColor=white" alt="Agents: Dwo"></a>
</p>

<p align="center">
  <strong>小巧、常驻设备、支持所有必须功能的最简Agent</strong>
</p>

## 🤔 为什么做了赤铎

一直想要一个能常驻在设备上的 Agent——体积小、性能好、功能完整，随叫随到。可看了一圈现成的，总觉得差点意思。

OpenClaw 功能很全，代价是常驻内存、组件数量和整体复杂度也跟着上去了：CLI、Web UI、gateway 和 runtime 全塞在一个大项目里，想把 Agent core 单独拎出来，牵一发动全身。另一头呢，很多轻量的 claw 项目只做了最基本的 agent loop——session、skills、子agent、权限、消息渠道、多端协作，这些日常真要用起来的东西却没补齐。各种厂商成熟的 Agent 呢？确实好用，可一切都已经定型，想从头调 system prompt、工具和存储，也得脱层皮。🫠

于是就有了赤铎：保留日常使用需要的功能：多平台互联、工具支持、子agent，克制但是必要的工具集合，提供了配置prompt，工具的接口。用户只需要稍加配置就能作为长期使用的个人 Agent。而我们的core和上层的抽象解耦，可以剥离开，方便继续开发和做实验。

按需拼装——单独用也行，组合起来就是一个完整的个人 Agent。✨

## ✨ 特点

| 亮点 | 说明 |
| --- | --- |
| **Rust 原生实现** | daemon 和 agent loop 运行在同一个原生二进制中，基础 daemon 实际常驻内存约 `6 MB`，也能处理多个并行 session。内存占用会随活跃 session、channel 连接变化。 |
| **克制的内置工具** | 直接执行的内置工具只有 terminal、read_file、file_edit 和 plan；更多能力通过 skills 和外部命令按需扩展。 |
| **模型与 Provider** | 内置 OpenAI、DeepSeek、Grok、Qwen、智谱等 Model List，统一走 Responses API；本地工具与 Provider 托管的 Web Search 可以出现在同一轮，并作为正常工具事件回放。 |
| **多平台接入** | 本地支持 CLI 和 ACP（IDE），远程支持微信、Telegram、飞书/Lark、QQ Bot；WebSocket 另有 /acp 和 /dwo 两条独立入口；图片和文件也能进入 session。 |
| **多端接力** | 同一个 session 可以同时被 CLI、ACP 和各个 channel 使用：任意已连接端点都能查看进度、取消任务、处理权限请求和继续发消息；运行中的新消息按 FIFO 排队，在模型响应或工具调用的边界加入当前 turn。 |
| **子 Agent** | 子 session 有独立 context 和 transcript，可以继续派生；父 session 不需要轮询，子任务结果会自动作为内部消息送达。支持 Fork 和一次性临时子 Agent（`--ephemeral`）。 |
| **Project、Section 与 Worktree** | Project 持久化工作路径、Section、Session assignment、Repository 和 Git Worktree；同一仓库可以用不同 worktree 让多个 session 并行工作。Automation 另存于 project 或 global scope。 |
| **Automation 定时任务** | 每个 Project 有独立的 cron 任务、Session 策略和执行历史；按无人值守方式运行，超时自动取消，需要人工确认的权限请求自动拒绝。 |
| **持久化会话** | 模型上下文和完整 transcript 分开存储；上下文压缩和模型切换都不会丢原始记录。 |
| **三种权限模式** | `full_access` 适合可信环境，`confirm` 会请求确认，`watch` 只开放简单的只读操作；子 Agent 只能收紧、不能放宽。 |
| **文件化配置** | system prompt、`AGENTS.md`、skills 和模型都放在固定目录里，改完由 watcher 热加载进现有 session。 |
| **可嵌入 Core** | `dwo-agent-service` 是独立的 session 运行时，暴露 session、repository、model、event 和 config API；宿主二进制通过本地 IPC 托管它，并把 ACP、微信等入口适配进来，桌面应用、IDE、服务端程序或实验项目都可以直接复用。 |

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
