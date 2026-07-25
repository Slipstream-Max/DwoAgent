use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use dwo_agent_service::{ContentBlock, MessageContent, SessionId};
use tokio::sync::Mutex;
use weixin_agent::{
    MediaInfo, MediaType, MessageContext, MessageHandler, WeixinClient, WeixinConfig,
};

use crate::host::Host;

use super::ChannelKind;
#[cfg(test)]
use super::attachments::file_uri_from_path;
use super::attachments::{
    attachment_directory, local_file_resource, media_mime_type, sanitize_filename,
    unique_attachment_path,
};
use super::bridge::{ConversationId, ConversationTransport, SessionBridge};
use super::command::parse_command;
use super::manager::WeixinChannelState;

pub(super) const CAPABILITY_PROMPT: &str = r#"A Weixin channel is bound. Your normal reasoning and responses are already streamed to the user through Weixin.

Do not use the proactive messaging commands for normal replies.
Only use `dwo channel weixin send-message <message>` when the user explicitly asks you to proactively send a specific message.
Only use `dwo channel weixin send-file <path>` when the user explicitly asks you to send a file.

Use `dwo channel weixin --help` to inspect the available commands."#;

pub(crate) struct RunningWeixin {
    client: Arc<WeixinClient>,
    target: String,
    client_task: tokio::task::JoinHandle<()>,
    bridge: Arc<SessionBridge>,
}

impl RunningWeixin {
    pub(crate) async fn start(host: Arc<Host>) -> Result<Self> {
        let runtime = host.channels.load_weixin().await?;
        let target = runtime.secret.bound_user_id.clone();
        let state = Arc::new(Mutex::new(runtime.state));
        let selected_session_id = state.lock().await.selected_session_id.clone();
        let client_ref = Arc::new(OnceLock::new());
        let conversation = Arc::new(WeixinConversation {
            host: host.clone(),
            target: target.clone(),
            state: state.clone(),
            client: client_ref.clone(),
        });
        let bridge = Arc::new(SessionBridge::new(
            host.clone(),
            ConversationId::new("weixin", target.clone()),
            runtime.config.replay_turns,
            selected_session_id,
            conversation.clone(),
        ));
        let handler = WeixinHandler {
            host,
            bound_user_id: target.clone(),
            media_input: runtime.config.media_input,
            bridge: bridge.clone(),
            conversation: conversation.clone(),
        };
        let config = WeixinConfig::builder()
            .token(runtime.secret.bot_token)
            .base_url(runtime.secret.base_url)
            .markdown_filter(runtime.config.markdown_filter)
            .build()?;
        let client = Arc::new(WeixinClient::builder(config).on_message(handler).build()?);
        client
            .context_tokens()
            .import(state.lock().await.adapter.context_tokens.clone());
        let _ = client_ref.set(client.clone());
        if let Err(error) = bridge.resume_observer().await {
            eprintln!("restore Weixin session observer: {error:#}");
        }
        let running_client = client.clone();
        let task_state = state.clone();
        let client_task = tokio::spawn(async move {
            let sync_buf = task_state.lock().await.adapter.sync_buf.clone();
            if let Err(error) = running_client.start(sync_buf).await {
                eprintln!("Weixin channel stopped: {error}");
            }
        });
        Ok(Self {
            client,
            target,
            client_task,
            bridge,
        })
    }

    pub(crate) async fn stop(self) {
        self.client.shutdown();
        self.bridge.stop().await;
        let mut client_task = self.client_task;
        if tokio::time::timeout(Duration::from_secs(5), &mut client_task)
            .await
            .is_err()
        {
            client_task.abort();
        }
    }

    pub(crate) async fn send_message(&self, text: &str) -> Result<()> {
        let context_token = self.client.context_tokens().get(&self.target);
        self.client
            .send_text(&self.target, text, context_token.as_deref())
            .await?;
        Ok(())
    }

    pub(crate) async fn send_file(&self, path: &Path) -> Result<()> {
        if !path.is_file() {
            bail!("file does not exist: {}", path.display());
        }
        let context_token = self.client.context_tokens().get(&self.target);
        self.client
            .send_media(&self.target, path, context_token.as_deref())
            .await?;
        Ok(())
    }
}

struct WeixinConversation {
    host: Arc<Host>,
    target: String,
    state: Arc<Mutex<WeixinChannelState>>,
    client: Arc<OnceLock<Arc<WeixinClient>>>,
}

impl WeixinConversation {
    fn client(&self) -> Result<Arc<WeixinClient>> {
        self.client
            .get()
            .cloned()
            .context("Weixin client is not ready")
    }

    async fn save_runtime(&self, sync_buf: Option<&str>) -> Result<()> {
        let snapshot = {
            let mut state = self.state.lock().await;
            if let Some(sync_buf) = sync_buf {
                state.adapter.sync_buf = Some(sync_buf.to_string());
            }
            if let Some(client) = self.client.get() {
                state.adapter.context_tokens = client.context_tokens().export_all();
            }
            state.clone()
        };
        self.host
            .channels
            .save_state(ChannelKind::Weixin, &snapshot)
            .await
    }
}

#[async_trait]
impl ConversationTransport for WeixinConversation {
    async fn send_text(&self, text: &str) -> Result<()> {
        let client = self.client()?;
        let mut first_error = None;
        for chunk in split_logical_message(text) {
            let context_token = client.context_tokens().get(&self.target);
            if let Err(error) = client
                .send_text(&self.target, &chunk, context_token.as_deref())
                .await
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

    async fn save_selected_session(&self, session_id: Option<&str>) -> Result<()> {
        let snapshot = {
            let mut state = self.state.lock().await;
            state.selected_session_id = session_id.map(str::to_string);
            state.clone()
        };
        self.host
            .channels
            .save_state(ChannelKind::Weixin, &snapshot)
            .await
    }
}

#[derive(Clone)]
struct WeixinHandler {
    host: Arc<Host>,
    bound_user_id: String,
    media_input: bool,
    bridge: Arc<SessionBridge>,
    conversation: Arc<WeixinConversation>,
}

#[async_trait]
impl MessageHandler for WeixinHandler {
    async fn on_message(&self, ctx: &MessageContext) -> weixin_agent::Result<()> {
        if ctx.from != self.bound_user_id {
            return Ok(());
        }
        let text = ctx.body.as_deref().unwrap_or("").trim();
        let result = if text.starts_with('/') {
            self.handle_command(ctx, text).await
        } else if text.is_empty() && ctx.media.is_none() {
            Ok(())
        } else {
            self.handle_prompt(ctx, text).await
        };
        if let Err(error) = result {
            let _ = ctx.reply_text(&format!("dwoagent error: {error:#}")).await;
        }
        Ok(())
    }

    async fn on_sync_buf_updated(&self, sync_buf: &str) -> weixin_agent::Result<()> {
        if let Err(error) = self.conversation.save_runtime(Some(sync_buf)).await {
            eprintln!("save Weixin sync state: {error:#}");
        }
        Ok(())
    }

    async fn on_shutdown(&self) -> weixin_agent::Result<()> {
        if let Err(error) = self.conversation.save_runtime(None).await {
            eprintln!("save Weixin shutdown state: {error:#}");
        }
        Ok(())
    }
}

impl WeixinHandler {
    async fn handle_command(&self, ctx: &MessageContext, text: &str) -> Result<()> {
        let messages = self.bridge.execute(parse_command(text)?).await?;
        for message in messages {
            reply_logical_message(ctx, &message).await?;
        }
        Ok(())
    }

    async fn handle_prompt(&self, ctx: &MessageContext, text: &str) -> Result<()> {
        let session_id = self.bridge.ensure_prompt_session().await?;
        let content = self.prompt_content(ctx, text, &session_id).await?;
        self.bridge.submit_prompt(content).await
    }

    async fn prompt_content(
        &self,
        ctx: &MessageContext,
        text: &str,
        session_id: &SessionId,
    ) -> Result<MessageContent> {
        let mut blocks = Vec::new();
        if !text.is_empty() {
            blocks.push(ContentBlock::text(text));
        }
        if let Some(media) = &ctx.media {
            if !self.media_input {
                bail!("Weixin media input is disabled");
            }
            let path = self.download_media(ctx, media, session_id).await?;
            blocks.push(resource_link_for_media(&path, media.media_type)?);
        }
        Ok(MessageContent::blocks(blocks))
    }

    async fn download_media(
        &self,
        ctx: &MessageContext,
        media: &MediaInfo,
        session_id: &SessionId,
    ) -> Result<PathBuf> {
        let directory = attachment_directory(self.host.profile_root_path(), "weixin", session_id);
        tokio::fs::create_dir_all(&directory).await?;
        let filename = media
            .file_name
            .as_deref()
            .map(sanitize_filename)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| default_media_filename(media.media_type));
        let destination = unique_attachment_path(&directory, &filename).await?;
        Ok(ctx.download_media(media, &destination).await?)
    }
}

fn default_media_filename(media_type: MediaType) -> String {
    match media_type {
        MediaType::Image => "image.jpg",
        MediaType::Video => "video.mp4",
        MediaType::Voice => "voice.dat",
        MediaType::File => "file.bin",
    }
    .to_string()
}

fn resource_link_for_media(path: &Path, media_type: MediaType) -> Result<ContentBlock> {
    let fallback = match media_type {
        MediaType::Image => "image/jpeg",
        MediaType::Voice => "audio/mpeg",
        MediaType::Video => "video/mp4",
        MediaType::File => "application/octet-stream",
    };
    let mime_type = media_mime_type(path, fallback);
    local_file_resource(path, &mime_type)
}

const WEIXIN_TEXT_CHUNK_CHARS: usize = 4000;

fn split_logical_message(text: &str) -> Vec<String> {
    let mut remaining = text.trim();
    let mut chunks = Vec::new();
    while remaining.chars().count() > WEIXIN_TEXT_CHUNK_CHARS {
        let hard_boundary = remaining
            .char_indices()
            .nth(WEIXIN_TEXT_CHUNK_CHARS)
            .map(|(index, _)| index)
            .unwrap_or(remaining.len());
        let preferred_boundary = remaining[..hard_boundary]
            .rfind("\n\n")
            .map(|index| index + 2)
            .filter(|index| remaining[..*index].chars().count() >= WEIXIN_TEXT_CHUNK_CHARS / 2);
        let boundary = preferred_boundary.unwrap_or(hard_boundary);
        let chunk = remaining[..boundary].trim();
        if !chunk.is_empty() {
            chunks.push(chunk.to_string());
        }
        remaining = remaining[boundary..].trim_start();
    }
    if !remaining.is_empty() {
        chunks.push(remaining.to_string());
    }
    chunks
}

async fn reply_logical_message(ctx: &MessageContext, text: &str) -> Result<()> {
    for chunk in split_logical_message(text) {
        ctx.reply_text(&chunk).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_messages_split_at_four_thousand_unicode_characters() {
        let text = format!("{}\n\n{}", "前".repeat(3_000), "后".repeat(2_000));
        let chunks = split_logical_message(&text);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), 3_000);
        assert_eq!(chunks[1].chars().count(), 2_000);
        assert_eq!(
            chunks.concat(),
            format!("{}{}", "前".repeat(3_000), "后".repeat(2_000))
        );
    }

    #[test]
    fn media_resource_includes_file_uri_mime_size_and_local_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("paper example.pdf");
        std::fs::write(&path, b"pdf").unwrap();

        let block = resource_link_for_media(&path, MediaType::File).unwrap();
        let ContentBlock::ResourceLink {
            uri,
            mime_type,
            description,
            size,
            ..
        } = block
        else {
            panic!("expected resource link");
        };
        assert!(uri.ends_with("paper%20example.pdf"));
        assert_eq!(mime_type.as_deref(), Some("application/pdf"));
        assert_eq!(size, Some(3));
        assert!(description.unwrap().contains("paper example.pdf"));
    }

    #[test]
    fn windows_verbatim_paths_are_hidden_in_file_uris() {
        let path = Path::new(r"\\?\C:\Users\Example User\paper.pdf");
        assert_eq!(
            file_uri_from_path(path),
            "file:///C:/Users/Example%20User/paper.pdf"
        );
    }
}
