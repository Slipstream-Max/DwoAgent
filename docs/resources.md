# Prompt、Skill 与 MCP

Resource 是 Agent 的可编辑能力层，位于 ~/.dwoagent/resource/。Profile 只负责声明外部资源路径；
本文件说明 Prompt、Rule、Skill 和 MCP 的文件格式、加载顺序与管理方式。

Model List 也在 resource/models/，但它有独立 schema，见 [模型与 Provider](models.md)。

## 目录结构

~~~text
resource/
|- prompts/
|  |- System.md
|  `- AGENTS.md
|- skills/
|  `- <skill>/
|     |- SKILL.md
|     |- references/
|     |- scripts/
|     `- assets/
|- skills.disabled/
|  `- <skill>/
|- models/
|  `- <family>.yaml
`- mcp/
   |- mcp.json
   `- oauth/
~~~

System.md 必须存在且非空。其他目录按需创建。oauth/ 和 skills.disabled/ 由 daemon 管理时生成。

## Prompt 和 Rule

### System.md

resource/prompts/System.md 是主 System Prompt。每次新建 Session、上下文重建和资源热加载都以它
为基础。文件缺失、为空或不是 UTF-8 时，Profile 无法正常加载。

System.md 应放稳定、全局的 Agent 身份和基本行为，不要放某个项目的临时要求。

### AGENTS.md 和其他 Rule

Rule 来源按下面顺序加入 System Prompt：

| 来源 | 适用目录 |
| --- | --- |
| resource/prompts/AGENTS.md | Profile 根目录 |
| <session-cwd>/AGENTS.md | Session 当前工作目录 |
| <session-cwd>/.agents/AGENTS.md | Session 当前工作目录 |
| externalRuleFiles | Profile 根目录；相对路径相对 Profile 根目录 |
| Topic 的 AGENTS.md | 关联 Session 当前工作目录 |

空的 Rule 文件会忽略。每份 Rule 都带 source 和 pwd，模型可以判断它来自哪里、约束哪个目录。
Topic Knowledge 的存储和移动行为见 [Project 文件与行为](projects.md)。

Profile 中配置外部 Rule：

~~~yaml
externalRuleFiles:
  - C:\shared\rules\security.md
  - resource/prompts/team.md
~~~

只有 System.md 和上述 Rule 来源会自动进入 Agent 上下文。resource/prompts/ 中其他 Markdown
可以通过 API 管理，但不会因为放进目录就自动成为 System Prompt。

Management RPC 使用 prompt.list/get/set 管理 Prompt，使用 rule.list/get/set 管理 Rule；默认
文件分别是 System.md 和 AGENTS.md。方法 envelope 见 [API 说明](api.md)。

## Skill

Skill 是一个以 SKILL.md 为入口的目录：

~~~text
resource/skills/deploy/
|- SKILL.md
|- references/
|  `- checklist.md
|- scripts/
|  `- verify.ps1
`- assets/
   `- template.yaml
~~~

SKILL.md 可以使用 YAML frontmatter：

~~~markdown
---
name: deploy
description: Deploy and verify the current project.
---

# Deploy

Read references/checklist.md, run scripts/verify.ps1, then deploy.
~~~

| 字段 | 必填 | 默认值 | 作用 |
| --- | --- | --- | --- |
| name | 否 | Skill 目录名 | Catalog 中的名称 |
| description | 否 | 空字符串 | Catalog 摘要，帮助模型选择 Skill |

Skill 文件必须是 UTF-8。目录根部必须有 SKILL.md；references、scripts、assets 和其他文件会
随目录一起安装，但 daemon 不会自动执行它们。模型先从 Catalog 看到 name、description 和
SKILL.md 路径，需要使用时再读取说明。

### Skill 来源和优先级

| 来源 | 路径 |
| --- | --- |
| Profile | resource/skills/<name>/SKILL.md |
| External | externalSkillsDirs 中每个目录的 <name>/SKILL.md |
| Project | <session-cwd>/.agents/skills/<name>/SKILL.md |

同名优先级为 Profile < External < Project。多个 External 目录按 Profile 中的顺序扫描，后面的
同名 Skill 覆盖前面的。

~~~yaml
externalSkillsDirs:
  - C:\shared\dwo-skills
  - D:\team\skills
~~~

CLI 管理：

~~~text
dwo skills list
dwo skills add <file-or-directory> [--name <name>]
dwo skills remove <name>
~~~

单个 Markdown 文件安装成 SKILL.md；目录安装保留全部子文件。Management RPC 还提供
skill.enable、skill.disable 和 skill.uninstall；禁用的目录移到 resource/skills.disabled/。

## MCP

MCP 配置文件是 resource/mcp/mcp.json。daemon 统一托管连接和 OAuth，不为每个 ACP Client
单独启动 Server。

### 完整示例

~~~json
{
  "mcpServers": {
    "local-tools": {
      "type": "stdio",
      "enabled": true,
      "command": "node",
      "args": ["server.js"],
      "cwd": "servers/local-tools",
      "env": {
        "TOKEN": "${LOCAL_TOOLS_TOKEN}",
        "PATH": "${PATH}"
      },
      "description": "Local project tools"
    },
    "github": {
      "type": "streamableHttp",
      "enabled": true,
      "url": "https://example.test/mcp",
      "headers": {
        "Authorization": "Bearer ${GITHUB_TOKEN}"
      },
      "auth": {
        "type": "oauth",
        "scopes": ["repo"]
      },
      "description": "GitHub tools"
    }
  }
}
~~~

顶层必须包含 mcpServers Object。Server 的 enabled 为 false 时不加载。

### stdio 字段

| 字段 | 必填 | 默认值 | 作用 |
| --- | --- | --- | --- |
| type | 否 | stdio | 只能是 stdio |
| enabled | 否 | true | 是否加载 |
| command | 是 | 无 | 可执行程序 |
| args | 否 | [] | 字符串参数数组 |
| env | 否 | {} | 传给子进程的环境变量 |
| cwd | 否 | daemon 环境 | 工作目录；相对路径相对 resource/mcp/ |
| description | 否 | 无 | Server Catalog 描述 |

子进程继承 daemon 环境，env 覆盖同名项。command、args、env 和 cwd 支持 ${NAME} 展开；
Windows 还支持 %NAME%。PATH 可以用 ${PATH} 继承并扩展现有值。

### Streamable HTTP 字段

| 字段 | 必填 | 默认值 | 作用 |
| --- | --- | --- | --- |
| type | 是 | 无 | streamableHttp、streamable-http 或 http |
| enabled | 否 | true | 是否加载 |
| url | 是 | 无 | MCP Endpoint |
| headers | 否 | {} | HTTP Header，支持 ${NAME} |
| auth | 否 | 无 | OAuth 配置 |
| description | 否 | 无 | Server Catalog 描述 |

OAuth：

~~~json
{
  "auth": {
    "type": "oauth",
    "scopes": ["repo", "read:user"]
  }
}
~~~

type 目前只支持 oauth；scopes 默认空数组。授权数据保存在 resource/mcp/oauth/，不会通过配置
查询回显。

### MCP 生命周期

daemon 启动或配置变化时初始化全部 Server，状态为 starting、ready、auth_required 或 failed。
成功的 stdio/HTTP 连接会持续复用；Catalog 只在 daemon 内存中保存，重启后从 mcp.json 重建。

CLI 管理：

~~~text
dwo mcp list
dwo mcp get <name>
dwo mcp add ...
dwo mcp add-json <name> <json>
dwo mcp remove <name>
dwo mcp search <query>
dwo mcp call <server.tool> --args '<json>'
dwo mcp auth <server> [--logout]
~~~

完整参数见 [CLI 命令参考](cli.md)。对话中要求 Agent 使用指定资源时，使用
[Slash Commands](slash-commands.md) 中的 /skill 和 /mcp。

## 热加载和排查

daemon 监听 Prompt、Rule、Skill 和 mcp.json。变化会在下一个安全边界更新 Session 环境；
MCP 配置变化会重新初始化相关连接。

排查顺序：

~~~text
dwo config-show
dwo skills list
dwo mcp list
dwo mcp get <name>
~~~

然后查看 ~/.dwoagent/logs/。不要把 API Key、Token、OAuth 数据或带凭据的 Header 提交到仓库。
