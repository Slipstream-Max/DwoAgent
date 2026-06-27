# Agent Profile 结构和配置

`dwo-agent` 的 agent profile 现在只保留三个顶层概念：

完整 `agent.yaml` 模板见 `docs/agent.full.yaml`。Supervisor 是机器级配置，见
`docs/supervisor-config.md` 和 `docs/supervisor.full.yaml`。

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
agentId: dwo-agent
name: dwo-agent
description: Dwo Agent runtime
policyMode: confirm
tools:
  fileEdit: enable
  terminal: enable
  subagent: enable
model:
  defaultModelId: deepseek-v4-pro
  models:
    - modelName: deepseek-v4-pro
      provider: deepseek
      modelId: deepseek-v4-pro
      apiKeyEnv: DEEPSEEK_API_KEY
      defaultReasoningMode: high
      compactThreshold: 0.8
```

顶层字段：

- `agentId`：必填 id，用于 session metadata 和展示。
- `name`：必填显示名称。
- `description`：必填简短描述。
- `policyMode`：必填权限策略。允许 `full_access`、`confirm`、`watch`。
- `maxRunningTurn`：可选正整数。省略时，agent loop 运行到模型停止、取消或出错。
- `sessionStoreDir`：可选，默认 `runtime/sessions`。
- Channel state 固定写入 `runtime/channel_state`，不提供 `agent.yaml` 配置项。
- `externalSkillsDirs`：可选，额外 skill roots，相对路径按 agent folder 解析。
- `externalRuleFiles`：可选，额外 rule 文件，相对路径按 agent folder 解析。
- `tools`：可选。支持 `fileEdit`、`terminal`、`subagent`，每个值为 `enable` 或 `disable`，默认启用。

运行时行为：

- `tools`、`maxRunningTurn`、policy mode、model id、reasoning mode、tool schemas 会在 session 创建时快照。
- 修改 `agent.yaml` 会影响新 session；已有 session 保留自己的持久化快照。
- 普通 session 写入 `<sessionStoreDir>/<year>/<month>/<day>/<sessionId>/`。

## model

`model` section 定义当前 agent 可用的模型别名。

```yaml
model:
  defaultModelId: deepseek-v4-pro
  models:
    - modelName: deepseek-v4-pro
      provider: deepseek
      modelId: deepseek-v4-pro
      apiKeyEnv: DEEPSEEK_API_KEY
      defaultReasoningMode: high
      compactThreshold: 0.8
```

- `defaultModelId`：必填默认模型别名，必须匹配某个 `modelName`。
- `models`：非空模型别名列表。
- `modelName`：会话使用的本地模型别名。
- `provider`：内置 provider catalog 里的 provider id。
- `modelId`：内置 provider catalog 里的 provider model id。
- `apiKeyEnv`：可选，读取 API key 的环境变量名。
- `apiKey`：可选，内联 API key。
- `apiBase`：可选，provider base URL 覆盖。
- `temperature`、`topP`、`timeoutSeconds`、`maxTokens`：可选请求参数。
- `defaultReasoningMode`：可选 reasoning mode，必须被所选 catalog model 支持。
- `compactThreshold`：可选上下文压缩阈值，范围 `(0, 1]`，默认 `0.8`。

运行时会把 `model` section 和内置 provider catalog 合并。catalog 提供模型能力、context window、max output tokens 和支持的 reasoning modes。

## policy

`policy` section 配置 terminal 命令黑白名单。它不决定当前 mode；默认 mode 仍由顶层 `policyMode` 决定。

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
    watchAllow:
      - exact: git status
      - prefix: git diff
      - prefix: Get-Content
```

- `full_access`：`terminal_exec.command` 命中 `deny` 就拒绝，否则执行。
- `confirm`：命中 `deny` 就拒绝；命中 `allow` 且是单条简单命令时直接执行；其他进入确认流程。
- `watch`：只有命中 `watchAllow` 且是单条简单命令时执行；其他拒绝。
- `fileEdit` 不读取 `policy.terminal`：`full_access` 直接执行，`confirm` 一律确认，`watch` 一律拒绝。
- subagent 固定继承父 session 的 mode，上限不能高于父 session。

## channels

`channels` section 只配置 profile host 的外部入口：Weixin 和 Feishu。ACP stdio 是 `dwoagent supervisor acp --agent-profile <path>` 的 shim/transport；桌面/UI WebSocket 属于 supervisor，不写入 agent profile。

```yaml
channels:
  weixin:
    enabled: false
    workspaceDir: .
    mediaInput: false
    mediaOutput: false
  feishu:
    enabled: false
    workspaceDir: .
    domain: feishu
```

如果 `channels` 缺失或为空，所有外部 channel 默认禁用。`dwoagent agent run` 仍会启动 stdio JSON-RPC profile host。
### Weixin

```yaml
channels:
  weixin:
    enabled: true
    workspaceDir: .
    markdownFilter: true
    mediaInput: true
    mediaOutput: true
    overrideModel: deepseek-v4-pro
    overrideReasoningMode: high
    defaultSessionId: null
```

运行时文件：

```text
runtime/channel_secret/weixin/auth.yaml
runtime/channel_state/weixin/context_tokens.json
runtime/channel_state/weixin/sync_buf.txt
runtime/channel_state/weixin/bridge_state.yaml
```

Weixin 不创建特殊会话目录。它通过 channel control state（兼容文件名 `bridge_state.yaml`）记录默认普通 session 和当前 `/switch` 绑定；真实对话、上下文和附件都保存在普通 session 中。附件下载到当前 active session 的 `attachments/inbox/weixin/<messageId>/`，并以 ACP `resource_link` 加入本轮输入。

### Feishu

```yaml
channels:
  feishu:
    enabled: true
    workspaceDir: .
    domain: feishu
    dmPolicy: allow_all
    groupPolicy: white_list
    allowFrom: ["*"]
    groupAllowFrom: []
    groupRequireMention: true
    mediaInput: true
    mediaOutput: true
    cardOutput: true
```

运行时文件：

```text
runtime/channel_secret/feishu/auth.yaml
runtime/channel_state/feishu/dm/<sender>/bridge_state.yaml
runtime/channel_state/feishu/group/<chat_id>/bridge_state.yaml
```

Feishu 私聊和群聊使用独立 channel state。真实对话、上下文和附件仍然保存在普通 session 中。开启 `mediaInput` 后，入站图片和文件会下载到当前 active session 的 `attachments/inbox/feishu/<messageId>/`。

Weixin 和 Feishu 支持：

```text
/help
/new
/list
/switch <sessionId>
/back
/where
/cancel
/approve <confirmation_id>
/deny <confirmation_id> [reason]
```

这些 slash commands 属于 Dwo channel control 语义；入站消息内容本身优先复用 ACP `ContentBlock`。

确认审计写入：

```text
runtime/channel_secret/audit/confirm_audit.jsonl
```

## automation

`automation` section 配置 `dwoagent agent run` 启动的 scheduler。Automation 不是 ingress channel；它只是按计划触发普通 agent session。

```yaml
automation:
  enabled: true
  jobs:
    - id: daily_digest
      enabled: true
      workspaceDir: .
      session:
        mode: new
      schedule:
        type: interval
        everySeconds: 3600
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
runtime/automation_state/<jobId>/state.yaml
```

每次 run 记录写入普通 session：

```text
runtime/sessions/<year>/<month>/<day>/<sessionId>/automation/<jobId>/runs/<runId>/run.yaml
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
agent.yaml externalRuleFiles entries
<cwd>/.agent/AGENTS.md
<cwd>/AGENTS.md
<cwd>/CLAUDE.md
```

每个非空 rule 文件会被包装成 `<rule>` 内的独立 source block，包含 source path 和转义后的内容。

## skills

Skills 是可选的。运行时从以下位置发现：

```text
resources/skills
agent.yaml externalSkillsDirs entries
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
agent.yaml externalRuleFiles entries
<cwd>/.agent/AGENTS.md
<cwd>/AGENTS.md
<cwd>/CLAUDE.md
resources/mcp.json
resources/skills
agent.yaml externalSkillsDirs entries
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

