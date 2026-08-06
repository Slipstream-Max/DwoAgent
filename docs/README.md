# 赤铎文档

根目录 [README](../README.md) 用于快速认识和运行赤铎；这里保存部署、配置和行为细节。

## 从这里开始

1. 按 README 的[“五分钟跑起来”](../README.md#五分钟跑起来)编译、安装并启动 daemon。
1. 在 [Profile 配置指南](profile.md) 中设置模型、权限和需要启用的 channel。
1. 选择一种对话入口：IDE/编辑器使用 [ACP](acp.md)，聊天应用使用 [Channels](channels.md)，脚本或终端使用 [CLI](commands.md)。

## 文档地图

| 文档 | 适合什么时候读 |
| --- | --- |
| [CLI 命令参考](commands.md) | 查询命令、参数和 session/MCP/automation 行为 |
| [ACP 使用指南](acp.md) | 将支持 ACP 的 IDE 或客户端连接到已有 daemon |
| [Channel 部署与使用](channels.md) | 部署微信、Telegram、飞书/Lark，查询 slash commands |
| [Subsessions 使用指南](subsessions.md) | 了解父子 session、配置继承、结果回传和任务控制 |
| [上下文压缩与 Handoff](context.md) | 了解长会话压缩、`/compact` 和 Agent 主动重建上下文 |
| [Automation 使用指南](automation.md) | 配置 cron、时区、新建/固定 session 和无人值守任务 |
| [Profile 配置指南](profile.md) | 修改模型、权限、资源、MCP、channel 和持久化目录 |

## 推荐阅读顺序

- 首次部署：README -> Profile 配置 -> ACP 或 Channels。
- 日常操作：CLI 命令参考 + 对应入口指南；需要拆分任务时阅读 Subsessions 使用指南。
- 排查状态：先运行 `dwo daemon status`、`dwo profile-list` 和 `dwo channel <name> status`，再检查 `~/.dwoagent/runtime/logs/`。
