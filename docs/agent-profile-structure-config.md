# Agent Profile 结构和配置

`dwo-agent` 的 agent profile 现在只保留三个顶层概念：

```text
<agent-folder>/
  agent.yaml
  resources/
    prompt/
      system.md                 # 必需，系统提示词
      AGENTS.md                 # 可选，profile 级 rule
    skills/                     # 可选
      <skill-name>/
        SKILL.md
    mcp.json                    # 可选，MCP 配置
  runtime/                      # 运行时生成，建议整体 gitignore
    sessions/
    channel_state/
    channel_secret/
    automation_state/
```

- `agent.yaml`：唯一配置入口，包含 agent 元信息、模型、policy、channels 和 automation。
- `resources/`：agent 的上下文资源。这里放 prompt、rule、skills 和 MCP 配置。
- `runtime/`：运行产物。这里放普通 session、channel state、channel secret 和 automation sticky state。

## agent.yaml

最小可用配置：

```yaml
agent_id: dwo-agent
name: dwo-agent
description: Dwo Agent runtime
policy_mode: confirm
tools:
  file_edit: enable
  terminal: enable
  subagent: enable
model:
  default_model_id: deepseek-v4-pro
  models:
    - model_name: deepseek-v4-pro
      provider: deepseek
      model_id: deepseek-v4-pro
      api_key_env: DEEPSEEK_API_KEY
      default_reasoning_mode: high
      compact_threshold: 0.8
```

顶层字段：

- `agent_id`：必填 id，用于 session metadata 和展示。
- `name`：必填显示名称。
- `description`：必填简短描述。
- `policy_mode`：必填权限策略。允许 `full_access`、`confirm`、`watch`。
- `max_running_turn`：可选正整数。省略时，agent loop 运行到模型停止、取消或出错。
- `session_store_dir`：可选，默认 `runtime/sessions`。
- `channel_state_dir`：可选，默认 `runtime/channel_state`。
- `external_skills_dirs`：可选，额外 skill roots，相对路径按 agent folder 解析。
- `external_rule_files`：可选，额外 rule 文件，相对路径按 agent folder 解析。
- `tools`：可选。支持 `file_edit`、`terminal`、`subagent`，每个值为 `enable` 或 `disable`，默认启用。

运行时行为：

- `tools`、`max_running_turn`、policy mode、model id、reasoning mode、tool schemas 会在 session 创建时快照。
- 修改 `agent.yaml` 会影响新 session；已有 session 保留自己的持久化快照。
- 普通 session 写入 `<session_store_dir>/<year>/<month>/<day>/<session_id>/`。

## model

`model` section 定义当前 agent 可用的模型别名。

```yaml
model:
  default_model_id: deepseek-v4-pro
  models:
    - model_name: deepseek-v4-pro
      provider: deepseek
      model_id: deepseek-v4-pro
      api_key_env: DEEPSEEK_API_KEY
      default_reasoning_mode: high
      compact_threshold: 0.8
```

- `default_model_id`：必填默认模型别名，必须匹配某个 `model_name`。
- `models`：非空模型别名列表。
- `model_name`：会话使用的本地模型别名。
- `provider`：内置 provider catalog 里的 provider id。
- `model_id`：内置 provider catalog 里的 provider model id。
- `api_key_env`：可选，读取 API key 的环境变量名。
- `api_key`：可选，内联 API key。
- `api_base`：可选，provider base URL 覆盖。
- `temperature`、`top_p`、`timeout_seconds`、`max_tokens`：可选请求参数。
- `default_reasoning_mode`：可选 reasoning mode，必须被所选 catalog model 支持。
- `compact_threshold`：可选上下文压缩阈值，范围 `(0, 1]`，默认 `0.8`。

运行时会把 `model` section 和内置 provider catalog 合并。catalog 提供模型能力、context window、max output tokens 和支持的 reasoning modes。

## policy

`policy` section 配置 terminal 命令黑白名单。它不决定当前 mode；默认 mode 仍由顶层 `policy_mode` 决定。

```yaml
policy:
  terminal:
    deny:
      - regex: '(?i)^\s*git\s+reset\s+--hard\b'
      - regex: '(?i)\bRemove-Item\b.*\b-Recurse\b'
    allow:
      - exact: git status
      - prefix: git diff
      - prefix: rg
      - prefix: cargo check
    watch_allow:
      - exact: git status
      - prefix: git diff
      - prefix: Get-Content
```

- `full_access`：`terminal_exec.command` 命中 `deny` 就拒绝，否则执行。
- `confirm`：命中 `deny` 就拒绝；命中 `allow` 且是单条简单命令时直接执行；其他进入确认流程。
- `watch`：只有命中 `watch_allow` 且是单条简单命令时执行；其他拒绝。
- `file_edit` 不读取 `policy.terminal`：`full_access` 直接执行，`confirm` 一律确认，`watch` 一律拒绝。
- subagent 固定继承父 session 的 mode，上限不能高于父 session。

## channels

`channels` section 配置 `dwo-agent serve` 启动的长生命周期入口。

```yaml
channels:
  stdio:
    enabled: true
    auth: true
  websocket:
    enabled: false
    bind_addr: 127.0.0.1:8765
    auth: true
  weixin:
    enabled: false
    workspace_dir: .
    media_input: false
    media_output: false
  feishu:
    enabled: false
    workspace_dir: .
    domain: feishu
```

如果 `channels` 缺失或为空，所有 channel 默认禁用。`serve` host 要求至少启用一个 channel 或一个 automation job。

### Stdio

```yaml
channels:
  stdio:
    enabled: true
    auth: true
```

登录生成 token：

```powershell
cargo run -- channel login stdio --agent-folder examples/dwo-agent
```

凭据写入：

```text
runtime/channel_secret/stdio/auth.yaml
```

`serve` 启动后会写入：

```text
runtime/channel_secret/stdio/daemon.yaml
```

### WebSocket

```yaml
channels:
  websocket:
    enabled: true
    bind_addr: 127.0.0.1:8765
    auth: true
```

凭据写入：

```text
runtime/channel_secret/websocket/auth.yaml
```

### Weixin

```yaml
channels:
  weixin:
    enabled: true
    workspace_dir: .
    markdown_filter: true
    media_input: true
    media_output: true
    override_model: deepseek-v4-pro
    override_reasoning_mode: high
    default_session_id: null
```

运行时文件：

```text
runtime/channel_secret/weixin/auth.yaml
runtime/channel_state/weixin/context_tokens.json
runtime/channel_state/weixin/sync_buf.txt
runtime/channel_state/weixin/bridge_state.yaml
```

Weixin 不创建特殊会话目录。它通过 `bridge_state.yaml` 记录默认普通 session 和当前 `/switch` 绑定；真实对话、上下文和附件都保存在普通 session 中。附件下载到当前 active session 的 `attachments/inbox/weixin/<message_id>/`，并以 `resource_link` 加入本轮输入。

### Feishu

```yaml
channels:
  feishu:
    enabled: true
    workspace_dir: .
    domain: feishu
    dm_policy: allow_all
    group_policy: white_list
    allow_from: ["*"]
    group_allow_from: []
    group_require_mention: true
    media_input: true
    media_output: true
    card_output: true
```

运行时文件：

```text
runtime/channel_secret/feishu/auth.yaml
runtime/channel_state/feishu/dm/<sender>/bridge_state.yaml
runtime/channel_state/feishu/group/<chat_id>/bridge_state.yaml
```

Feishu 私聊和群聊使用独立 channel state。真实对话、上下文和附件仍然保存在普通 session 中。开启 `media_input` 后，入站图片和文件会下载到当前 active session 的 `attachments/inbox/feishu/<message_id>/`。

Weixin 和 Feishu 支持：

```text
/list
/switch <session_id>
/back
/where
/approve <confirmation_id>
/deny <confirmation_id>
```

确认审计写入：

```text
runtime/channel_secret/audit/confirm_audit.jsonl
```

## automation

`automation` section 配置 `serve` 启动的 scheduler。Automation 不是 ingress channel；它只是按计划触发普通 agent session。

```yaml
automation:
  enabled: true
  jobs:
    - id: daily_digest
      enabled: true
      workspace_dir: .
      session:
        mode: new
      schedule:
        type: interval
        every_seconds: 3600
      prompt: "总结当前项目状态。"
      notify:
        - channel: weixin
```

`session.mode` 支持：

- `new`：每次创建新 session。
- `fixed`：复用指定 session id。
- `sticky`：第一次创建，后续复用同一个 session。

Sticky 状态写入：

```text
runtime/automation_state/<job_id>/state.yaml
```

每次 run 记录写入普通 session：

```text
runtime/sessions/<year>/<month>/<day>/<session_id>/automation/<job_id>/runs/<run_id>/run.yaml
```

## resources/prompt

系统提示词是必需文件：

```text
resources/prompt/system.md
```

它会作为 `<agent_prompt>` 插入 system context。文件必须存在且不能为空。

Profile 级 rule 是可选文件：

```text
resources/prompt/AGENTS.md
```

Rules 读取顺序：

```text
resources/prompt/AGENTS.md
agent.yaml external_rule_files entries
<cwd>/.agent/AGENTS.md
<cwd>/AGENTS.md
<cwd>/CLAUDE.md
```

每个非空 rule 文件会被包装成 `<rule>` 内的独立 source block，包含 source path 和转义后的内容。

## skills

Skills 是可选的。运行时从以下位置发现：

```text
resources/skills
agent.yaml external_skills_dirs entries
<cwd>/.agent/skills
```

每个 `SKILL.md` 必须以 YAML frontmatter 开头：

```yaml
---
name: code-review
description: Review code changes and call out concrete defects.
---
```

生成的 `<available_skills>` block 只包含每个 skill 的 name、description 和 `SKILL.md` location。模型可以据此决定是否打开对应 skill 文件。

## MCP

`resources/mcp.json` 是可选的 MCP 配置入口。运行时只检查这个文件是否存在，不解析、不读取文件内容，也不会自动注册 native MCP tool 或注入额外 skill。

如果存在，system context 和 env block watcher snapshot 会加入：

```xml
<mcp>
  <config>...</config>
  <usage>...</usage>
</mcp>
```

`<usage>` 会提示模型通过 terminal 检查或安装 `mcporter`，并使用：

```powershell
mcporter --config "<config-path>" list --json
mcporter --config "<config-path>" list <server> --schema --json
mcporter --config "<config-path>" call <server.tool> --args '<json>' --output json
```

## Env Block Watcher

每个主会话都会启动一个 env block watcher。它观察这些动态上下文来源：

```text
resources/prompt/AGENTS.md
agent.yaml external_rule_files entries
<cwd>/.agent/AGENTS.md
<cwd>/AGENTS.md
<cwd>/CLAUDE.md
resources/mcp.json
resources/skills
agent.yaml external_skills_dirs entries
<cwd>/.agent/skills
```

当 snapshot 变化时，它会在 agent-loop 边界追加一条 watcher system message，不会改写 stable system prefix。

```xml
<watcher_content>
<env_block>
  <rule>...</rule>
  <mcp>...</mcp>
  <available_skills>...</available_skills>
  <env_context>...</env_context>
</env_block>
</watcher_content>
```

## Generated System Context

新 session 的 system context 由这些 blocks 构成：

```text
<agent_context>
  <agent_prompt>...</agent_prompt>
  <rule>...</rule>
  <tools>...</tools>
  <mcp>...</mcp>
  <available_skills>...</available_skills>
  <env_context>...</env_context>
</agent_context>
```

`<mcp>` 只在 `resources/mcp.json` 存在时出现。
