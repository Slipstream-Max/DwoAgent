# Prompt 与 Skill

Resource 是 Agent 的可编辑能力层，位于 ~/.dwoagent/resource/。Profile 只负责声明外部资源路径；
本文件说明 Prompt、Rule 和 Skill 的文件格式、加载顺序与管理方式。

Model List 也在 resource/models/，但它有独立 schema，见 [模型与 Provider](models.md)。

## 目录结构

~~~text
resource/
|- models/
|  `- <family>.yaml
|- prompts/
|  |- System.md
|  `- AGENTS.md
`- skills/
   |- <skill>/
   |  |- SKILL.md
   |  |- references/
   |  |- scripts/
   |  `- assets/
   `- .disabled/<skill>/
~~~

System.md 必须存在且非空。其他目录按需创建。禁用的 Skill 保存在 skills/.disabled/，不进入 Catalog。

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
| Project 根目录或 Session cwd 下的 AGENTS.md | 作用于该目录及其子目录 |

空的 Rule 文件会忽略。每份 Rule 都带 source 和 pwd，模型可以判断它来自哪里、约束哪个目录。
Project rules 的路径和读写行为见 [Project 文件与行为](projects.md)。

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
skill.enable、skill.disable 和 skill.uninstall；禁用的目录移到 resource/skills/.disabled/。

### 通过 mcp2skill 使用外部工具

需要使用 MCP 服务时，可以在项目外通过 mcp2skill 将服务器转换为普通 Skill，安装到
resource/skills/ 或 externalSkillsDirs。生成目录包含 SKILL.md 和 scripts/cli.py；Agent
读取 Skill 后，通过 terminal 运行它的 CLI。mcp2skill 及其运行依赖由用户另行安装。

例如，进入 mcp2skill 目录后执行：

~~~text
uv run --no-project python scripts/mcp2skill.py <config.json> --out <profile>/resource/skills
~~~

使用生成的 Skill 时，在该 Skill 目录按其说明执行，例如：

~~~text
uv run --with fastmcp python scripts/cli.py call-tool <tool> --arg value
~~~

DWO 不再内置 MCP 连接、Catalog、OAuth、`dwo mcp`、`/mcp` 或 `mcp.*` 管理 RPC；
连接和认证由外部工具负责。旧配置和凭据文件不会被自动删除，也不再加载。

## 热加载和排查

daemon 监听 Prompt、Rule 和 Skill。变化会在下一个安全边界更新 Session 环境。

排查顺序：

~~~text
dwo config-show
dwo skills list
~~~

然后查看 ~/.dwoagent/logs/。不要把 API Key、Token、OAuth 数据或带凭据的 Header 提交到仓库。
