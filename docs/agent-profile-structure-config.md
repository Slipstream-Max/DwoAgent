# Agent Profile 结构和配置

本文档说明 `dwo-agent` 使用的 agent profile 文件夹结构：必需文件、可选资源、workspace 覆盖文件，以及各部分什么时候被加载。

## 文件夹结构

`--agent-folder` 可以直接指向一个 agent structure 目录，也可以指向一个包含 `agent-structure/` 子目录的文件夹。有效的 agent structure 目录必须包含：

```text
<agent-folder>/
  agent.yaml
  model.yaml
  policy.yaml                  # 可选
  channels.yaml                # 可选
  resources/
    agents/
      <agent_id>.agent.md
      <agent_id>.rule.md        # 可选
    skills/                     # 可选
      <skill-name>/
        SKILL.md
```

等价的嵌套结构：

```text
<agent-folder>/
  agent-structure/
    agent.yaml
    model.yaml
    policy.yaml                # 可选
    channels.yaml              # 可选
    resources/
      agents/
```

运行时启动时会解析 agent structure 目录，并把它作为 `session_store_dir`、`channel_session_dir` 等相对路径的基准目录。

## 运行模式

同一个 agent profile 可以用不同 host 模式启动：

```powershell
cargo run -- acp --agent-folder examples/dwo-agent
cargo run -- serve --agent-folder examples/weixin-agent
cargo run -- channel login weixin --agent-folder examples/weixin-agent
cargo run -- channel login feishu --agent-folder examples/weixin-agent --app-id cli_xxx --app-secret xxx
```

- `acp`：通过 stdio 运行 ACP，并从 ACP `session/new` 创建会话。
- `serve`：启动由 `channels.yaml` 配置的长生命周期 ingress channel。
- `channel login weixin`：执行微信扫码登录，并把凭据写入当前 agent profile。
- `channel login feishu`：保存飞书应用凭据到当前 agent profile。

## agent.yaml

`agent.yaml` 定义 agent profile 元数据和运行时默认值。

```yaml
agent_id: dwo-agent
name: dwo-agent
description: Dwo Agent runtime
policy_mode: confirm
session_store_dir: sessions
channel_session_dir: channel_sessions
max_running_turn: 8
external_skills_dirs:
  - shared/skills
external_rule_files:
  - shared/rules/common.md
tools:
  file_edit: enable
  terminal: enable
  subagent: enable
```

字段说明：

- `agent_id`：必填 id，同时用于选择 `resources/agents/` 下的文件。
- `name`：必填显示名称。
- `description`：必填简短描述。
- `policy_mode`：必填权限策略。允许值：`full_access`、`confirm`、`watch`。
- `session_store_dir`：可选，普通 agent session 存储根目录。默认 `sessions`。相对路径按 agent structure 目录解析。
- `channel_session_dir`：可选，channel session 存储根目录。默认 `channel_sessions`。相对路径按 agent structure 目录解析。
- `max_running_turn`：可选正整数。省略时，agent loop 会一直运行，直到模型停止、会话取消或发生错误。
- `external_skills_dirs`：可选，额外 skill root 目录列表。相对路径按 agent structure 目录解析。不存在的目录会被忽略。
- `external_rule_files`：可选，额外 rule 文件列表。相对路径按 agent structure 目录解析。不存在或为空的文件会被忽略。
- `tools`：可选对象。省略时默认 `enable`。支持 `file_edit`、`terminal`、`subagent`，每个值为 `enable` 或 `disable`。

会话行为：

- `tools`、`max_running_turn`、policy mode、model id、reasoning mode、tool schemas 会在会话创建时快照。
- 修改 `agent.yaml` 会影响新会话；已有会话继续使用持久化的运行时工具快照，除非代码显式迁移它们。
- `session_store_dir` 只影响 ACP/普通 session 的创建、加载和列表；`channel_session_dir` 只影响 channel 自己维护的持久化 session。
- 运行时可以通过 session API 修改 policy mode；这是会话状态变更，不是重新读取 `agent.yaml`。

## policy.yaml

`policy.yaml` 是可选文件，用于配置 terminal 命令的黑白名单。它不决定当前 mode；默认 mode 仍由 `agent.yaml` 的 `policy_mode` 决定。

```yaml
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
    - prefix: git show
    - prefix: git log
    - prefix: rg
    - prefix: Get-Content
    - prefix: Select-String
    - prefix: Get-ChildItem
```

规则语义：

- `full_access`：`terminal_exec.command` 命中 `deny` 就拒绝，否则执行。
- `confirm`：`terminal_exec.command` 命中 `deny` 就拒绝；命中 `allow` 且是单条简单命令时直接执行；其他进入确认流程。
- `watch`：`terminal_exec.command` 只有命中 `watch_allow` 且是单条简单命令时执行；其他拒绝。
- `allow` 和 `watch_allow` 不会放行多命令、管道、重定向、命令替换或带 `env` 覆盖的 command；这些在 `confirm` 下会进入确认，在 `watch` 下会拒绝。
- `file_edit` 不读取 `policy.yaml`：`full_access` 直接执行，`confirm` 一律确认，`watch` 一律拒绝。
- subagent 固定继承父 session 的 mode，上限不能高于父 session；subagent 内部的 terminal/file tool call 继续使用同一份 `policy.yaml`。

## model.yaml

`model.yaml` 定义当前 agent 可用的模型别名。

```yaml
default_model_id: deepseek-v4-pro
models:
  - model_name: deepseek-v4-pro
    provider: deepseek
    model_id: deepseek-v4-pro
    api_key_env: DEEPSEEK_API_KEY
    default_reasoning_mode: high
    compact_threshold: 0.8
```

字段说明：

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

运行时会把 `model.yaml` 和内置 provider catalog 合并。catalog 提供模型能力、context window、max output tokens 和支持的 reasoning modes。

## channels.yaml

`channels.yaml` 是可选文件，用于配置 `dwo-agent serve` 启动的长生命周期 service ingress channels。

```yaml
weixin:
  enabled: true
  workspace_dir: .
  markdown_filter: true
  media_input: true
  media_output: true
  response_detail: response_only
  override_model: deepseek-v4-pro
  override_reasoning_mode: high

websocket:
  enabled: false
  bind_addr: 127.0.0.1:8765
  auth: true

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
  response_detail: response_only
  override_model: deepseek-v4-pro
  override_reasoning_mode: high
```

顶层配置：

- `weixin`：微信单用户 assistant channel。
- `websocket`：ACP-over-WebSocket channel，协议行为与 `acp` stdio 相同。
- `feishu`：飞书/Lark assistant channel。

如果 `channels.yaml` 不存在或为空，所有 channel 默认禁用。`serve` host 要求至少启用一个 channel。

### WebSocket Channel

WebSocket 字段：

- `enabled`：默认 `false`。
- `bind_addr`：监听地址。默认 `127.0.0.1:8765`。也可以写成别名 `bind`。
- `auth`：是否启用 bearer token 鉴权。默认 `true`。启用 websocket 时建议保持 `true`。

WebSocket channel 复用 ACP stdio 的 request/response 和 notification 行为：client 通过 websocket 发送一条 JSON-RPC text/binary message，服务端回一条 JSON-RPC text message。每个 websocket connection 是一条独立 ACP transport connection；`initialize`、`session/new`、`session/prompt`、`session/cancel`、`session/list`、`session/load`、`session/set_mode` 和 `session/set_config_option` 的语义与 stdio ACP 保持一致。

生成 WebSocket token：

```powershell
cargo run -- channel login websocket --agent-folder examples/dwo-agent
```

该命令要求 `websocket.enabled: true` 且 `websocket.auth: true`，然后写入：

```text
<agent-folder>/channel_secret/websocket/auth.yaml
```

客户端连接时在 websocket handshake 携带：

```http
Authorization: Bearer <token>
```

### Weixin Channel

Weixin 字段：

- `enabled`：默认 `false`。
- `workspace_dir`：Weixin 会话使用的 workspace cwd。相对路径按 agent structure 目录解析。默认 `.`。
- `markdown_filter`：是否让 Weixin SDK 应用 markdown filtering。默认 `true`。
- `media_input`：是否下载入站非文本媒体，并作为附件传给 agent。默认 `false`。
- `media_output`：是否向 Weixin channel session 的模型暴露 `weixin_reply_media(path)`。默认 `false`。
- `response_detail`：回复细节级别。`response_only` 只发送最终回复；`detailed` 会在最终回复前发送完整 thinking 和截断后的 tool call 参数摘要。默认 `response_only`。
- `override_model`：可选模型别名，只在 Weixin channel session 首次创建时使用。
- `override_reasoning_mode`：可选 reasoning mode，只在 Weixin channel session 首次创建时使用。

微信登录：

```powershell
cargo run -- channel login weixin --agent-folder examples/weixin-agent
```

登录会写入：

```text
<agent-folder>/channel_secret/weixin/auth.yaml
```

`auth.yaml` 包含签发的微信凭据，不应该提交到仓库。

Weixin 运行时文件：

```text
<agent-folder>/channel_secret/weixin/auth.yaml
<agent-folder>/channel_secret/weixin/context_tokens.json
<channel_session_dir>/weixin/session/
<channel_session_dir>/weixin/session/sync_buf.txt
```

Weixin channel 使用 `<channel_session_dir>/weixin/session/` 下的一个持久化 channel session。`channel_session_dir` 来自 `agent.yaml`，默认是 `<agent-folder>/channel_sessions`，可以改成任意绝对路径或相对路径。Channel secret 仍固定保存在当前 agent profile 的 `channel_secret/weixin/` 下。

一旦该 channel session 已存在，它会保留自己的 model、reasoning mode、runtime tool schemas 和工具快照。之后修改 `override_model`、`override_reasoning_mode` 或 `media_output`，不会自动改写已有 channel session。需要新的 channel 配置快照时，应删除该 channel session 或显式迁移它。

### Feishu Channel

Feishu 字段：

- `enabled`：默认 `false`。
- `workspace_dir`：Feishu 会话使用的 workspace cwd。相对路径按 agent structure 目录解析。默认 `.`。
- `domain`：`feishu` 或 `lark`。默认 `feishu`，使用 `https://open.feishu.cn`；`lark` 使用国际版 Lark。
- `dm_policy`：私聊策略。`allow_all` 接受所有私聊；`white_list` 只接受 `allow_from` 中的 sender open id。默认 `white_list`。
- `group_policy`：群聊策略。`allow_all` 接受所有群；`white_list` 只接受 `group_allow_from` 中的群 `chat_id`。默认 `white_list`。
- `allow_from`：私聊 sender open id 白名单。`"*"` 表示全部允许。
- `group_allow_from`：群聊 `chat_id` 白名单。`"*"` 表示全部允许。
- `group_require_mention`：群聊是否必须 @机器人 才触发。默认 `true`。
- `media_input`：是否下载入站图片和文件，并作为附件传给 agent。默认 `false`。
- `media_output`：是否向 Feishu channel session 的模型暴露 `feishu_reply_media(path)`，用于上传并回复图片或文件。默认 `false`。
- `card_output`：是否向 Feishu channel session 的模型暴露 `feishu_reply_card(card)`，用于发送飞书交互卡片。默认 `false`。
- `response_detail`：回复细节级别。`response_only` 只发送最终回复；`detailed` 会在最终回复前发送完整 thinking 和截断后的 tool call 参数摘要。默认 `response_only`。
- `override_model`：可选模型别名，只在对应 Feishu channel session 首次创建时使用。
- `override_reasoning_mode`：可选 reasoning mode，只在对应 Feishu channel session 首次创建时使用。

飞书凭据：

```powershell
cargo run -- channel login feishu --agent-folder examples/weixin-agent --app-id cli_xxx --app-secret xxx
```

也可以从环境变量读取：

```powershell
$env:FEISHU_APP_ID="cli_xxx"
$env:FEISHU_APP_SECRET="xxx"
cargo run -- channel login feishu --agent-folder examples/weixin-agent
```

登录会写入：

```text
<agent-folder>/channel_secret/feishu/auth.yaml
```

`auth.yaml` 只保存 `app_id` 和 `app_secret`，不应该提交到仓库。

Feishu 运行时文件：

```text
<agent-folder>/channel_secret/feishu/auth.yaml
<channel_session_dir>/feishu/dm/<sender>/
<channel_session_dir>/feishu/group/<chat_id>/
<channel_session_dir>/feishu/dm/<sender>/attachments/<message_id>/
<channel_session_dir>/feishu/group/<chat_id>/attachments/<message_id>/
```

Feishu 私聊和群聊使用独立持久化 channel session。收到私聊消息时使用 sender open id 定位：

```text
<channel_session_dir>/feishu/dm/<sender>/
```

收到群聊消息时使用群 `chat_id` 定位：

```text
<channel_session_dir>/feishu/group/<chat_id>/
```

如果对应目录里已有 session metadata 和 context，就加载已有 session；否则创建新 session。`channel_session_dir` 来自 `agent.yaml`，默认是 `<agent-folder>/channel_sessions`，可以改成任意绝对路径或相对路径。Channel secret 仍固定保存在当前 agent profile 的 `channel_secret/feishu/` 下。

开启 `media_input` 后，入站图片和文件会下载到对应 session 的 `attachments/<message_id>/` 下，并以 `resource_link` 加入上下文；图片会额外加入 data URL image block，供支持多模态的模型读取。

开启 `media_output` 后，新建 Feishu channel session 会获得 `feishu_reply_media` 工具。工具会把 workspace 或当前 channel session 目录内的本地文件上传到飞书，然后回复当前私聊或群聊。`kind: auto` 会把常见图片扩展名作为图片消息发送，其他文件作为文件消息发送；也可以指定 `kind: image` 或 `kind: file`。

开启 `card_output` 后，新建 Feishu channel session 会获得 `feishu_reply_card` 工具。工具接收飞书 interactive card JSON object，并发送到当前私聊或群聊。当前实现只发送新卡片，不做卡片更新、延迟更新或表单回调处理。

## Agent Prompt

主 agent prompt 是必需文件：

```text
<agent-folder>/resources/agents/<agent_id>.agent.md
```

它会作为 `<agent_prompt>` 插入 system context。文件必须存在且不能为空。

加载行为：

- 新会话创建初始 system message 时读取该文件。
- 已有会话通常复用持久化的 context messages。
- context compaction 会重建 stable system-message prefix，因此压缩时可以拾取该文件的变更。

## Rules

Rules 是可选的。运行时按以下顺序读取 rule 文件：

```text
<agent-folder>/resources/agents/<agent_id>.rule.md
agent.yaml external_rule_files entries
<cwd>/.agent/AGENTS.md
<cwd>/AGENTS.md
<cwd>/CLAUDE.md
```

`cwd` 是会话 workspace：

- ACP 会话使用 `session/new.cwd`。
- 解析后的 `cwd` 会随 session 持久化，并在加载 session 时复用。

每个非空 rule 文件会被包装成 `<rule>` 内的独立 source block，包含解析后的 source path 和转义后的内容。缺失或空文件会被忽略。

Rule 优先级是追加式的，不是覆盖式的：所有发现到的 rule 文件都会按顺序加入。agent-wide 行为放在 agent folder rule；共享 profile 行为放在 `external_rule_files`；workspace-specific 指引放在 `.agent/AGENTS.md` 或根目录 `AGENTS.md`。

加载行为与 system context 生命周期一致：

- 新会话创建初始 system message 时读取 rules。
- 已有会话通常保留持久化的 rules。
- context compaction 会重建 system messages，因此可以拾取 rule 文件变更。

## Skills

Skills 是可选的。运行时从以下位置发现 skills：

```text
<agent-folder>/resources/skills
agent.yaml external_skills_dirs entries
<cwd>/.agent/skills
```

这些位置既支持单个 skill root：

```text
resources/skills/
  SKILL.md
```

也支持多个子 skill 目录：

```text
resources/skills/
  code-review/
    SKILL.md
  release-notes/
    SKILL.md
```

`<cwd>/.agent/skills` 下也支持同样的结构。

每个 `SKILL.md` 必须以 YAML frontmatter 开头，并包含：

```yaml
---
name: code-review
description: Review code changes and call out concrete defects.
---
```

必填字段：

- `name`：kebab-case，例如 `code-review`。
- `description`：非空文本。生成 prompt 时会 trim，并截断到 500 个字符。

当前会解析的可选 frontmatter 字段：

- `license`
- `compatibility`
- `allowed-tools`
- `metadata`

生成的 `<available_skills>` block 只包含每个 skill 的 name、description 和 `SKILL.md` location。模型可以据此决定是否打开对应 skill 文件。

发现顺序：

1. Agent profile skills：`<agent-folder>/resources/skills`
2. 配置的外部 skill roots：`agent.yaml external_skills_dirs`
3. Workspace skills：`<cwd>/.agent/skills`

发现到的 skill 目录会 canonicalize，并按路径去重。如果同一个物理 skill 能从多个 root 访问，它只会出现一次。

加载行为：

- 新会话创建初始 system message 时发现 skills。
- 已有会话通常保留持久化的 skill 列表。
- context compaction 会重建 system messages，因此可以拾取新增、删除或修改过的 skills。

## Env Block Watcher

每个主会话都会启动一个 env block watcher。它观察 rules、skills 和 environment context 使用的同一组动态上下文来源：

```text
<agent-folder>/resources/agents/<agent_id>.rule.md
agent.yaml external_rule_files entries
<cwd>/.agent/AGENTS.md
<cwd>/AGENTS.md
<cwd>/CLAUDE.md
<agent-folder>/resources/skills
agent.yaml external_skills_dirs entries
<cwd>/.agent/skills
```

第一版实现会轮询这些来源，并重建当前 env block snapshot。当 snapshot 变化时，它会为当前 session 排队 watcher content。它不会主动调用模型，也不会改写 stable system prefix。

排队的 watcher content 只会在 agent-loop 边界注入：

```text
stable system prefix
watcher content
user input
assistant thinking / response
tool call
tool result
watcher content
assistant thinking / response
```

注入的 watcher content 是一条包含当前 env block snapshot 的 system message：

```xml
<watcher_content>
<env_block>
  <rule>...</rule>
  <available_skills>...</available_skills>
  <env_context>...</env_context>
</env_block>
</watcher_content>
```

压缩时，只有初始 system context 会作为 stable prefix 保留。Watcher content 会被当作普通 conversation content，因此旧的 env block snapshots 可以和周围 turn history 一起被总结、折叠。这样对 prefix cache 更友好：skill 或 rule 变化只会追加一个小的动态更新，而不是替换原始 system message。

## Generated System Context

system context 由这些 blocks 构成：

```text
<agent_context>
  <agent_prompt>...</agent_prompt>
  <rule>...</rule>
  <tools>...</tools>
  <available_skills>...</available_skills>
  <env_context>...</env_context>
</agent_context>
```

`<tools>` 来自当前 session 快照下来的 `tools` 配置。`<env_context>` 包含 workspace path、terminal shell 等运行环境信息。

## 使用建议

属于 agent profile 本身的行为，放在 agent-folder resources 里。属于项目或 workspace 的特定行为，放在 workspace `.agent/` 文件里，让它跟随仓库或 workspace 走。

多个 agent profile 复用的共享资源，例如组织级 skills 或通用 rules，可以放在 `external_skills_dirs` 和 `external_rule_files` 指向的位置。

推荐结构：

```text
my-agent/
  agent.yaml
  model.yaml
  channels.yaml
  shared/
    rules/
      common.md
    skills/
      shared-planning/
        SKILL.md
  resources/
    agents/
      my-agent.agent.md
      my-agent.rule.md
    skills/
      planning/
        SKILL.md

my-workspace/
  AGENTS.md
  .agent/
    AGENTS.md
    skills/
      repo-debugging/
        SKILL.md
```

修改 prompt、rule 或 skill 文件后，新建会话可以保证立即看到变化。已有会话可能通过 env block watcher 在下一次 agent loop 前看到动态更新；context compaction 后，新的完整 system context 也会被重建。
