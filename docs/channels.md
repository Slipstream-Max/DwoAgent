# Channel 配置与行为

Channel 是消息平台适配器：它把微信、Telegram、飞书/Lark 或 QQ Bot 的私聊接到 daemon。
所有 channel 共享同一批 Session、模型、工具和权限；它们只是不同的消息入口。

远程 IDE 或管理面板走独立的 [WebSocket transport](websocket.md)，不属于 Channel。

## 配置字段

Channel 字段在 [Profile 配置](profile.md) 的 channels 段出现。最小示例：

~~~yaml
channels:
  weixin:
    enabled: true
    replayTurns: 5
    outputMode: final
    markdownFilter: true
    mediaInput: true
  telegram:
    enabled: false
    replayTurns: 5
    outputMode: final
    botTokenEnv: TELEGRAM_BOT_TOKEN
    tgProxy: null
    mediaInput: true
  feishu:
    enabled: false
    replayTurns: 5
    outputMode: final
    appIdEnv: FEISHU_APP_ID
    appSecretEnv: FEISHU_APP_SECRET
    platform: feishu
    mediaInput: true
  qq:
    enabled: false
    replayTurns: 5
    outputMode: final
    mediaInput: true
~~~

| 字段 | 作用 |
| --- | --- |
| enabled | 是否启动 adapter；改动后会热重启对应 channel |
| replayTurns | /use 切换 Session 时回放最近多少个完成 turn，范围 0..=10 |
| outputMode | final 只发最终回答；full 还发 reasoning、tool-call 和阶段性回答 |
| mediaInput | 是否接收图片和文件；关闭后只处理文本 |
| markdownFilter | 仅微信使用；把 Markdown 转成微信可读文本 |
| botTokenEnv | Telegram token 所在的环境变量名 |
| tgProxy | Telegram 使用的 HTTP/HTTPS 代理 |
| appIdEnv / appSecretEnv | 飞书/Lark 凭据所在的环境变量名 |
| platform | feishu（国内）或 lark（海外） |

凭据从环境变量读取，不要写进 profile 或提交到仓库。

启用后检查：

~~~text
dwo daemon stop
dwo daemon start
dwo channel list
dwo channel <weixin|telegram|feishu|qq> status
~~~

## 绑定平台

### 微信

1. 开启 channels.weixin.enabled，重启 daemon。
2. 运行 dwo channel weixin bind。
3. 用微信扫描终端二维码并按手机提示确认。
4. 用 dwo channel weixin status 确认绑定。

解绑使用 dwo channel weixin unbind。

### Telegram

Telegram 使用 Bot API long polling，不需要 webhook 或公网地址。

1. 在 BotFather 创建 bot，取得 token。
2. 将 token 放入 botTokenEnv 指向的环境变量。
3. 开启 channels.telegram.enabled；需要代理时设置 tgProxy。
4. 重启 daemon，运行 dwo channel telegram bind。
5. 在 bot 私聊中发送终端显示的一次性 /bind <code>。
6. 用 dwo channel telegram status 确认绑定。

绑定文件只保存 bot 标识和 user/chat，不保存 token。photo、document、video 会下载到
runtime/attachments/telegram/<date>/<session-id>/，再作为 resource link 交给模型。

### 飞书 / Lark

飞书和 Lark 使用开放平台 WebSocket 长连接，不需要 webhook。

1. 创建企业自建应用并启用机器人。
2. 选择长连接事件订阅，订阅 im.message.receive_v1。
3. 开通接收消息、以应用身份发送消息、获取消息资源和上传图片/文件所需权限。
4. 发布应用，让目标用户可以私聊机器人。
5. 设置 appIdEnv 和 appSecretEnv 指定的两个环境变量。
6. 使用 platform: feishu（国内）或 platform: lark（海外）。
7. 重启 daemon，运行 dwo channel feishu bind，在私聊中发送 /bind <code>。
8. 用 dwo channel feishu status 确认绑定。

绑定文件只保存 open_id 和 chat_id。入站 text、image、file 会作为 prompt 或 resource link
提交给模型。

### QQ Bot

QQ channel 目前只支持单用户 C2C 私聊，不支持群聊，也不需要在 profile 中填写凭据。

1. 开启 channels.qq.enabled 并重启 daemon。
2. 运行 dwo channel qq bind。
3. 扫描 QQ 官方二维码；返回结果必须包含目标用户的 userOpenid。
4. 用 dwo channel qq status 确认绑定。

二维码返回的 AppID、AppSecret 和用户 OpenID 保存在 channels/qq/secret.yaml。
QQ 出站文件首版限制为 20 MiB。

## 消息如何进入 Session

- 普通文本提交到当前 Session；没有当前 Session 时会自动创建一个。
- /new 创建并选择 Session，/use 切换，/fork 复制但不切换。
- 仅发送图片或文件也是有效 prompt。
- confirm 模式下直接发送 /allow 或 /deny 处理当前权限请求。
- /cancel 才会中断当前 turn；发送新消息只会排队，不会取消工具。
- 普通回答会由 adapter 自动发送；主动发送才使用 dwo channel ... send-message/send-file。

命令完整语法见 [Slash Commands](slash-commands.md)，CLI 发送和管理命令见 [CLI 命令参考](cli.md)。

## 排查

~~~text
dwo daemon status
dwo channel list
dwo channel <name> status
~~~

依次确认 enabled: true、daemon 能读取凭据环境变量、平台应用已发布并具备所需权限，再重新
执行 bind。connected 只表示绑定信息有效，不等于实时网络连接健康；详细错误看
~/.dwoagent/logs/。
