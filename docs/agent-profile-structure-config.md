# Agent Profile 结构和配置

当前 rewrite 只有一个 profile host 和一个本地 daemon。profile 根目录默认是 `~/.dwoagent`，配置入口固定为 `profile.yaml`，资源入口固定为 `resource/`。

```text
<profile-root>/
|- profile.yaml
|- resource/
|  |- prompts/System.md       必需
|  |- prompts/AGENTS.md       可选
|  |- skills/<skill>/SKILL.md 可选
|  `- mcp.json                可选
|- runtime/
|  |- sessions/YYYY/MM/DD/<session-id>/
|  |  |- session.json
|  |  |- model_context.json
|  |  `- client_transcript.jsonl
|  |- workspaces/
|  |- attachments/
|  |- channel-capabilities/<channel>.md
|  |- mcp/catalog.json
|  |- mcp/oauth/
|  `- logs/
`- channels/
   |- weixin/
   |  |- runtime.yaml
   |  `- secret.yaml
   `- telegram/
      |- runtime.yaml
      `- secret.yaml
```

`session.json` 保存 session 自身配置，`model_context.json` 保存当前模型上下文，`client_transcript.jsonl` 是完整、追加式的客户端事件流。压缩只重建 model context，不删除 transcript。

## profile.yaml

```yaml
name: coder
description: coding agent
policyMode: confirm
channels:
  weixin:
    enabled: false
  telegram:
    enabled: false
    replayTurns: 5
    botTokenEnv: TELEGRAM_BOT_TOKEN
    tgProxy: null
    mediaInput: true
automation:
  enabled: false
  jobs: []
model:
  defaultModelId: deepseek-v4-pro
  providers:
    deepseek:
      type: deepseek
      apiKeyEnv: DEEPSEEK_API_KEY
  models:
    - modelName: deepseek-v4-pro
      provider: deepseek
      modelId: deepseek-v4-pro
```

支持的顶层字段只有：

- `name`、`description`：profile 标识和描述，不能为空。
- `policyMode`：`full_access`、`confirm` 或 `watch`，决定 session 默认权限模式。
- `channels`：支持 daemon 托管的 Weixin 和 Telegram 私聊配置；不配置则不启动对应 channel。Telegram token 只从 `botTokenEnv` 指向的环境变量读取，`tgProxy` 是仅用于 Telegram 的可选 HTTP 代理，`mediaInput` 控制 photo、document、video 输入。
- `automation`：daemon 托管的定时任务配置。
- `model`：provider 实例、模型别名和 profile 级限制覆盖。

旧的 `agent.yaml`、`tools` 开关、supervisor profile registry 和额外 transport 配置不属于当前 schema，会被拒绝。

Telegram 通过 `dwo channel telegram bind` 创建一次性验证码，并在 bot 私聊中用 `/bind <code>` 绑定唯一用户。`channels/telegram/secret.yaml` 保存 bot ID/username 和绑定 user/chat，不保存 token；`runtime.yaml` 只保存当前选中的 session。Telegram 和 Weixin 可以选择同一个全局 session。

已启用且绑定的 channel adapter 各自维护 system prompt 文案，并把无 secret 的派生投影写入 `runtime/channel-capabilities/<channel>.md`。context builder 只通用扫描这些投影，不包含任何微信或 Telegram 专用判断；绑定和解绑会通过 environment watcher 更新已有 session。

## model

```yaml
model:
  defaultModelId: deepseek-v4-pro
  providers:
    deepseek:
      type: deepseek
      baseUrl: null
      apiKeyEnv: DEEPSEEK_API_KEY
  models:
    - modelName: deepseek-v4-pro
      provider: deepseek
      modelId: deepseek-v4-pro
      contextWindowTokens: 1000000
      maxOutputTokens: 384000
      compactThreshold: 0.5
      defaultReasoningMode: high
```

模型 alias 必须匹配 `modelName`。`provider` 和 `modelId` 在内置 catalog 中解析；`baseUrl`、`apiKeyEnv`、`apiKey` 可以作为 profile 级 provider 设置；context/output 限制、`compactThreshold` 和默认 reasoning mode 可以由 profile 覆盖。headers、retry、request body 和 capabilities 仍由内置 model catalog 管理。

context token 由 daemon 根据完整的 system prompt、消息、reasoning、图片、tool call/result 和 tool schema 直接估算，不使用 provider 的 input/output 累加。触发阈值为 `(contextWindowTokens - maxOutputTokens) * compactThreshold`。压缩完成后立即重新估算并发送 `usage_update`；模型切换也会按目标模型的 context window 发送新的 usage update。

### 图片模型切换

从支持图片的模型切换到纯文本模型时，daemon 会先使用当前或最后成功的视觉模型，把所有含图历史压缩成文字，再提交目标模型。摘要失败时 model context 和 session 配置都保持原值。图片 turn 正在运行时不能降级切换；纯文本模型也会在写入 transcript/model context 前拒绝新的图片 prompt。

迁移完成后 model context 不再包含旧图片，但 `client_transcript.jsonl` 保留原始图片供 replay。切回视觉模型不会自动把已经压缩掉的图片恢复到 model context，用户需要重新附图。

## resource

`resource/prompts/System.md` 是固定的必需系统提示词。`resource/prompts/AGENTS.md` 是可选规则文件；`resource/skills/<name>/SKILL.md` 是可选 skill。daemon 只观察这些固定路径和当前工作目录的规则/skill 变化，并在 agent-loop 边界追加 watcher 消息。

## MCP

`resource/mcp.json` 是可选配置：

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "."]
    },
    "github": {
      "type": "streamableHttp",
      "url": "https://example.test/mcp",
      "headers": {"Authorization": "Bearer ${GITHUB_TOKEN}"}
    }
  }
}
```

daemon 启动时并发初始化所有 server，并持续托管成功的 stdio/HTTP 连接。配置 watcher 对新增或变更 server 使用同样的初始化流程。catalog 状态为 `starting`、`ready`、`auth_required` 或 `failed`；`runtime/mcp/catalog.json` 是当前内存 catalog 的派生投影，不是活跃连接证明。MCP schema 不会注册成模型 native tool。

模型通过 terminal 使用以下命令：

```text
dwo mcp search <query>
dwo mcp call <server.tool> --args '<json>'
dwo mcp auth <server> [--logout]
```

`search` 只读取当前内存 catalog。server 命中时列出全部工具但只对直接命中的工具展开 schema；tool-only 命中时只列出匹配工具并展开 schema。输出是 YAML 风格文本，只有 `call --args` 使用 JSON payload。

完整 CLI 参考见 [commands.md](commands.md)。
