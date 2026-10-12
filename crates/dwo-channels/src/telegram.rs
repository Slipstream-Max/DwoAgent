use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use dwo_agent_service::{
    ActiveToolCall, ContentBlock, MessageContent, PendingPermission, SessionId,
};
use teloxide::dispatching::Dispatcher;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{FileId, InlineKeyboardButton, InlineKeyboardMarkup, InputFile};
use tokio::sync::Mutex;

use super::{ChannelBindingProgress, ChannelHost, ChannelManager, ChannelPollParams};
use dwo_command::{command_descriptions, parse_command, routes_to_channel_command};

use super::ChannelKind;
use super::attachments::{
    attachment_directory, local_file_resource, media_mime_type, sanitize_filename,
    unique_attachment_path,
};
use super::bridge::{ChannelIngress, ConversationId, ConversationTransport};
use super::gateway::{
    ChannelAdapter, ChannelBinder, ChannelRuntime, ChannelStarter, PreparedChannel,
};
use super::manager::{TelegramChannelState, telegram_bot};
use super::markdown::{FenceState, balance_fences};
use super::permission::{
    PermissionAction, PermissionActionStore, new_action_id, remove_group, take_action,
};
use super::render::{render_tool_call, split_message};

pub(super) const CAPABILITY_PROMPT: &str = r#"A Telegram channel is bound. Your normal reasoning and responses are already streamed to the user through Telegram.

Do not use the proactive messaging commands for normal replies.
Only use `dwo channel telegram send-message <message>` when the user explicitly asks you to proactively send a specific message.
Only use `dwo channel telegram send-file <path>` when the user explicitly asks you to send a file.

Use `dwo channel telegram --help` to inspect the available commands."#;

type TelegramHandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

pub struct TelegramAdapter;

#[async_trait]
impl ChannelBinder for TelegramAdapter {
    async fn begin_bind(
        &self,
        _host: Arc<dyn ChannelHost>,
        channels: Arc<ChannelManager>,
    ) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(
            channels.begin_telegram_bind().await?,
        )?)
    }

    async fn poll_bind(
        &self,
        _host: Arc<dyn ChannelHost>,
        channels: Arc<ChannelManager>,
        params: ChannelPollParams,
    ) -> Result<ChannelBindingProgress> {
        Ok(channels
            .poll_telegram_bind(&params.binding_id)
            .await?
            .into())
    }
}

struct TelegramStarter {
    bot: Bot,
    host: Arc<dyn ChannelHost>,
    access: TelegramAccess,
    approvals: Arc<PermissionActionStore>,
    rich: Arc<RichMessageSender>,
}

pub struct RunningTelegram {
    bot: Bot,
    chat_id: ChatId,
    task: tokio::task::JoinHandle<()>,
    shutdown: teloxide::dispatching::ShutdownToken,
    rich: Arc<RichMessageSender>,
}

#[async_trait]
impl ChannelAdapter for TelegramAdapter {
    async fn prepare(
        &self,
        host: Arc<dyn ChannelHost>,
        channels: Arc<ChannelManager>,
    ) -> Result<PreparedChannel> {
        let runtime = channels.load_telegram().await?;
        let bot = telegram_bot(&runtime.bot_token, runtime.config.tg_proxy.as_deref())?;
        let me = bot.get_me().await?;
        ensure!(
            me.id.0 == runtime.secret.bot_id,
            "Telegram token belongs to bot {}, but the channel is bound to bot {}",
            me.id.0,
            runtime.secret.bot_id
        );
        bot.set_my_commands(
            command_descriptions()
                .into_iter()
                .map(|(command, description)| {
                    teloxide::types::BotCommand::new(command, description)
                }),
        )
        .await?;

        let state = Arc::new(Mutex::new(runtime.state));
        let selected_session_id = state.lock().await.selected_session_id.clone();
        let chat_id = ChatId(runtime.secret.bound_chat_id);
        let approvals = Arc::new(Mutex::new(HashMap::new()));
        let rich = Arc::new(RichMessageSender::new(
            &runtime.bot_token,
            runtime.config.tg_proxy.as_deref(),
        )?);
        let conversation = Arc::new(TelegramConversation {
            channels: channels.clone(),
            chat_id,
            state,
            approvals: approvals.clone(),
            rich: rich.clone(),
        });
        let access = TelegramAccess {
            user_id: runtime.secret.bound_user_id,
            chat_id: runtime.secret.bound_chat_id,
            media_input: true,
        };
        Ok(PreparedChannel {
            conversation: ConversationId::new("telegram", runtime.secret.bound_user_id.to_string()),
            replay_turns: runtime.config.replay_turns,
            selected_session_id,
            transport: conversation,
            starter: Box::new(TelegramStarter {
                bot,
                host,
                access,
                approvals,
                rich,
            }),
        })
    }
}

#[async_trait]
impl ChannelStarter for TelegramStarter {
    async fn start(
        self: Box<Self>,
        ingress: Arc<dyn ChannelIngress>,
    ) -> Result<Box<dyn ChannelRuntime>> {
        let rich = self.rich.clone();
        let handler = dptree::entry()
            .branch(Update::filter_message().endpoint(handle_message))
            .branch(Update::filter_callback_query().endpoint(handle_callback_query));
        let mut dispatcher = Dispatcher::builder(self.bot.clone(), handler)
            .dependencies(dptree::deps![
                self.host,
                ingress,
                self.access,
                self.approvals,
                rich
            ])
            .build();
        let shutdown = dispatcher.shutdown_token();
        let task = tokio::spawn(async move {
            dispatcher.dispatch().await;
        });
        Ok(Box::new(RunningTelegram {
            bot: self.bot,
            chat_id: ChatId(self.access.chat_id),
            task,
            shutdown,
            rich: self.rich,
        }))
    }
}

#[async_trait]
impl ChannelRuntime for RunningTelegram {
    async fn stop(self: Box<Self>) {
        if let Ok(stopped) = self.shutdown.shutdown() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), stopped).await;
        }
        let mut task = self.task;
        if tokio::time::timeout(std::time::Duration::from_secs(1), &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
    }

    async fn send_message(&self, text: &str) -> Result<()> {
        send_chunks(&self.rich, self.chat_id, text, None).await
    }

    async fn send_file(&self, path: &Path) -> Result<()> {
        if !path.is_file() {
            bail!("file does not exist: {}", path.display());
        }
        self.bot
            .send_document(self.chat_id, InputFile::file(path.to_path_buf()))
            .await?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct TelegramAccess {
    user_id: u64,
    chat_id: i64,
    media_input: bool,
}

async fn handle_message(
    bot: Bot,
    message: Message,
    host: Arc<dyn ChannelHost>,
    ingress: Arc<dyn ChannelIngress>,
    access: TelegramAccess,
    rich: Arc<RichMessageSender>,
) -> TelegramHandlerResult {
    let Some(user) = message.from.as_ref() else {
        return Ok(());
    };
    if !message.chat.is_private()
        || user.id.0 != access.user_id
        || message.chat.id.0 != access.chat_id
    {
        return Ok(());
    }

    let text = message
        .text()
        .or_else(|| message.caption())
        .map(str::trim)
        .unwrap_or("");
    let media = incoming_media(&message);
    if text.is_empty() && media.is_none() {
        return Ok(());
    }
    let result = process_bound_message(
        &bot,
        host.as_ref(),
        ingress.as_ref(),
        text,
        media,
        access.media_input,
    )
    .await;
    match result {
        Ok(messages) => {
            for text in messages {
                send_chunks(&rich, message.chat.id, &text, None).await?;
            }
        }
        Err(error) => {
            send_chunks(
                &rich,
                message.chat.id,
                &format!("dwoagent error: {error:#}"),
                None,
            )
            .await?;
        }
    }
    Ok(())
}

async fn handle_callback_query(
    bot: Bot,
    query: CallbackQuery,
    ingress: Arc<dyn ChannelIngress>,
    access: TelegramAccess,
    approvals: Arc<PermissionActionStore>,
) -> TelegramHandlerResult {
    if query.from.id.0 != access.user_id {
        return Ok(());
    }
    let Some(message) = query.regular_message() else {
        return Ok(());
    };
    if message.chat.id.0 != access.chat_id {
        return Ok(());
    }
    let Some(action_id) = query.data.as_deref() else {
        return Ok(());
    };
    bot.answer_callback_query(query.id).await?;
    let action = take_action(&approvals, action_id).await;
    let Some(action) = action else {
        return Ok(());
    };
    if let Err(error) = ingress
        .resolve_permission(&action.session_id, &action.request_id, action.allowed)
        .await
    {
        tracing::debug!(
            event = "channel.telegram_permission_already_resolved",
            error = %format!("{error:#}"),
            "Telegram permission action lost the cross-channel race"
        );
    }
    Ok(())
}

async fn process_bound_message(
    bot: &Bot,
    host: &dyn ChannelHost,
    ingress: &dyn ChannelIngress,
    text: &str,
    media: Option<IncomingMedia>,
    media_input: bool,
) -> Result<Vec<String>> {
    if routes_to_channel_command(text) {
        return ingress.execute(parse_command(text)?).await;
    }
    let session_id = ingress.ensure_prompt_session().await?;
    let content = prompt_content(bot, host, text, media, media_input, &session_id).await?;
    ingress.submit_prompt(content).await?;
    Ok(Vec::new())
}

#[derive(Clone)]
struct IncomingMedia {
    file_id: FileId,
    filename: String,
    mime_type: String,
}

fn incoming_media(message: &Message) -> Option<IncomingMedia> {
    if let Some(photo) = message.photo().and_then(|photos| {
        photos.iter().max_by_key(|photo| {
            (
                u64::from(photo.width) * u64::from(photo.height),
                photo.file.size,
            )
        })
    }) {
        return Some(IncomingMedia {
            file_id: photo.file.id.clone(),
            filename: format!("photo-{}.jpg", message.id.0),
            mime_type: "image/jpeg".to_string(),
        });
    }
    if let Some(document) = message.document() {
        return Some(IncomingMedia {
            file_id: document.file.id.clone(),
            filename: document
                .file_name
                .clone()
                .unwrap_or_else(|| format!("document-{}.bin", message.id.0)),
            mime_type: document
                .mime_type
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "application/octet-stream".to_string()),
        });
    }
    message.video().map(|video| IncomingMedia {
        file_id: video.file.id.clone(),
        filename: video
            .file_name
            .clone()
            .unwrap_or_else(|| format!("video-{}.mp4", message.id.0)),
        mime_type: video
            .mime_type
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "video/mp4".to_string()),
    })
}

async fn prompt_content(
    bot: &Bot,
    host: &dyn ChannelHost,
    text: &str,
    media: Option<IncomingMedia>,
    media_input: bool,
    session_id: &SessionId,
) -> Result<MessageContent> {
    let mut blocks = Vec::new();
    if !text.is_empty() {
        blocks.push(ContentBlock::text(text));
    }
    if let Some(media) = media {
        if !media_input {
            bail!("Telegram media input is disabled");
        }
        let path = download_media(bot, host, &media, session_id).await?;
        let mime_type = media_mime_type(&path, &media.mime_type);
        blocks.push(local_file_resource(&path, &mime_type)?);
    }
    Ok(MessageContent::blocks(blocks))
}

async fn download_media(
    bot: &Bot,
    host: &dyn ChannelHost,
    media: &IncomingMedia,
    session_id: &SessionId,
) -> Result<PathBuf> {
    let directory = attachment_directory(host.profile_root_path(), "telegram", session_id);
    tokio::fs::create_dir_all(&directory).await?;
    let filename = sanitize_filename(&media.filename);
    let filename = if filename.is_empty() {
        "attachment.bin".to_string()
    } else {
        filename
    };
    let destination = unique_attachment_path(&directory, &filename).await?;
    let file = bot.get_file(media.file_id.clone()).await?;
    let mut output = tokio::fs::File::create(&destination)
        .await
        .with_context(|| format!("create Telegram attachment {}", destination.display()))?;
    if let Err(error) = bot.download_file(&file.path, &mut output).await {
        drop(output);
        let _ = tokio::fs::remove_file(&destination).await;
        return Err(error.into());
    }
    Ok(destination)
}

struct TelegramConversation {
    channels: Arc<ChannelManager>,
    chat_id: ChatId,
    state: Arc<Mutex<TelegramChannelState>>,
    approvals: Arc<PermissionActionStore>,
    rich: Arc<RichMessageSender>,
}

#[async_trait]
impl ConversationTransport for TelegramConversation {
    async fn send_text(&self, text: &str) -> Result<()> {
        send_chunks(&self.rich, self.chat_id, text, None).await
    }

    async fn send_permission_request(
        &self,
        session_id: &SessionId,
        call: &ActiveToolCall,
        permission: &PendingPermission,
    ) -> Result<()> {
        let allow = new_action_id()?;
        let deny = new_action_id()?;
        let group_id = new_action_id()?;
        let expires_at = Instant::now() + Duration::from_secs(10 * 60);
        {
            let mut approvals = self.approvals.lock().await;
            approvals.retain(|_, action| Instant::now() < action.expires_at);
            approvals.insert(
                allow.clone(),
                PermissionAction {
                    group_id: group_id.clone(),
                    session_id: session_id.clone(),
                    request_id: permission.request_id.clone(),
                    allowed: true,
                    expires_at,
                },
            );
            approvals.insert(
                deny.clone(),
                PermissionAction {
                    group_id: group_id.clone(),
                    session_id: session_id.clone(),
                    request_id: permission.request_id.clone(),
                    allowed: false,
                    expires_at,
                },
            );
        }
        let keyboard = InlineKeyboardMarkup::new([[
            InlineKeyboardButton::callback("允许", allow),
            InlineKeyboardButton::callback("拒绝", deny),
        ]]);
        let result = send_chunks(
            &self.rich,
            self.chat_id,
            &render_tool_call(call, "request id", &permission.request_id),
            Some(keyboard),
        )
        .await;
        if result.is_err() {
            remove_group(&self.approvals, &group_id).await;
        }
        result?;
        Ok(())
    }

    async fn save_selected_session(&self, session_id: Option<&str>) -> Result<()> {
        let snapshot = {
            let mut state = self.state.lock().await;
            state.selected_session_id = session_id.map(str::to_string);
            state.clone()
        };
        self.channels
            .save_state(ChannelKind::Telegram, &snapshot)
            .await
    }
}

const TELEGRAM_TEXT_CHUNK_CHARS: usize = 4096;

fn split_logical_message(text: &str) -> Vec<String> {
    split_message(text, TELEGRAM_TEXT_CHUNK_CHARS)
}

async fn send_chunks(
    rich: &RichMessageSender,
    chat_id: ChatId,
    text: &str,
    markup: Option<InlineKeyboardMarkup>,
) -> Result<()> {
    if chat_id.0 <= 0 {
        bail!("Telegram private chat id must be positive");
    }
    let mut fence = FenceState::default();
    for chunk in split_logical_message(text) {
        // 围栏跨段补平后原样走 Rich message：markdown 是唯一事实来源，
        // 不转换、不回退 —— 发送失败就记 ERROR 并抛出。
        let balanced = balance_fences(&chunk, &mut fence);
        rich.send_markdown(chat_id, &balanced, markup.as_ref())
            .await
            .inspect_err(|error| {
                tracing::error!(
                    event = "channel.telegram_message_failed",
                    chat_id = chat_id.0,
                    error = %format!("{error:#}"),
                    "Telegram rich message failed"
                );
            })?;
    }
    tracing::info!(
        event = "channel.telegram_rich_message_sent",
        chat_id = chat_id.0,
        "Telegram rich messages sent"
    );
    Ok(())
}

/// 直连 Bot API 发 Rich message —— teloxide 0.13 尚未封装
/// `sendRichMessage` 方法。发送失败不回退：记 ERROR 并抛给调用方。
struct RichMessageSender {
    client: reqwest::Client,
    token: String,
}

impl RichMessageSender {
    fn new(token: &str, proxy: Option<&str>) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30));
        // 空串按「没配代理」处理：直连（和 teloxide 客户端保持一致）。
        if let Some(proxy) = proxy.filter(|proxy| !proxy.trim().is_empty()) {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy)
                    .with_context(|| format!("configure Telegram HTTP proxy {proxy}"))?,
            );
        }
        Ok(Self {
            client: builder.build()?,
            token: token.to_string(),
        })
    }

    /// 发送一条 rich message；非 2xx 或 `ok != true` 一律算失败。
    async fn send_markdown(
        &self,
        chat_id: ChatId,
        markdown: &str,
        markup: Option<&InlineKeyboardMarkup>,
    ) -> Result<()> {
        let url = format!("https://api.telegram.org/bot{}/sendRichMessage", self.token);
        let response = self
            .client
            .post(url)
            .json(&rich_message_body(chat_id, markdown, markup)?)
            .send()
            .await?;
        let status = response.status();
        let payload: serde_json::Value = response.json().await?;
        if !status.is_success()
            || payload.get("ok").and_then(serde_json::Value::as_bool) != Some(true)
        {
            let description = payload
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown error");
            bail!("sendRichMessage rejected ({status}): {description}");
        }
        Ok(())
    }
}

fn rich_message_body(
    chat_id: ChatId,
    markdown: &str,
    markup: Option<&InlineKeyboardMarkup>,
) -> Result<serde_json::Value> {
    let mut body = serde_json::json!({
        "chat_id": chat_id.0,
        "rich_message": {"markdown": markdown},
    });
    if let Some(markup) = markup {
        body["reply_markup"] = serde_json::to_value(markup)?;
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_messages_split_without_changing_model_output() {
        let text = format!("  {}\n\n{}  ", "front".repeat(700), "back".repeat(400));
        let chunks = split_logical_message(&text);

        assert_eq!(chunks.concat(), text);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.chars().count() <= TELEGRAM_TEXT_CHUNK_CHARS)
        );
    }

    #[test]
    fn telegram_runtime_has_one_selected_session() {
        let state = TelegramChannelState {
            selected_session_id: Some("session-test".to_string()),
            ..Default::default()
        };
        assert_eq!(state.selected_session_id.as_deref(), Some("session-test"));
    }

    #[test]
    fn rich_message_body_carries_markdown() {
        let body = rich_message_body(ChatId(7), "| a | b |\n| --- | --- |", None).unwrap();
        assert_eq!(body["chat_id"].as_i64(), Some(7));
        assert_eq!(
            body["rich_message"]["markdown"].as_str(),
            Some("| a | b |\n| --- | --- |")
        );
        assert!(body.get("reply_markup").is_none());
    }

    #[tokio::test]
    async fn consuming_telegram_permission_action_removes_its_sibling() {
        let approvals = Mutex::new(std::collections::HashMap::from([
            (
                "allow".to_string(),
                PermissionAction {
                    group_id: "group".to_string(),
                    session_id: SessionId::parse("session-test").unwrap(),
                    request_id: "request-test".to_string(),
                    allowed: true,
                    expires_at: Instant::now() + Duration::from_secs(60),
                },
            ),
            (
                "deny".to_string(),
                PermissionAction {
                    group_id: "group".to_string(),
                    session_id: SessionId::parse("session-test").unwrap(),
                    request_id: "request-test".to_string(),
                    allowed: false,
                    expires_at: Instant::now() + Duration::from_secs(60),
                },
            ),
        ]));

        let action = take_action(&approvals, "allow").await.unwrap();
        assert!(action.allowed);
        assert!(approvals.lock().await.is_empty());
    }
}
