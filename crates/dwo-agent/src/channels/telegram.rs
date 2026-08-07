use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use dwo_agent_service::{ContentBlock, MessageContent, SessionId};
use teloxide::dispatching::Dispatcher;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{FileId, InputFile};
use tokio::sync::Mutex;

use crate::host::Host;

use super::ChannelKind;
use super::attachments::{
    attachment_directory, local_file_resource, media_mime_type, sanitize_filename,
    unique_attachment_path,
};
use super::bridge::{ChannelIngress, ConversationId, ConversationTransport};
use super::command::{command_descriptions, parse_command};
use super::gateway::{
    ChannelAdapter, ChannelBinder, ChannelBindingProgress, ChannelPollParams, ChannelRuntime,
    ChannelStarter, PreparedChannel,
};
use super::manager::{TelegramChannelState, telegram_bot};

pub(super) const CAPABILITY_PROMPT: &str = r#"A Telegram channel is bound. Your normal reasoning and responses are already streamed to the user through Telegram.

Do not use the proactive messaging commands for normal replies.
Only use `dwo channel telegram send-message <message>` when the user explicitly asks you to proactively send a specific message.
Only use `dwo channel telegram send-file <path>` when the user explicitly asks you to send a file.

Use `dwo channel telegram --help` to inspect the available commands."#;

type TelegramHandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

pub(crate) struct TelegramAdapter;

#[async_trait]
impl ChannelBinder for TelegramAdapter {
    async fn begin_bind(&self, host: Arc<Host>) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(
            host.channels().begin_telegram_bind().await?,
        )?)
    }

    async fn poll_bind(
        &self,
        host: Arc<Host>,
        params: ChannelPollParams,
    ) -> Result<ChannelBindingProgress> {
        Ok(host
            .channels()
            .poll_telegram_bind(&params.binding_id)
            .await?
            .into())
    }
}

struct TelegramStarter {
    bot: Bot,
    host: Arc<Host>,
    access: TelegramAccess,
}

pub(crate) struct RunningTelegram {
    bot: Bot,
    chat_id: ChatId,
    task: tokio::task::JoinHandle<()>,
    shutdown: teloxide::dispatching::ShutdownToken,
}

#[async_trait]
impl ChannelAdapter for TelegramAdapter {
    async fn prepare(&self, host: Arc<Host>) -> Result<PreparedChannel> {
        let runtime = host.channels().load_telegram().await?;
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
        let conversation = Arc::new(TelegramConversation {
            host: host.clone(),
            bot: bot.clone(),
            chat_id,
            state,
        });
        let access = TelegramAccess {
            user_id: runtime.secret.bound_user_id,
            chat_id: runtime.secret.bound_chat_id,
            media_input: runtime.config.media_input,
        };
        Ok(PreparedChannel {
            conversation: ConversationId::new("telegram", runtime.secret.bound_user_id.to_string()),
            replay_turns: runtime.config.replay_turns,
            replay_mode: runtime.config.replay_mode,
            selected_session_id,
            transport: conversation,
            starter: Box::new(TelegramStarter { bot, host, access }),
        })
    }
}

#[async_trait]
impl ChannelStarter for TelegramStarter {
    async fn start(
        self: Box<Self>,
        ingress: Arc<dyn ChannelIngress>,
    ) -> Result<Box<dyn ChannelRuntime>> {
        let handler = Update::filter_message().endpoint(handle_message);
        let mut dispatcher = Dispatcher::builder(self.bot.clone(), handler)
            .dependencies(dptree::deps![self.host, ingress, self.access])
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
        send_chunks(&self.bot, self.chat_id, text).await
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
    host: Arc<Host>,
    ingress: Arc<dyn ChannelIngress>,
    access: TelegramAccess,
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
        &host,
        ingress.as_ref(),
        text,
        media,
        access.media_input,
    )
    .await;
    match result {
        Ok(messages) => {
            for text in messages {
                send_chunks(&bot, message.chat.id, &text).await?;
            }
        }
        Err(error) => {
            send_chunks(&bot, message.chat.id, &format!("dwoagent error: {error:#}")).await?;
        }
    }
    Ok(())
}

async fn process_bound_message(
    bot: &Bot,
    host: &Host,
    ingress: &dyn ChannelIngress,
    text: &str,
    media: Option<IncomingMedia>,
    media_input: bool,
) -> Result<Vec<String>> {
    if text.starts_with('/') {
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
    host: &Host,
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
    host: &Host,
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
    host: Arc<Host>,
    bot: Bot,
    chat_id: ChatId,
    state: Arc<Mutex<TelegramChannelState>>,
}

#[async_trait]
impl ConversationTransport for TelegramConversation {
    async fn send_text(&self, text: &str) -> Result<()> {
        send_chunks(&self.bot, self.chat_id, text).await
    }

    async fn save_selected_session(&self, session_id: Option<&str>) -> Result<()> {
        let snapshot = {
            let mut state = self.state.lock().await;
            state.selected_session_id = session_id.map(str::to_string);
            state.clone()
        };
        self.host
            .channels()
            .save_state(ChannelKind::Telegram, &snapshot)
            .await
    }
}

const TELEGRAM_TEXT_CHUNK_CHARS: usize = 4096;

fn split_logical_message(text: &str) -> Vec<String> {
    let mut remaining = text;
    let mut chunks = Vec::new();
    while remaining.chars().count() > TELEGRAM_TEXT_CHUNK_CHARS {
        let hard_boundary = remaining
            .char_indices()
            .nth(TELEGRAM_TEXT_CHUNK_CHARS)
            .map(|(index, _)| index)
            .unwrap_or(remaining.len());
        let preferred_boundary = remaining[..hard_boundary]
            .rfind("\n\n")
            .map(|index| index + 2)
            .filter(|index| remaining[..*index].chars().count() >= TELEGRAM_TEXT_CHUNK_CHARS / 2);
        let boundary = preferred_boundary.unwrap_or(hard_boundary);
        chunks.push(remaining[..boundary].to_string());
        remaining = &remaining[boundary..];
    }
    if !remaining.is_empty() {
        chunks.push(remaining.to_string());
    }
    chunks
}

async fn send_chunks(bot: &Bot, chat_id: ChatId, text: &str) -> Result<()> {
    if chat_id.0 <= 0 {
        bail!("Telegram private chat id must be positive");
    }
    let mut first_error = None;
    for chunk in split_logical_message(text) {
        if let Err(error) = bot.send_message(chat_id, chunk).await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    if let Some(error) = first_error {
        return Err(error.into());
    }
    Ok(())
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
}
