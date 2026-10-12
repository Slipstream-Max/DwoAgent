use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use dwo_agent_service::{
    ActiveToolCall, ContentBlock, MessageContent, PendingPermission, SessionId,
};
use open_lark::auth::AuthService;
use open_lark::communication::im::v1::message::create::{CreateMessageBody, CreateMessageRequest};
use open_lark::communication::im::v1::message::models::ReceiveIdType;
use open_lark::communication::im::v1::message::patch::PatchMessageCardRequest;
use open_lark::communication::im::v1::message::resource::get::{
    GetMessageResourceRequest, MessageResourceType,
};
use open_lark::communication::{CommunicationClient, MediaFileUpload, MessageRecipient};
use open_lark::ws_client::{EventDispatcherHandler, LarkWsClient};
use serde::Deserialize;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use super::{ChannelBindingProgress, ChannelHost, ChannelManager, ChannelPollParams};
#[cfg(test)]
use dwo_command::render_command_help;
use dwo_command::{parse_command, routes_to_channel_command};

use super::ChannelKind;
use super::attachments::{
    attachment_directory, local_file_resource, media_mime_type, sanitize_filename,
    unique_attachment_path,
};
use super::bridge::{ChannelIngress, ConversationId, ConversationTransport};
use super::gateway::{
    ChannelAdapter, ChannelBinder, ChannelRuntime, ChannelStarter, PreparedChannel,
};
use super::manager::{FeishuChannelConfig, FeishuChannelState};
use super::markdown::{FenceState, balance_fences};
use super::permission::{
    PermissionAction, PermissionActionStore, PermissionCard, PermissionCardStore, new_action_id,
    put_card, remove_group, take_action, take_card,
};
use super::render::render_tool_call;
use super::render::split_message;

pub(super) const CAPABILITY_PROMPT: &str = r#"A Feishu/Lark private channel is bound. Your normal reasoning and responses are already streamed to the user through Feishu or Lark.

Do not use the proactive messaging commands for normal replies.
Only use `dwo channel feishu send-message <message>` when the user explicitly asks you to proactively send a specific message.
Only use `dwo channel feishu send-file <path>` when the user explicitly asks you to send a file.

Use `dwo channel feishu --help` to inspect the available commands."#;

// 卡片 JSON 有约 30KB 大小上限，多字节文本按 3 字节/字符留足余量。
const FEISHU_TEXT_CHUNK_CHARS: usize = 8_000;
const RECENT_MESSAGE_LIMIT: usize = 256;

pub struct FeishuAdapter;

#[async_trait]
impl ChannelBinder for FeishuAdapter {
    async fn begin_bind(
        &self,
        _host: Arc<dyn ChannelHost>,
        channels: Arc<ChannelManager>,
    ) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(channels.begin_feishu_bind().await?)?)
    }

    async fn poll_bind(
        &self,
        _host: Arc<dyn ChannelHost>,
        channels: Arc<ChannelManager>,
        params: ChannelPollParams,
    ) -> Result<ChannelBindingProgress> {
        Ok(channels
            .poll_feishu_bind(&params.binding_id)
            .await?
            .into())
    }
}

struct FeishuStarter {
    host: Arc<dyn ChannelHost>,
    api: FeishuApi,
    access: FeishuAccess,
    approvals: Arc<PermissionActionStore>,
    cards: Arc<PermissionCardStore>,
}

pub struct RunningFeishu {
    api: FeishuApi,
    connection_task: tokio::task::JoinHandle<()>,
    message_task: tokio::task::JoinHandle<()>,
    cancel: CancellationToken,
}

#[async_trait]
impl ChannelAdapter for FeishuAdapter {
    async fn prepare(
        &self,
        host: Arc<dyn ChannelHost>,
        channels: Arc<ChannelManager>,
    ) -> Result<PreparedChannel> {
        let runtime = channels.load_feishu().await?;
        let config = openlark_config(
            &runtime.config,
            runtime.app_id.clone(),
            runtime.app_secret.clone(),
        );
        validate_credentials(&config).await?;

        let state = Arc::new(Mutex::new(runtime.state));
        let selected_session_id = state.lock().await.selected_session_id.clone();
        let approvals = Arc::new(Mutex::new(HashMap::new()));
        let cards: Arc<PermissionCardStore> = Arc::new(Mutex::new(HashMap::new()));
        let api = FeishuApi {
            config,
            open_id: runtime.secret.bound_open_id.clone(),
        };
        let conversation = Arc::new(FeishuConversation {
            channels: channels.clone(),
            api: api.clone(),
            state,
            approvals: approvals.clone(),
            cards: cards.clone(),
        });
        let access = FeishuAccess {
            open_id: runtime.secret.bound_open_id,
            chat_id: runtime.secret.bound_chat_id,
            media_input: true,
        };
        Ok(PreparedChannel {
            conversation: ConversationId::new("feishu", access.open_id.clone()),
            replay_turns: runtime.config.replay_turns,
            selected_session_id,
            transport: conversation,
            starter: Box::new(FeishuStarter {
                host,
                api,
                access,
                approvals,
                cards,
            }),
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
            self.approvals,
            self.cards,
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

pub fn openlark_config(
    settings: &FeishuChannelConfig,
    app_id: String,
    app_secret: String,
) -> open_lark::Config {
    let config = open_lark::Config::builder()
        .app_id(app_id)
        .app_secret(app_secret)
        .base_url(settings.platform.base_url())
        .enable_token_cache(true)
        .req_timeout(Duration::from_secs(30))
        .build();
    // `CommunicationClient::new` 直接吃裸 config 时不会像统一 `Client` 那样自动
    // 装 token provider —— 发消息会报 `NoOpTokenProvider 未实现 token 获取逻辑`
    // （2026-10-10 实测：绑定成功但回执发不出去）。按官方推荐的两步法显式挂上
    // 带缓存的实现：先建「基础 Config」，再注入「业务 Config」，避免循环引用。
    config.with_token_provider(open_lark::auth::AuthTokenProvider::new(config.clone()))
}

pub async fn validate_credentials(config: &open_lark::Config) -> Result<()> {
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
    host: Arc<dyn ChannelHost>,
    ingress: Arc<dyn ChannelIngress>,
    api: FeishuApi,
    access: FeishuAccess,
    approvals: Arc<PermissionActionStore>,
    cards: Arc<PermissionCardStore>,
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
        match parse_feishu_action(&payload) {
            Ok(Some(action)) => {
                if action.open_id != access.open_id || action.chat_id != access.chat_id {
                    continue;
                }
                if let Some(pending) = take_action(&approvals, &action.action_id).await {
                    tracing::info!(
                        event = "channel.feishu_permission_action",
                        allowed = pending.allowed,
                        "Feishu permission action received"
                    );
                    if let Err(error) = ingress
                        .resolve_permission(
                            &pending.session_id,
                            &pending.request_id,
                            pending.allowed,
                        )
                        .await
                    {
                        tracing::debug!(
                            event = "channel.feishu_permission_already_resolved",
                            error = %format!("{error:#}"),
                            "Feishu permission action lost the cross-channel race"
                        );
                    }
                    // 卡片收口：去掉按钮、标上结果 —— 按过就不能再按。
                    if let Some(card) = take_card(&cards, &pending.group_id).await
                        && let Err(error) = api
                            .close_permission_card(&card.message_id, &card.text, pending.allowed)
                            .await
                    {
                        tracing::warn!(
                            event = "channel.feishu_card_patch_failed",
                            error = %format!("{error:#}"),
                            "close Feishu permission card failed"
                        );
                    }
                }
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    event = "channel.event_parse_failed",
                    channel = "feishu",
                    kind = "card_action",
                    error = %format!("{error:#}"),
                    "parse channel card action failed"
                );
                continue;
            }
        }
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
        let result = process_bound_message(
            host.as_ref(),
            ingress.as_ref(),
            &api,
            incoming,
            access.media_input,
        )
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
    host: &dyn ChannelHost,
    ingress: &dyn ChannelIngress,
    api: &FeishuApi,
    incoming: IncomingMessage,
    media_input: bool,
) -> Result<Vec<String>> {
    if incoming
        .text
        .as_deref()
        .is_some_and(routes_to_channel_command)
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
    host: &dyn ChannelHost,
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
    host: &dyn ChannelHost,
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

    async fn send_permission_card(&self, text: &str, allow: &str, deny: &str) -> Result<String> {
        let sent = CreateMessageRequest::new(self.config.clone())
            .receive_id_type(ReceiveIdType::OpenId)
            .execute(CreateMessageBody {
                receive_id: self.open_id.clone(),
                msg_type: "interactive".to_string(),
                content: serde_json::to_string(&feishu_permission_card(text, allow, deny))?,
                uuid: None,
            })
            .await
            .context("send Feishu permission card")?;
        Ok(sent["message_id"].as_str().unwrap_or_default().to_string())
    }

    /// 收口那张权限卡片：去掉按钮、标上结果 —— 「按过就不能再按」。
    async fn close_permission_card(
        &self,
        message_id: &str,
        text: &str,
        allowed: bool,
    ) -> Result<()> {
        // 没记到 message id 的老卡片就跳过（结果在会话里还有上下文）。
        if message_id.is_empty() {
            return Ok(());
        }
        let status = if allowed {
            "✅ **已允许**"
        } else {
            "⛔ **已拒绝**"
        };
        let card = serde_json::json!({
            "schema": "2.0",
            "config": {"update_multi": true},
            "body": {"elements": [
                {"tag": "markdown", "content": format!("{status}\n{text}")}
            ]}
        });
        PatchMessageCardRequest::new(self.config.clone())
            .message_id(message_id)
            .execute(serde_json::json!({"content": serde_json::to_string(&card)?}))
            .await
            .context("patch Feishu permission card")?;
        Ok(())
    }
}

struct FeishuAction {
    open_id: String,
    chat_id: String,
    action_id: String,
}

fn feishu_permission_card(text: &str, allow: &str, deny: &str) -> serde_json::Value {
    let button = |label: &str, element_id: &str, kind: &str, action_id: &str| {
        serde_json::json!({
            "tag": "button",
            "element_id": element_id,
            "text": {"tag": "plain_text", "content": label},
            "type": kind,
            "width": "fill",
            "size": "medium",
            "behaviors": [{
                "type": "callback",
                "value": {"action_id": action_id}
            }]
        })
    };
    serde_json::json!({
        "schema": "2.0",
        "config": {"update_multi": true},
        "body": {
            "elements": [
                {"tag": "markdown", "content": text},
                {
                    "tag": "column_set",
                    "flex_mode": "none",
                    "horizontal_spacing": "8px",
                    "columns": [
                        {
                            "tag": "column",
                            "width": "weighted",
                            "weight": 1,
                            "elements": [
                                button("允许", "allow_button", "primary_filled", allow)
                            ]
                        },
                        {
                            "tag": "column",
                            "width": "weighted",
                            "weight": 1,
                            "elements": [
                                button("拒绝", "deny_button", "danger_filled", deny)
                            ]
                        }
                    ]
                }
            ]
        }
    })
}

/// 出站正文的 markdown 卡片壳 —— 飞书 text 消息不认 markdown，卡片认。
fn feishu_markdown_card(text: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": "2.0",
        "config": {"update_multi": true},
        "body": {"elements": [{"tag": "markdown", "content": text}]}
    })
}

pub async fn send_text_to(config: &open_lark::Config, open_id: &str, text: &str) -> Result<()> {
    if text.is_empty() {
        bail!("Feishu message must not be empty");
    }
    let mut fence = FenceState::default();
    for chunk in split_logical_message(text) {
        // 正文原样包成 markdown 卡片；围栏被切在两半时补平。
        // 失败不回退 —— 记 ERROR 并抛出。
        let balanced = balance_fences(&chunk, &mut fence);
        CreateMessageRequest::new(config.clone())
            .receive_id_type(ReceiveIdType::OpenId)
            .execute(CreateMessageBody {
                receive_id: open_id.to_string(),
                msg_type: "interactive".to_string(),
                content: serde_json::to_string(&feishu_markdown_card(&balanced))?,
                uuid: None,
            })
            .await
            .inspect_err(|error| {
                tracing::error!(
                    event = "channel.feishu_message_failed",
                    error = %format!("{error:#}"),
                    "Feishu markdown card failed"
                );
            })
            .context("send Feishu markdown card")?;
    }
    Ok(())
}

fn split_logical_message(text: &str) -> Vec<String> {
    split_message(text, FEISHU_TEXT_CHUNK_CHARS)
}

#[derive(Clone)]
struct FeishuAccess {
    open_id: String,
    chat_id: String,
    media_input: bool,
}

struct FeishuConversation {
    channels: Arc<ChannelManager>,
    api: FeishuApi,
    state: Arc<Mutex<FeishuChannelState>>,
    approvals: Arc<PermissionActionStore>,
    cards: Arc<PermissionCardStore>,
}

#[async_trait]
impl ConversationTransport for FeishuConversation {
    async fn send_text(&self, text: &str) -> Result<()> {
        self.api.send_text(text).await
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
        let text = render_tool_call(call, "request id", &permission.request_id);
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
        let result = self
            .api
            .send_permission_card(&text, &allow, &deny)
            .await;
        match result {
            Ok(message_id) => {
                put_card(
                    &self.cards,
                    &group_id,
                    PermissionCard {
                        message_id,
                        text,
                        expires_at,
                    },
                )
                .await;
                Ok(())
            }
            Err(error) => {
                remove_group(&self.approvals, &group_id).await;
                Err(error)
            }
        }
    }

    async fn save_selected_session(&self, session_id: Option<&str>) -> Result<()> {
        let snapshot = {
            let mut state = self.state.lock().await;
            state.selected_session_id = session_id.map(str::to_string);
            state.clone()
        };
        self.channels
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
struct CardActionEnvelope {
    header: EventHeader,
    event: CardActionEvent,
}

#[derive(Deserialize)]
struct CardActionEvent {
    operator: CardActionOperator,
    action: CardActionData,
    context: CardActionContext,
}

#[derive(Deserialize)]
struct CardActionOperator {
    #[serde(default)]
    open_id: String,
    #[serde(default)]
    operator_id: Option<SenderId>,
}

impl CardActionOperator {
    fn open_id(self) -> String {
        if self.open_id.is_empty() {
            self.operator_id.map(|id| id.open_id).unwrap_or_default()
        } else {
            self.open_id
        }
    }
}

#[derive(Deserialize)]
struct CardActionData {
    value: CardActionValue,
}

#[derive(Deserialize)]
struct CardActionValue {
    #[serde(default)]
    action_id: String,
}

#[derive(Deserialize)]
struct CardActionContext {
    #[serde(default)]
    open_chat_id: String,
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

fn parse_feishu_action(payload: &[u8]) -> Result<Option<FeishuAction>> {
    let value: serde_json::Value = serde_json::from_slice(payload)?;
    if value["header"]["event_type"].as_str() != Some("card.action.trigger") {
        return Ok(None);
    }
    let envelope: CardActionEnvelope = serde_json::from_value(value)?;
    let open_id = envelope.event.operator.open_id();
    let chat_id = envelope.event.context.open_chat_id;
    let action_id = envelope.event.action.value.action_id;
    if envelope.header.event_type != "card.action.trigger"
        || open_id.is_empty()
        || chat_id.is_empty()
        || action_id.is_empty()
    {
        bail!("Feishu card action omitted identity fields");
    }
    Ok(Some(FeishuAction {
        open_id,
        chat_id,
        action_id,
    }))
}

pub fn bind_identity(payload: &[u8], code: &str) -> Option<(String, String)> {
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

    #[test]
    fn permission_card_contains_allow_and_deny_action_ids() {
        let card = feishu_permission_card("tool", "allow-id", "deny-id");
        assert_eq!(card["body"]["elements"][0]["tag"], "markdown");
        assert_eq!(card["body"]["elements"][1]["tag"], "column_set");
        let columns = &card["body"]["elements"][1]["columns"];
        assert_eq!(
            columns[0]["elements"][0]["behaviors"][0]["value"]["action_id"],
            "allow-id"
        );
        assert_eq!(
            columns[1]["elements"][0]["behaviors"][0]["value"]["action_id"],
            "deny-id"
        );
        assert_eq!(columns[0]["elements"][0]["tag"], "button");
        assert_eq!(columns[1]["elements"][0]["tag"], "button");
    }

    #[test]
    fn markdown_card_wraps_outbound_text() {
        let card = feishu_markdown_card("**加粗**\n\n```rust\nlet a = 1;\n```");
        assert_eq!(card["schema"], "2.0");
        assert_eq!(card["body"]["elements"][0]["tag"], "markdown");
        assert_eq!(
            card["body"]["elements"][0]["content"],
            "**加粗**\n\n```rust\nlet a = 1;\n```"
        );
    }

    #[test]
    fn card_action_is_parsed_for_the_bound_identity() {
        let payload = serde_json::to_vec(&json!({
            "header": {"event_type": "card.action.trigger"},
            "event": {
                "operator": {"open_id": "ou_user"},
                "action": {"value": {"action_id": "allow-id"}},
                "context": {"open_chat_id": "oc_chat"}
            }
        }))
        .unwrap();
        let action = parse_feishu_action(&payload).unwrap().unwrap();

        assert_eq!(action.open_id, "ou_user");
        assert_eq!(action.chat_id, "oc_chat");
        assert_eq!(action.action_id, "allow-id");
    }

    #[test]
    fn card_action_accepts_nested_operator_identity_for_compatible_providers() {
        let payload = serde_json::to_vec(&json!({
            "header": {"event_type": "card.action.trigger"},
            "event": {
                "operator": {"operator_id": {"open_id": "ou_nested"}},
                "action": {"value": {"action_id": "deny-id"}},
                "context": {"open_chat_id": "oc_chat"}
            }
        }))
        .unwrap();
        let action = parse_feishu_action(&payload).unwrap().unwrap();

        assert_eq!(action.open_id, "ou_nested");
        assert_eq!(action.action_id, "deny-id");
    }

    #[tokio::test]
    async fn consuming_feishu_permission_action_removes_its_sibling() {
        let approvals = Mutex::new(HashMap::from([
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
