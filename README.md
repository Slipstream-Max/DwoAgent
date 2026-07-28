<p align="center">
  <img src="assets/logo.svg" alt="赤铎 Dwo Agent" width="760">
</p>

# 赤铎 · Dwo Agent

<p align="center">
  Rust 写的轻量 Agent Runtime<br>
  本地能用，远程能聊，也方便嵌入现有软件
</p>

---

赤铎（Dwo Agent）是一个用 Rust 写的轻量 Agent Runtime，可以常驻在本机或服务器上。CLI、ACP、微信、Telegram 和飞书/Lark 共享同一批 session，远程和本地可以随时接着使用。

模型调用、终端、文件编辑、MCP、skills、subsessions、权限控制、上下文压缩和定时任务都已经接好。任务可以从 IDE 开始，再从手机继续；core 也可以嵌入其他程序，并按需要调整 system prompt、agent step、存储和工具。

## 为什么做赤铎

赤铎是一个体积小、性能好、功能完整的 Agent 运行时。Core 保留日常使用需要的功能，稍加配置就能作为长期使用的个人 Agent。清楚的 Rust API 也方便继续开发和做实验。

OpenClaw 的功能很多，常驻内存、组件数量和整体复杂度也比较高。CLI、Web UI、gateway 和 runtime 混在一个大项目里时，单独取出 Agent core 会比较费力。很多轻量的 claw 项目只完成了基本 agent loop，session、MCP、skills、subsessions、权限、消息渠道和多端协作还没有补齐。另一些成熟 Agent 已经形成固定产品形态，从头调整 system prompt、agent step、工具和存储同样需要花不少时间。

赤铎主要提供 Agent Runtime。CLI 控制 daemon，ACP 和 channel 处理对话，`dwo-agent-service` 提供可嵌入的 session core。各部分可以单独使用，也能组合成一个完整的个人 Agent。

## 特点

| 亮点 | 说明 |
| --- | --- |
| **Rust 原生实现** | daemon 和 agent loop 运行在同一个原生二进制中，基础 daemon 实际常驻内存约 `6 MB`，也能处理多个并行 session。内存占用会随活跃 session、channel 和 MCP 连接变化。 |
| **完整 Agent 功能** | 内置终端、文件编辑、MCP、skills、subsessions、上下文压缩、权限控制和 cron automation。 |
| **本地与远程入口** | 本地支持 ACP 和 CLI，远程支持微信、Telegram、飞书/Lark，图片和文件也可以进入 session。 |
| **Session 原生广播** | ACP 和各个 channel 可以同时订阅同一个 session，已连接端点都能查看进度、取消任务、处理权限请求和继续发送消息。 |
| **运行中继续操作** | 新消息按顺序进入队列，在模型响应或工具调用的边界加入当前 turn。一个入口等待回复时，其他入口仍可正常操作。 |
| **持久化会话** | 当前模型上下文和完整 transcript 分开存储，压缩上下文不会删除原始记录。 |
| **三种权限模式** | `full_access` 适合可信环境，`confirm` 会请求确认，`watch` 只开放简单的只读操作。 |
| **可嵌入 Core** | `dwo-agent-service` 暴露 session、repository、model、event 和 config API，可用于桌面应用、IDE、服务端程序和实验项目。 |
| **文件化配置** | system prompt、`AGENTS.md`、skills、模型和 MCP 都有固定目录，修改后由 watcher 更新现有 session。 |

## 关于记忆

赤铎目前没有额外加入长期记忆系统，也没有内置向量库、用户画像或自动提取并写回记忆的流程。现阶段还没有找到在准确性、可控性、成本和维护复杂度上都适合日常使用的方案。

Session 会保存完整 transcript 和当前模型上下文，明确需要长期保留的内容可以放进 system prompt、`AGENTS.md` 或 skills。以后出现经过实际使用验证的记忆方案，再考虑加入对应能力。

## Build & Install

需要 Rust `1.95` 或更新版本。

```bash
git clone https://github.com/Slipstream-Max/dwoagent.git
cd dwoagent
cargo build --release -p dwo-agent
```

Windows PowerShell：

```powershell
.\target\release\dwo.exe install --start
.\target\release\dwo.exe daemon status
```

macOS / Linux：

```bash
./target/release/dwo install --start
~/.dwoagent/bin/dwo daemon status
```

`install` 会把可执行文件和默认 profile 安装到 `~/.dwoagent/`。Windows 会将 `~/.dwoagent/bin` 加入用户 PATH 并注册登录启动任务；macOS 使用 LaunchAgent。Linux 当前不会修改 PATH 或注册系统服务，可以把 `~/.dwoagent/bin` 加入 PATH，或用 `dwo serve` 前台运行。

默认模型需要 `DEEPSEEK_API_KEY`。在启动 daemon 的环境中设置它：

```powershell
$env:DEEPSEEK_API_KEY = "your-key"
dwo daemon start
```

```bash
export DEEPSEEK_API_KEY="your-key"
dwo daemon start
```

模型、权限和 channel 都在 `~/.dwoagent/profile.yaml` 配置，完整字段见 [Profile 配置指南](docs/profile.md)。

> Windows ARM64 构建需要进入 Visual Studio Developer PowerShell，并选择 ARM64 host/toolchain，使 `ring` 等原生依赖能够找到 C 编译器。

## Profile Structure

`dwo install` 默认创建 `~/.dwoagent/`：

```text
~/.dwoagent/
|- profile.yaml
|- bin/
|- resource/
|  |- prompts/
|  |  |- System.md
|  |  `- AGENTS.md
|  |- skills/<skill>/SKILL.md
|  `- mcp.json
|- runtime/
|  |- sessions/YYYY/MM/DD/<session-id>/
|  |- workspaces/<session-id>/
|  |- attachments/<channel>/
|  |- channel-capabilities/
|  |- mcp/
|  `- logs/
`- channels/<channel>/
   |- runtime.yaml
   `- secret.yaml
```

| 路径 | 用途 |
| --- | --- |
| `profile.yaml` | Profile 名称、默认权限、模型、channels 和 automation。 |
| `resource/prompts/System.md` | Agent 的主 system prompt。 |
| `resource/prompts/AGENTS.md` | Profile 级规则，可留空。工作目录中的 `AGENTS.md` 也会加载。 |
| `resource/skills/` | 本地 skills，每个 skill 使用独立目录和 `SKILL.md`。 |
| `resource/mcp.json` | stdio、HTTP 和 OAuth MCP server 配置。 |
| `runtime/` | Session、附件、MCP catalog、OAuth 和日志等运行数据。 |
| `channels/` | 各 channel 的绑定信息和当前 session。Telegram token 与飞书 App 凭据从环境变量读取。 |

通常只需要编辑 `profile.yaml` 和 `resource/`。`runtime/`、channel state 和 secret 文件由 daemon 管理。完整目录、YAML 字段、模型和运行数据说明见 [Profile 配置指南](docs/profile.md)。

## CLI 快速上手

启动和检查服务：

```text
dwo daemon start
dwo daemon status
dwo daemon stop
dwo serve                  # 前台运行 daemon，适合调试
```

从终端提交第一项任务：

```powershell
dwo session prompt "检查这个项目并说明它的结构" --cwd C:\path\to\project --title "project review"
dwo session list
dwo session watch <session-id>
```

CLI 的 `prompt` 用来提交任务，`watch` 用来读取最新事件。需要连续聊天时，可以直接接 ACP 或消息 channel。

常用管理命令：

```text
dwo profile-list
dwo session list [--all]
dwo session cancel <session-id>
dwo session delete <session-id>

dwo mcp search <query>
dwo mcp call <server.tool> --args '<json>'
dwo mcp auth <server> [--logout]

dwo automation list
dwo automation run <job>
dwo channel list
```

所有参数和行为见 [CLI 命令参考](docs/commands.md)。

## 如何对话

### ACP：从 IDE 或 ACP 客户端连接

先确保 daemon 正在运行，然后把下面的进程配置为客户端的 ACP agent：

```text
command: dwo
args: [acp]
```

使用自定义 profile：

```text
command: dwo
args: [--config-path, /path/to/profile.yaml, acp]
```

`dwo acp` 通过 stdio 接入现有 daemon，共享 session、模型配置、工具事件和权限请求。具体能力、客户端配置要点与限制见 [ACP 使用指南](docs/acp.md)。

### Channels：从聊天应用连接

赤铎目前支持微信、Telegram 和飞书/Lark 私聊。启用 channel 后重启 daemon，再完成绑定：

```text
dwo channel weixin bind       # 终端扫码
dwo channel telegram bind     # 私聊机器人发送一次性 /bind <code>
dwo channel feishu bind       # 私聊机器人发送一次性 /bind <code>
```

Telegram 使用 long polling，飞书/Lark 使用 WebSocket 长连接，都不需要公网 webhook。环境变量、开放平台权限和完整部署步骤见 [Channel 部署与使用](docs/channels.md)。

所有 channel 共用下面这组 slash commands：

| 命令 | 用途 |
| --- | --- |
| `/help` | 显示命令列表 |
| `/list` | 列出 session，并标记当前选择 |
| `/new [名称] [--cwd <路径>]` | 创建并选择一个 session |
| `/use <session-id>` | 切换 session，并回放最近对话 |
| `/status` | 查看当前 session、模型和运行状态 |
| `/model <名称>` | 切换当前 session 的模型 |
| `/reasoning <级别\|off>` | 调整或关闭 reasoning |
| `/policy [full_access\|confirm\|watch]` | 查看或修改工具权限策略 |
| `/allow [request-id]` | 允许当前或指定的权限请求 |
| `/deny [request-id]` | 拒绝当前或指定的权限请求 |
| `/cancel` | 取消正在运行的 turn |
| `/del <session-id>` | 删除 session |

普通文本直接作为 prompt 发送。在 `confirm` 模式下，Agent 请求执行敏感工具时，可以直接回复 `/allow` 或 `/deny`。

## Automation

定时任务写在 `~/.dwoagent/profile.yaml`。下面的任务每天 9:00 创建一个 session，并检查项目状态：

```yaml
automation:
  enabled: true
  jobs:
    - name: daily-report
      schedule:
        cron: "0 9 * * *"
        timezone: Asia/Shanghai
      session:
        mode: new
        cwd: projects/demo
        title: Daily report
      prompt: 检查项目状态并整理今天需要处理的事项。
```

| Session 模式 | 用法 |
| --- | --- |
| `new` | 每次运行创建新 session，可设置 `cwd` 和 `title`。 |
| `fixed` | 把 prompt 投递到指定 `sessionId`，适合持续更新同一项任务。 |

```text
dwo automation list
dwo automation status
dwo automation run daily-report
```

Cron 使用 `分钟 小时 日期 月份 星期` 五个字段。Daemon 会读取配置变化并更新执行时间。Automation 无人值守运行，遇到工具权限确认时会自动拒绝，避免任务长期等待。完整字段和 fixed session 示例见 [Automation 使用指南](docs/automation.md)。

## 运行方式

```text
                         +------------------+
 ACP client ------------>|                  |
 CLI ------------------->|    dwo daemon    |----> model provider
 Weixin ---------------->|                  |----> terminal / file tools
 Telegram -------------->| sessions + MCP   |----> managed MCP servers
 Feishu/Lark ----------->| + automation     |
                         +------------------+
```

所有入口共享 session，每个 channel 会单独保存当前选择的 session。没有显式 `cwd` 的 session 使用 `runtime/workspaces/<session-id>/`；完整 transcript 与当前模型上下文分别持久化，模型压缩不会删除 transcript。

## Core 嵌入

真正运行 session 和 agent loop 的代码在 `crates/dwo-agent-service`。它提供这些公开类型：

- `AgentService`：创建、加载、列出和删除 session。
- `SessionAgent`：提交 prompt、订阅事件、取消 turn、修改配置和处理权限请求。
- `SessionRepository`：session 存储接口，项目内已经有内存和文件系统实现。
- `ModelClient`：模型接口，可以使用 profile 创建默认 client，也可以接入自定义实现。
- `SessionSubscription`：先返回完整 snapshot，再持续接收广播事件。

`dwo-agent` crate 在 core 外面加上 daemon、IPC、CLI、ACP、channels、MCP 和 automation。只需要 Agent 能力时，可以直接依赖 core；需要完整个人 Agent 时，运行 `dwo` daemon 即可。

System prompt 位于 `resource/prompts/System.md`，项目规则位于 `resource/prompts/AGENTS.md`，skills 和 MCP 也使用独立目录。做新的 agent step、上下文策略、工具策略或客户端时，可以从对应 crate 开始修改，不需要先拆开一个完整的 Web 产品。

## 项目说明

赤铎来自实际使用中的需求，目前也用于作者的日常 Agent 工作流。后续开发会继续做好稳定常驻、多端协作、低开销和扩展能力。

## 文档

| 文档 | 内容 |
| --- | --- |
| [文档索引](docs/README.md) | 按首次使用、日常操作和深入理解组织的阅读入口 |
| [ACP 使用指南](docs/acp.md) | ACP 启动、session、权限、内容类型与限制 |
| [Channel 部署与使用](docs/channels.md) | 微信、Telegram、飞书/Lark 部署和 slash commands |
| [Automation 使用指南](docs/automation.md) | Cron、时区、新建/固定 session 和无人值守行为 |
| [CLI 命令参考](docs/commands.md) | daemon、session、MCP、channel、automation 命令 |
| [Profile 配置指南](docs/profile.md) | 完整 profile.yaml、资源目录、模型、MCP 与运行数据 |

## Development

```powershell
cargo fmt --all
cargo test --workspace
```

工作区按功能拆成几个 crate：`dwo-agent` 包含 daemon、CLI 和 adapters，`dwo-agent-service` 包含 session actor 和 agent loop，`dwo-context`、`dwo-model-client`、`dwo-mcp`、`dwo-tools` 与 `dwo-pty` 分别处理上下文、模型、MCP、工具和终端。
