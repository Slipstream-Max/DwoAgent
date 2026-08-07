use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use dwo_agent_service::{ContentBlock, MessageContent, SessionId};
use open_lark::auth::AuthService;
use open_lark::communication::im::v1::message::resource::get::{
    GetMessageResourceRequest, MessageResourceType,
};
use open_lark::communication::{CommunicationClient, MediaFileUpload, MessageRecipient};
use open_lark::ws_client::{EventDispatcherHandler, LarkWsClient};
use serde::Deserialize;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::host::Host;

use super::ChannelKind;
use super::attachments::{
    attachment_directory, local_file_resource, media_mime_type, sanitize_filename,
    unique_attachment_path,
};
use super::bridge::{ChannelIngress, ConversationId, ConversationTransport};
use super::command::parse_command;
#[cfg(test)]
use super::command::render_command_help;
use super::gateway::{
    ChannelAdapter, ChannelBinder, ChannelBindingProgress, ChannelPollParams, ChannelRuntime,
    ChannelStarter, PreparedChannel,
};
use super::manager::{FeishuChannelConfig, FeishuChannelState};

pub(super) const CAPABILITY_PROMPT: &str = r#"A Feishu/Lark private channel is bound. Your normal reasoning and responses are already streamed to the user through Feishu or Lark.

Do not use the proactive messaging commands for normal replies.
Only use `dwo channel feishu send-message <message>` when the user explicitly asks you to proactively send a specific message.
Only use `dwo channel feishu send-file <path>` when the user explicitly asks you to send a file.

Use `dwo channel feishu --help` to inspect the available commands."#;

const FEISHU_TEXT_CHUNK_CHARS: usize = 20_000;
const RECENT_MESSAGE_LIMIT: usize = 256;

pub(crate) struct FeishuAdapter;

#[async_trait]
impl ChannelBinder for FeishuAdapter {
    async fn begin_bind(&self, host: Arc<Host>) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(
            host.channels().begin_feishu_bind().await?,
        )?)
    }

    async fn poll_bind(
        &self,
        host: Arc<Host>,
        params: ChannelPollParams,
    ) -> Result<ChannelBindingProgress> {
        Ok(host
            .channels()
            .poll_feishu_bind(&params.binding_id)
            .await?
            .into())
    }
}

struct FeishuStarter {
    host: Arc<Host>,
    api: FeishuApi,
    access: FeishuAccess,
}

pub(crate) struct RunningFeishu {
    api: FeishuApi,
    connection_task: tokio::task::JoinHandle<()>,
    message_task: tokio::task::JoinHandle<()>,
    cancel: CancellationToken,
}

#[async_trait]
impl ChannelAdapter for FeishuAdapter {
    async fn prepare(&self, host: Arc<Host>) -> Result<PreparedChannel> {
        let runtime = host.channels().load_feishu().await?;
        let config = openlark_config(
            &runtime.config,
            runtime.app_id.clone(),
            runtime.app_secret.clone(),
        );
        validate_credentials(&config).await?;

        let state = Arc::new(Mutex::new(runtime.state));
        let selected_session_id = state.lock().await.selected_session_id.clone();
        let api = FeishuApi {
            config,
            open_id: runtime.secret.bound_open_id.clone(),
        };
        let conversation = Arc::new(FeishuConversation {
            host: host.clone(),
            api: api.clone(),
            state,
        });
        let access = FeishuAccess {
            open_id: runtime.secret.bound_open_id,
            chat_id: runtime.secret.bound_chat_id,
            media_input: runtime.config.media_input,
        };
        Ok(PreparedChannel {
            conversation: ConversationId::new("feishu", access.open_id.clone()),
            replay_turns: runtime.config.replay_turns,
            selected_session_id,
            transport: conversation,
            starter: Box::new(FeishuStarter { host, api, access }),
        })
    }
}

#[async_trait]
impl ChannelStarter for FeishuStarter {
    async fn start(
        self: Box<Self>,
        ingress: Arc<dyn ChannelIngress>,
    ) -> Result<Box<dyn ChannelRuntime>> {
        let (payload_tx, payload_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let connection_task = tokio::spawn(run_connection(
            self.api.config.clone(),
            payload_tx,
            cancel.child_token(),
        ));
        let message_task = tokio::spawn(run_messages(
            self.host,
            ingress,
            self.api.clone(),
            self.access.clone(),
            payload_rx,
            cancel.child_token(),
        ));
        Ok(Box::new(RunningFeishu {
            api: self.api,
            connection_task,
            message_task,
            cancel,
        }))
    }
}

#[async_trait]
impl ChannelRuntime for RunningFeishu {
    async fn stop(self: Box<Self>) {
        self.cancel.cancel();
        stop_task(self.connection_task).await;
        stop_task(self.message_task).await;
    }

    async fn send_message(&self, text: &str) -> Result<()> {
        self.api.send_text(text).await
    }

    async fn send_file(&self, path: &Path) -> Result<()> {
        self.api.send_file(path).await
    }
}

async fn stop_task(mut task: tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(Duration::from_secs(2), &mut task)
        .await
        .is_err()
    {
        task.abort();
    }
}

pub(crate) fn openlark_config(
    settings: &FeishuChannelConfig,
    app_id: String,
    app_secret: String,
) -> open_lark::Config {
    open_lark::Config::builder()
        .app_id(app_id)
        .app_secret(app_secret)
        .base_url(settings.platform.base_url())
        .enable_token_cache(true)
        .req_timeout(Duration::from_secs(30))
        .build()
}

pub(crate) async fn validate_credentials(config: &open_lark::Config) -> Result<()> {
    AuthService::new(config.clone())
        .v3()
        .tenant_access_token_internal()
        .app_id(config.app_id().to_string())
        .app_secret(config.app_secret().to_string())
        .execute()
        .await
        .context("validate Feishu application credentials")?;
    Ok(())
}

async fn run_connection(
    config: open_lark::Config,
    payload_tx: mpsc::UnboundedSender<Vec<u8>>,
    cancel: CancellationToken,
) {
    let handler = EventDispatcherHandler::builder()
        .payload_sender(payload_tx)
        .build();
    let mut retry_delay = Duration::from_secs(2);
    loop {
        let result = tokio::select! {
            _ = cancel.cancelled() => break,
            result = LarkWsClient::open(Arc::new(config.clone()), handler.clone()) => result,
        };
        tracing::warn!(
            event = "channel.connection_stopped",
            channel = "feishu",
            result = ?result,
            "channel connection stopped"
        );
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(retry_delay) => {}
        }
        retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
    }
}

async fn run_messages(
    host: Arc<Host>,
    ingress: Arc<dyn ChannelIngress>,
    api: FeishuApi,
    access: FeishuAccess,
    mut payload_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    cancel: CancellationToken,
) {
    let mut recent = RecentMessages::default();
    loop {
        let payload = tokio::select! {
            _ = cancel.cancelled() => break,
            payload = payload_rx.recv() => match payload {
                Some(payload) => payload,
                None => break,
            }
        };
        let incoming = match parse_incoming(&payload) {
            Ok(Some(incoming)) => incoming,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(
                    event = "channel.event_parse_failed",
                    channel = "feishu",
                    error = %format!("{error:#}"),
                    "parse channel event failed"
                );
                continue;
            }
        };
        if incoming.open_id != access.open_id
            || incoming.chat_id != access.chat_id
            || !recent.insert(&incoming.message_id)
        {
            continue;
        }
        let result =
            process_bound_message(&host, ingress.as_ref(), &api, incoming, access.media_input)
                .await;
        match result {
            Ok(messages) => {
                for message in messages {
                    if let Err(error) = api.send_text(&message).await {
                        tracing::warn!(
                            event = "channel.message_send_failed",
                            channel = "feishu",
                            kind = "command_response",
                            error = %format!("{error:#}"),
                            "send channel message failed"
                        );
                    }
                }
            }
            Err(error) => {
                if let Err(send_error) = api.send_text(&format!("dwoagent error: {error:#}")).await
                {
                    tracing::warn!(
                        event = "channel.message_send_failed",
                        channel = "feishu",
                        kind = "error_response",
                        error = %format!("{send_error:#}"),
                        "send channel message failed"
                    );
                }
            }
        }
    }
}

async fn process_bound_message(
    host: &Host,
    ingress: &dyn ChannelIngress,
    api: &FeishuApi,
    incoming: IncomingMessage,
    media_input: bool,
) -> Result<Vec<String>> {
    if incoming
        .text
        .as_deref()
        .is_some_and(|text| text.starts_with('/'))
        && incoming.media.is_none()
    {
        return ingress
            .execute(parse_command(incoming.text.as_deref().unwrap_or_default())?)
            .await;
    }
    let session_id = ingress.ensure_prompt_session().await?;
    let content = prompt_content(host, api, incoming, media_input, &session_id).await?;
    ingress.submit_prompt(content).await?;
    Ok(Vec::new())
}

async fn prompt_content(
    host: &Host,
    api: &FeishuApi,
    incoming: IncomingMessage,
    media_input: bool,
    session_id: &SessionId,
) -> Result<MessageContent> {
    let mut blocks = Vec::new();
    if let Some(text) = incoming.text.filter(|text| !text.is_empty()) {
        blocks.push(ContentBlock::text(text));
    }
    if let Some(media) = incoming.media {
        if !media_input {
            bail!("Feishu media input is disabled");
        }
        let (path, mime_type) =
            download_media(host, api, &incoming.message_id, media, session_id).await?;
        blocks.push(local_file_resource(&path, &mime_type)?);
    }
    Ok(MessageContent::blocks(blocks))
}

async fn download_media(
    host: &Host,
    api: &FeishuApi,
    message_id: &str,
    media: IncomingMedia,
    session_id: &SessionId,
) -> Result<(PathBuf, String)> {
    let (key, filename, resource_type, fallback_mime) = match media {
        IncomingMedia::Image { image_key } => (
            image_key,
            format!("image-{message_id}.img"),
            MessageResourceType::Image,
            "application/octet-stream",
        ),
        IncomingMedia::File {
            file_key,
            file_name,
        } => (
            file_key,
            file_name,
            MessageResourceType::File,
            "application/octet-stream",
        ),
    };
    let bytes = GetMessageResourceRequest::new(api.config.clone())
        .message_id(message_id)
        .file_key(key)
        .resource_type(resource_type)
        .execute()
        .await
        .context("download Feishu message resource")?;
    let (filename, fallback_mime) = if resource_type == MessageResourceType::Image {
        image_filename_and_mime(message_id, &bytes)
    } else {
        (filename, fallback_mime.to_string())
    };
    let directory = attachment_directory(host.profile_root_path(), "feishu", session_id);
    tokio::fs::create_dir_all(&directory).await?;
    let filename = sanitize_filename(&filename);
    let filename = if filename.is_empty() {
        "attachment.bin".to_string()
    } else {
        filename
    };
    let destination = unique_attachment_path(&directory, &filename).await?;
    tokio::fs::write(&destination, bytes)
        .await
        .with_context(|| format!("write Feishu attachment {}", destination.display()))?;
    let mime_type = media_mime_type(&destination, &fallback_mime);
    Ok((destination, mime_type))
}

fn image_filename_and_mime(message_id: &str, bytes: &[u8]) -> (String, String) {
    let (extension, mime_type) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        ("png", "image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        ("jpg", "image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        ("gif", "image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        ("webp", "image/webp")
    } else {
        ("img", "application/octet-stream")
    };
    (
        format!("image-{message_id}.{extension}"),
        mime_type.to_string(),
    )
}

#[derive(Clone)]
struct FeishuApi {
    config: open_lark::Config,
    open_id: String,
}

impl FeishuApi {
    async fn send_text(&self, text: &str) -> Result<()> {
        send_text_to(&self.config, &self.open_id, text).await
    }

    async fn send_file(&self, path: &Path) -> Result<()> {
        if !path.is_file() {
            bail!("file does not exist: {}", path.display());
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("Feishu file name must be valid UTF-8")?
            .to_string();
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("read file {}", path.display()))?;
        let client = CommunicationClient::new(self.config.clone());
        let uploaded = client
            .im
            .upload_file(MediaFileUpload::new(file_name, bytes))
            .await
            .context("upload Feishu file")?;
        client
            .im
            .send_file(
                MessageRecipient::open_id(self.open_id.clone()),
                uploaded.file_key,
            )
            .await
            .context("send Feishu file")?;
        Ok(())
    }
}

pub(crate) async fn send_text_to(
    config: &open_lark::Config,
    open_id: &str,
    text: &str,
) -> Result<()> {
    if text.is_empty() {
        bail!("Feishu message must not be empty");
    }
    let client = CommunicationClient::new(config.clone());
    for chunk in split_logical_message(text) {
        client
            .im
            .send_text(MessageRecipient::open_id(open_id), chunk)
            .await
            .context("send Feishu text message")?;
    }
    Ok(())
}

fn split_logical_message(text: &str) -> Vec<String> {
    let mut remaining = text;
    let mut chunks = Vec::new();
    while remaining.chars().count() > FEISHU_TEXT_CHUNK_CHARS {
        let hard_boundary = remaining
            .char_indices()
            .nth(FEISHU_TEXT_CHUNK_CHARS)
            .map(|(index, _)| index)
            .unwrap_or(remaining.len());
        let preferred_boundary = remaining[..hard_boundary]
            .rfind("\n\n")
            .map(|index| index + 2)
            .filter(|index| remaining[..*index].chars().count() >= FEISHU_TEXT_CHUNK_CHARS / 2);
        let boundary = preferred_boundary.unwrap_or(hard_boundary);
        chunks.push(remaining[..boundary].to_string());
        remaining = &remaining[boundary..];
    }
    if !remaining.is_empty() {
        chunks.push(remaining.to_string());
    }
    chunks
}

#[derive(Clone)]
struct FeishuAccess {
    open_id: String,
    chat_id: String,
    media_input: bool,
}

struct FeishuConversation {
    host: Arc<Host>,
    api: FeishuApi,
    state: Arc<Mutex<FeishuChannelState>>,
}

#[async_trait]
impl ConversationTransport for FeishuConversation {
    async fn send_text(&self, text: &str) -> Result<()> {
        self.api.send_text(text).await
    }

    async fn save_selected_session(&self, session_id: Option<&str>) -> Result<()> {
        let snapshot = {
            let mut state = self.state.lock().await;
            state.selected_session_id = session_id.map(str::to_string);
            state.clone()
        };
        self.host
            .channels()
            .save_state(ChannelKind::Feishu, &snapshot)
            .await
    }
}

#[derive(Debug)]
struct IncomingMessage {
    message_id: String,
    open_id: String,
    chat_id: String,
    text: Option<String>,
    media: Option<IncomingMedia>,
}

#[derive(Debug)]
enum IncomingMedia {
    Image { image_key: String },
    File { file_key: String, file_name: String },
}

#[derive(Deserialize)]
struct EventEnvelope {
    header: EventHeader,
    event: MessageEvent,
}

#[derive(Deserialize)]
struct EventHeader {
    event_type: String,
}

#[derive(Deserialize)]
struct MessageEvent {
    sender: EventSender,
    message: EventMessage,
}

#[derive(Deserialize)]
struct EventSender {
    sender_id: SenderId,
}

#[derive(Deserialize)]
struct SenderId {
    #[serde(default)]
    open_id: String,
}

#[derive(Deserialize)]
struct EventMessage {
    #[serde(default)]
    message_id: String,
    #[serde(default)]
    chat_id: String,
    #[serde(default)]
    chat_type: String,
    #[serde(default)]
    message_type: String,
    #[serde(default)]
    content: String,
}

#[derive(Deserialize)]
struct TextContent {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct ImageContent {
    #[serde(default)]
    image_key: String,
}

#[derive(Deserialize)]
struct FileContent {
    #[serde(default)]
    file_key: String,
    #[serde(default)]
    file_name: String,
}

fn parse_incoming(payload: &[u8]) -> Result<Option<IncomingMessage>> {
    let envelope: EventEnvelope = serde_json::from_slice(payload)?;
    if envelope.header.event_type != "im.message.receive_v1"
        || envelope.event.message.chat_type != "p2p"
    {
        return Ok(None);
    }
    let message = envelope.event.message;
    if message.message_id.is_empty()
        || message.chat_id.is_empty()
        || envelope.event.sender.sender_id.open_id.is_empty()
    {
        bail!("Feishu private message event omitted identity fields");
    }
    let (text, media) = match message.message_type.as_str() {
        "text" => {
            let content: TextContent = serde_json::from_str(&message.content)?;
            let text = content.text.trim().to_string();
            if text.is_empty() {
                return Ok(None);
            }
            (Some(text), None)
        }
        "image" => {
            let content: ImageContent = serde_json::from_str(&message.content)?;
            if content.image_key.is_empty() {
                bail!("Feishu image message omitted image_key");
            }
            (
                None,
                Some(IncomingMedia::Image {
                    image_key: content.image_key,
                }),
            )
        }
        "file" => {
            let content: FileContent = serde_json::from_str(&message.content)?;
            if content.file_key.is_empty() {
                bail!("Feishu file message omitted file_key");
            }
            let file_name = if content.file_name.trim().is_empty() {
                format!("file-{}.bin", message.message_id)
            } else {
                content.file_name
            };
            (
                None,
                Some(IncomingMedia::File {
                    file_key: content.file_key,
                    file_name,
                }),
            )
        }
        _ => return Ok(None),
    };
    Ok(Some(IncomingMessage {
        message_id: message.message_id,
        open_id: envelope.event.sender.sender_id.open_id,
        chat_id: message.chat_id,
        text,
        media,
    }))
}

pub(crate) fn bind_identity(payload: &[u8], code: &str) -> Option<(String, String)> {
    let incoming = parse_incoming(payload).ok()??;
    let text = incoming.text.as_deref()?;
    let mut parts = text.split_whitespace();
    let command = parts.next()?;
    (command == "/bind" && parts.next() == Some(code) && parts.next().is_none())
        .then_some((incoming.open_id, incoming.chat_id))
}

#[derive(Default)]
struct RecentMessages {
    order: VecDeque<String>,
    ids: HashSet<String>,
}

impl RecentMessages {
    fn insert(&mut self, message_id: &str) -> bool {
        if !self.ids.insert(message_id.to_string()) {
            return false;
        }
        self.order.push_back(message_id.to_string());
        if self.order.len() > RECENT_MESSAGE_LIMIT
            && let Some(oldest) = self.order.pop_front()
        {
            self.ids.remove(&oldest);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message_payload(message_type: &str, content: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "header": {"event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_id": {"open_id": "ou_user"}},
                "message": {
                    "message_id": "om_message",
                    "chat_id": "oc_chat",
                    "chat_type": "p2p",
                    "message_type": message_type,
                    "content": content.to_string()
                }
            }
        }))
        .unwrap()
    }

    #[test]
    fn private_text_and_bind_command_are_parsed() {
        let payload = message_payload("text", json!({"text": "/bind A1B2C3D4"}));
        let incoming = parse_incoming(&payload).unwrap().unwrap();

        assert_eq!(incoming.open_id, "ou_user");
        assert_eq!(incoming.chat_id, "oc_chat");
        assert_eq!(incoming.text.as_deref(), Some("/bind A1B2C3D4"));
        assert_eq!(
            bind_identity(&payload, "A1B2C3D4"),
            Some(("ou_user".to_string(), "oc_chat".to_string()))
        );
        assert_eq!(bind_identity(&payload, "WRONG"), None);
    }

    #[test]
    fn image_and_file_messages_become_media_prompts() {
        let image = parse_incoming(&message_payload("image", json!({"image_key": "img_1"})))
            .unwrap()
            .unwrap();
        assert!(matches!(
            image.media,
            Some(IncomingMedia::Image { ref image_key }) if image_key == "img_1"
        ));

        let file = parse_incoming(&message_payload(
            "file",
            json!({"file_key": "file_1", "file_name": "report.pdf"}),
        ))
        .unwrap()
        .unwrap();
        assert!(matches!(
            file.media,
            Some(IncomingMedia::File { ref file_key, ref file_name })
                if file_key == "file_1" && file_name == "report.pdf"
        ));
    }

    #[test]
    fn messages_split_without_changing_model_output() {
        let text = format!("  {}\n\n{}  ", "front".repeat(5000), "back".repeat(3000));
        let chunks = split_logical_message(&text);

        assert_eq!(chunks.concat(), text);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.chars().count() <= FEISHU_TEXT_CHUNK_CHARS)
        );
    }

    #[test]
    fn duplicate_message_ids_are_ignored() {
        let mut recent = RecentMessages::default();
        assert!(recent.insert("om_1"));
        assert!(!recent.insert("om_1"));
    }

    #[test]
    fn command_help_is_shared_with_other_channels() {
        assert!(render_command_help().contains("/allow"));
        assert!(render_command_help().contains("/deny"));
    }
}
