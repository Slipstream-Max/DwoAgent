use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes_gcm::aead::{Aead, KeyInit, generic_array::GenericArray};
use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use botrs::interaction::Interaction;
use botrs::models::message::{
    C2CMessageParams, Keyboard, KeyboardButton, KeyboardButtonAction, KeyboardButtonPermission,
    KeyboardButtonRenderData, KeyboardContent, KeyboardRow, MarkdownPayload, Media,
};
use botrs::{BotApi, C2CMessage, Client, Context, EventHandler, Intents, Token};
use dwo_agent_service::{
    ActiveToolCall, ContentBlock, MessageContent, PendingPermission, SessionId,
};
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use crate::host::Host;
use crate::slash_commands::{parse_command, routes_to_channel_command};

use super::ChannelKind;
use super::attachments::{
    attachment_directory, local_file_resource, media_mime_type, sanitize_filename,
    unique_attachment_path,
};
use super::bridge::{ChannelIngress, ConversationId, ConversationTransport};
use super::gateway::{
    ChannelAdapter, ChannelBinder, ChannelBindingProgress, ChannelPollParams, ChannelRuntime,
    ChannelStarter, PreparedChannel,
};
use super::manager::{ChannelOutputMode, QqChannelState};
use super::render::render_tool_call;

pub(super) const CAPABILITY_PROMPT: &str = r#"A QQ channel is bound to one private C2C user. Normal reasoning and responses are already delivered through QQ.

Do not use proactive QQ messaging for normal replies. Only use `dwo channel qq send-message <message>` when the user explicitly asks you to proactively send a message, and `dwo channel qq send-file <path>` when the user explicitly asks you to send a file."#;

const QQ_API_BASE: &str = "https://api.sgroup.qq.com";
const QQ_BIND_BASE: &str = "https://q.qq.com";
const QQ_MAX_FILE_BYTES: usize = 20 * 1024 * 1024;
const QQ_TEXT_CHUNK_CHARS: usize = 1800;
const QQ_PASSIVE_REPLY_MAX_MESSAGES: u32 = 4;
const QQ_PASSIVE_REPLY_WINDOW: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone)]
pub(crate) struct QqBindTask {
    pub(crate) task_id: String,
    pub(crate) key: String,
}

#[derive(Debug)]
pub(crate) enum QqQrPoll {
    Waiting,
    Expired,
    Completed {
        app_id: String,
        app_secret: String,
        user_openid: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct BindResponse<T> {
    retcode: i32,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    data: Option<T>,
}

#[derive(Debug, Default, Deserialize)]
struct CreateBindData {
    task_id: String,
}

#[derive(Debug, Default, Deserialize)]
struct PollBindData {
    #[serde(default)]
    status: u8,
    #[serde(default, deserialize_with = "deserialize_optional_string_or_number")]
    bot_appid: Option<String>,
    #[serde(default)]
    bot_encrypt_secret: Option<String>,
    #[serde(default)]
    user_openid: Option<String>,
}

pub(crate) async fn create_bind_task() -> Result<QqBindTask> {
    let mut key_bytes = [0_u8; 32];
    getrandom::fill(&mut key_bytes).context("generate QQ QR binding key")?;
    let key = STANDARD.encode(key_bytes);
    let response = reqwest::Client::new()
        .post(format!("{QQ_BIND_BASE}/lite/create_bind_task"))
        .json(&json!({"key": key}))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("create QQ QR binding task")?;
    let response = response
        .error_for_status()
        .context("QQ QR binding task returned an HTTP error")?
        .json::<BindResponse<CreateBindData>>()
        .await
        .context("decode QQ QR binding task response")?;
    ensure!(
        response.retcode == 0,
        "QQ QR binding task failed: {}",
        response.msg.unwrap_or_default()
    );
    let task_id = response
        .data
        .context("QQ QR binding task omitted data")?
        .task_id;
    ensure!(
        !task_id.trim().is_empty(),
        "QQ QR binding task omitted task_id"
    );
    Ok(QqBindTask { task_id, key })
}

pub(crate) fn bind_qr_url(task_id: &str) -> String {
    let task_id = url::form_urlencoded::byte_serialize(task_id.as_bytes()).collect::<String>();
    format!("{QQ_BIND_BASE}/qqbot/openclaw/connect.html?task_id={task_id}&source=dwoagent&_wv=2",)
}

fn deserialize_optional_string_or_number<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    value
        .map(|value| match value {
            serde_json::Value::String(value) => Ok(value),
            serde_json::Value::Number(value) => Ok(value.to_string()),
            other => Err(serde::de::Error::custom(format!(
                "expected string or number, got {other}"
            ))),
        })
        .transpose()
}

pub(crate) async fn poll_bind_task(task_id: &str, key: &str) -> Result<QqQrPoll> {
    let response = reqwest::Client::new()
        .post(format!("{QQ_BIND_BASE}/lite/poll_bind_result"))
        .json(&json!({"task_id": task_id}))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("poll QQ QR binding task")?;
    let response = response
        .error_for_status()
        .context("QQ QR binding poll returned an HTTP error")?
        .json::<BindResponse<PollBindData>>()
        .await
        .context("decode QQ QR binding response")?;
    ensure!(
        response.retcode == 0,
        "QQ QR binding poll failed: {}",
        response.msg.unwrap_or_default()
    );
    let data = response.data.unwrap_or(PollBindData {
        status: 0,
        bot_appid: None,
        bot_encrypt_secret: None,
        user_openid: None,
    });
    match data.status {
        2 => {
            let app_id = data.bot_appid.context("QQ QR binding omitted bot_appid")?;
            let encrypted = data
                .bot_encrypt_secret
                .context("QQ QR binding omitted bot_encrypt_secret")?;
            let app_secret = decrypt_bind_secret(&encrypted, key)?;
            Ok(QqQrPoll::Completed {
                app_id,
                app_secret,
                user_openid: data.user_openid,
            })
        }
        3 => Ok(QqQrPoll::Expired),
        _ => Ok(QqQrPoll::Waiting),
    }
}

fn decrypt_bind_secret(encrypted_base64: &str, key_base64: &str) -> Result<String> {
    let key = STANDARD
        .decode(key_base64)
        .context("decode QQ QR binding AES key")?;
    ensure!(key.len() == 32, "QQ QR binding AES key must be 32 bytes");
    let encrypted = STANDARD
        .decode(encrypted_base64)
        .context("decode QQ encrypted secret")?;
    ensure!(
        encrypted.len() >= 12 + 16,
        "QQ encrypted secret is too short"
    );
    let cipher = aes_gcm::Aes256Gcm::new(GenericArray::from_slice(&key));
    let plaintext = cipher
        .decrypt(GenericArray::from_slice(&encrypted[..12]), &encrypted[12..])
        .map_err(|_| anyhow::anyhow!("decrypt QQ bot secret"))?;
    String::from_utf8(plaintext).context("QQ bot secret is not UTF-8")
}

pub(crate) async fn validate_credentials(app_id: &str, app_secret: &str) -> Result<()> {
    let token = Token::new(app_id, app_secret);
    token
        .validate()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let api = build_api(token)?;
    api.get_bot_info()
        .await
        .context("validate QQ bot credentials")?;
    Ok(())
}

fn build_api(token: Token) -> Result<BotApi> {
    let http = botrs::http::HttpClient::new(30, false)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(BotApi::new(http, token))
}

pub(crate) struct QqAdapter;

#[async_trait]
impl ChannelBinder for QqAdapter {
    async fn begin_bind(&self, host: Arc<Host>) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(
            host.channels().begin_qq_bind().await?,
        )?)
    }

    async fn poll_bind(
        &self,
        host: Arc<Host>,
        params: ChannelPollParams,
    ) -> Result<ChannelBindingProgress> {
        Ok(host
            .channels()
            .poll_qq_bind(&params.binding_id)
            .await?
            .into())
    }
}

struct QqStarter {
    host: Arc<Host>,
    api: BotApi,
    token: Token,
    openid: String,
    media_input: bool,
    conversation: Arc<QqConversation>,
    enabled: Arc<std::sync::atomic::AtomicBool>,
}

pub(crate) struct RunningQq {
    api: BotApi,
    token: Token,
    openid: String,
    task: tokio::task::JoinHandle<()>,
    enabled: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl ChannelAdapter for QqAdapter {
    async fn prepare(&self, host: Arc<Host>) -> Result<PreparedChannel> {
        let runtime = host.channels().load_qq().await?;
        let token = Token::new(runtime.app_id.clone(), runtime.app_secret.clone());
        let api = build_api(token.clone())?;
        api.get_bot_info().await.context("connect QQ bot")?;

        let state = Arc::new(Mutex::new(runtime.state));
        let selected_session_id = state.lock().await.selected_session_id.clone();
        let approvals = Arc::new(Mutex::new(HashMap::new()));
        let conversation = Arc::new(QqConversation {
            host: host.clone(),
            api: api.clone(),
            openid: runtime.secret.bound_user_openid.clone(),
            state,
            reply: Mutex::new(None),
            approvals: approvals.clone(),
            output_mode: runtime.config.output_mode,
        });
        let enabled = Arc::new(std::sync::atomic::AtomicBool::new(true));
        Ok(PreparedChannel {
            conversation: ConversationId::new("qq", runtime.secret.bound_user_openid.clone()),
            replay_turns: runtime.config.replay_turns,
            output_mode: runtime.config.output_mode,
            selected_session_id,
            transport: conversation.clone(),
            starter: Box::new(QqStarter {
                host,
                api,
                token,
                openid: runtime.secret.bound_user_openid,
                media_input: runtime.config.media_input,
                conversation,
                enabled,
            }),
        })
    }
}

#[async_trait]
impl ChannelStarter for QqStarter {
    async fn start(
        self: Box<Self>,
        ingress: Arc<dyn ChannelIngress>,
    ) -> Result<Box<dyn ChannelRuntime>> {
        let handler = QqHandler {
            host: self.host,
            ingress,
            conversation: self.conversation,
            bound_user_openid: self.openid.clone(),
            media_input: self.media_input,
            enabled: self.enabled.clone(),
        };
        let mut client = Client::new(
            self.token.clone(),
            Intents::new().with_public_messages().with_interaction(),
            handler,
            false,
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let task = tokio::spawn(async move {
            if let Err(error) = client.start().await {
                tracing::error!(
                    event = "channel.qq_stopped",
                    error = %error,
                    "QQ client stopped"
                );
            }
        });

        Ok(Box::new(RunningQq {
            api: self.api,
            token: self.token,
            openid: self.openid,
            task,
            enabled: self.enabled,
        }))
    }
}

#[async_trait]
impl ChannelRuntime for RunningQq {
    async fn stop(self: Box<Self>) {
        self.enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.task.abort();
    }

    async fn send_message(&self, text: &str) -> Result<()> {
        send_c2c_text(&self.api, &self.openid, text, None).await
    }

    async fn send_file(&self, path: &Path) -> Result<()> {
        send_local_file(&self.api, &self.token, &self.openid, path, None).await
    }
}

struct QqHandler {
    host: Arc<Host>,
    ingress: Arc<dyn ChannelIngress>,
    conversation: Arc<QqConversation>,
    bound_user_openid: String,
    media_input: bool,
    enabled: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl EventHandler for QqHandler {
    async fn c2c_message_create(&self, _ctx: Context, message: C2CMessage) {
        if !self.enabled.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let Some(openid) = message
            .author
            .as_ref()
            .and_then(|author| author.user_openid.as_deref())
        else {
            return;
        };
        if openid != self.bound_user_openid {
            return;
        }
        let text = message.content.as_deref().unwrap_or("").trim();
        if text.is_empty() && message.attachments.is_empty() {
            return;
        }
        let Some(message_id) = message.id.as_deref() else {
            tracing::warn!(
                event = "channel.qq_message_without_id",
                "ignore QQ message without id"
            );
            return;
        };
        self.conversation.begin_reply(message_id).await;

        if routes_to_channel_command(text) {
            let result = match parse_command(text) {
                Ok(command) => self.ingress.execute(command).await,
                Err(error) => Err(error),
            };
            match result {
                Ok(messages) => {
                    for response in messages {
                        if let Err(error) = self.conversation.send_text(&response).await {
                            tracing::warn!(event = "channel.qq_message_send_failed", error = %format!("{error:#}"), "send QQ response failed");
                        }
                    }
                }
                Err(error) => {
                    let _ = self
                        .conversation
                        .send_text(&format!("dwoagent error: {error:#}"))
                        .await;
                }
            }
            return;
        }

        if let Err(error) = self.process_prompt(&message, text).await {
            let _ = self
                .conversation
                .send_text(&format!("dwoagent error: {error:#}"))
                .await;
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        if !self.enabled.load(std::sync::atomic::Ordering::Relaxed)
            || interaction.user_openid.as_deref() != Some(self.bound_user_openid.as_str())
        {
            return;
        }
        let Some(interaction_id) = interaction.id.as_deref() else {
            return;
        };
        let action_id = interaction.data.resolved.button_data.as_str();
        let Some(action_id) = (!action_id.is_empty()).then_some(action_id) else {
            return;
        };
        let action = take_pending_action(&self.conversation.approvals, action_id).await;
        let Some(action) = action else {
            let _ = ctx.on_interaction_result(interaction_id, 0).await;
            return;
        };
        let _ = ctx.on_interaction_result(interaction_id, 0).await;
        if let Err(error) = self
            .ingress
            .resolve_permission(&action.session_id, &action.request_id, action.allowed)
            .await
        {
            tracing::warn!(event = "channel.qq_permission_resolve_failed", error = %format!("{error:#}"), "resolve QQ permission failed");
        }
    }

    async fn error(&self, error: botrs::BotError) {
        tracing::warn!(event = "channel.qq_handler_error", error = %error, "QQ event handler error");
    }
}

impl QqHandler {
    async fn process_prompt(&self, message: &C2CMessage, text: &str) -> Result<()> {
        let session_id = self.ingress.ensure_prompt_session().await?;
        let mut blocks = Vec::new();
        if !text.is_empty() {
            blocks.push(ContentBlock::text(text));
        }
        if !message.attachments.is_empty() {
            ensure!(self.media_input, "QQ media input is disabled");
            for (index, attachment) in message.attachments.iter().enumerate() {
                let path = download_attachment(&self.host, &session_id, attachment, index).await?;
                let mime = media_mime_type(
                    &path,
                    attachment
                        .content_type
                        .as_deref()
                        .unwrap_or("application/octet-stream"),
                );
                blocks.push(local_file_resource(&path, &mime)?);
            }
        }
        self.ingress
            .submit_prompt(MessageContent::blocks(blocks))
            .await
    }
}

struct ReplyContext {
    msg_id: String,
    next_seq: u32,
    remaining: u32,
    expires_at: Instant,
}

struct PendingAction {
    group_id: String,
    session_id: SessionId,
    request_id: String,
    allowed: bool,
    expires_at: Instant,
}

struct QqConversation {
    host: Arc<Host>,
    api: BotApi,
    openid: String,
    state: Arc<Mutex<QqChannelState>>,
    reply: Mutex<Option<ReplyContext>>,
    approvals: Arc<Mutex<HashMap<String, PendingAction>>>,
    output_mode: ChannelOutputMode,
}

impl QqConversation {
    async fn begin_reply(&self, msg_id: &str) {
        *self.reply.lock().await = Some(ReplyContext {
            msg_id: msg_id.to_string(),
            next_seq: 1,
            remaining: QQ_PASSIVE_REPLY_MAX_MESSAGES,
            expires_at: Instant::now() + QQ_PASSIVE_REPLY_WINDOW,
        });
    }

    async fn reserve_reply(&self) -> Option<(String, u32)> {
        let mut reply = self.reply.lock().await;
        let context = reply.as_mut()?;
        if Instant::now() >= context.expires_at {
            *reply = None;
            return None;
        }
        if context.remaining == 0 {
            return None;
        }
        let msg_id = context.msg_id.clone();
        let msg_seq = context.next_seq;
        context.next_seq = context.next_seq.saturating_add(1);
        context.remaining -= 1;
        Some((msg_id, msg_seq))
    }
}

#[async_trait]
impl ConversationTransport for QqConversation {
    async fn send_text(&self, text: &str) -> Result<()> {
        ensure!(!text.is_empty(), "QQ message must not be empty");
        let chunks = split_text(text);
        let replies = if self.output_mode == ChannelOutputMode::Full {
            vec![None; chunks.len()]
        } else {
            let mut replies = vec![None; chunks.len()];
            if let Some(reply) = self.reserve_reply().await {
                replies[0] = Some(reply);
            }
            replies
        };
        send_c2c_chunks(&self.api, &self.openid, chunks, replies).await
    }

    fn max_text_chars(&self) -> usize {
        QQ_TEXT_CHUNK_CHARS
    }

    fn defer_tool_call_to_permission(&self, mode: dwo_tools::SessionMode) -> bool {
        mode == dwo_tools::SessionMode::Confirm
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
            approvals.retain(|_, value| Instant::now() < value.expires_at);
            approvals.insert(
                allow.clone(),
                PendingAction {
                    group_id: group_id.clone(),
                    session_id: session_id.clone(),
                    request_id: permission.request_id.clone(),
                    allowed: true,
                    expires_at,
                },
            );
            approvals.insert(
                deny.clone(),
                PendingAction {
                    group_id,
                    session_id: session_id.clone(),
                    request_id: permission.request_id.clone(),
                    allowed: false,
                    expires_at,
                },
            );
        }
        let keyboard = Keyboard {
            id: None,
            content: Some(KeyboardContent {
                rows: Some(vec![KeyboardRow {
                    buttons: Some(vec![
                        permission_button("allow", "允许", "已允许", &allow, 1),
                        permission_button("deny", "拒绝", "已拒绝", &deny, 0),
                    ]),
                }]),
                style: None,
            }),
        };
        let mut params = C2CMessageParams {
            msg_type: 2,
            markdown: Some(MarkdownPayload {
                content: Some(render_tool_call(call, "request id", &permission.request_id)),
                ..Default::default()
            }),
            keyboard: Some(botrs::models::message::KeyboardPayload {
                content: serde_json::to_value(keyboard)?,
            }),
            ..Default::default()
        };
        if self.output_mode == ChannelOutputMode::Final
            && let Some((msg_id, msg_seq)) = self.reserve_reply().await
        {
            params.msg_id = Some(msg_id);
            params.msg_seq = Some(msg_seq);
        }
        self.api
            .send_c2c_message(&self.openid, params)
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(())
    }

    async fn save_selected_session(&self, session_id: Option<&str>) -> Result<()> {
        let snapshot = {
            let mut state = self.state.lock().await;
            state.selected_session_id = session_id.map(str::to_string);
            state.clone()
        };
        self.host
            .channels()
            .save_state(ChannelKind::Qq, &snapshot)
            .await
    }
}

fn permission_button(
    id: &str,
    label: &str,
    visited_label: &str,
    data: &str,
    style: u32,
) -> KeyboardButton {
    KeyboardButton {
        id: Some(id.to_string()),
        render_data: Some(KeyboardButtonRenderData {
            label: Some(label.to_string()),
            visited_label: Some(visited_label.to_string()),
            style: Some(style),
        }),
        action: Some(KeyboardButtonAction {
            action_type: Some(1),
            permission: Some(KeyboardButtonPermission {
                permission_type: Some(2),
                ..Default::default()
            }),
            click_limit: Some(1),
            data: Some(data.to_string()),
            ..Default::default()
        }),
        group_id: Some("permission".to_string()),
    }
}

fn new_action_id() -> Result<String> {
    let mut bytes = [0_u8; 9];
    getrandom::fill(&mut bytes).context("generate QQ permission action id")?;
    Ok(format!("perm_{}", URL_SAFE_NO_PAD.encode(bytes)))
}

async fn take_pending_action(
    approvals: &Mutex<HashMap<String, PendingAction>>,
    action_id: &str,
) -> Option<PendingAction> {
    let mut approvals = approvals.lock().await;
    approvals.retain(|_, value| Instant::now() < value.expires_at);
    let action = approvals.remove(action_id);
    if let Some(action) = &action {
        approvals.retain(|_, pending| pending.group_id != action.group_id);
    }
    action
}

async fn send_c2c_text(
    api: &BotApi,
    openid: &str,
    text: &str,
    reply: Option<(String, u32)>,
) -> Result<()> {
    ensure!(!text.is_empty(), "QQ message must not be empty");
    let chunks = split_text(text);
    let replies = match reply {
        Some((msg_id, msg_seq)) => chunks
            .iter()
            .enumerate()
            .map(|(index, _)| Some((msg_id.clone(), msg_seq.saturating_add(index as u32))))
            .collect(),
        None => vec![None; chunks.len()],
    };
    send_c2c_chunks(api, openid, chunks, replies).await
}

async fn send_c2c_chunks(
    api: &BotApi,
    openid: &str,
    chunks: Vec<String>,
    replies: Vec<Option<(String, u32)>>,
) -> Result<()> {
    ensure!(
        chunks.len() == replies.len(),
        "QQ chunks and reply plans differ"
    );
    for (chunk, reply) in chunks.into_iter().zip(replies) {
        let (msg_id, msg_seq) = reply.map_or((None, None), |(id, seq)| (Some(id), Some(seq)));
        let params = C2CMessageParams {
            content: Some(chunk),
            msg_id,
            msg_seq,
            ..Default::default()
        };
        api.send_c2c_message(openid, params)
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    }
    Ok(())
}

fn split_text(text: &str) -> Vec<String> {
    let mut remaining = text;
    let mut chunks = Vec::new();
    while remaining.chars().count() > QQ_TEXT_CHUNK_CHARS {
        let boundary = remaining
            .char_indices()
            .nth(QQ_TEXT_CHUNK_CHARS)
            .map(|(index, _)| index)
            .unwrap_or(remaining.len());
        chunks.push(remaining[..boundary].to_string());
        remaining = &remaining[boundary..];
    }
    if !remaining.is_empty() {
        chunks.push(remaining.to_string());
    }
    chunks
}

async fn download_attachment(
    host: &Host,
    session_id: &SessionId,
    attachment: &botrs::models::message::MessageAttachment,
    index: usize,
) -> Result<PathBuf> {
    let raw_url = attachment
        .url
        .as_deref()
        .context("QQ attachment omitted URL")?;
    let url = if raw_url.starts_with("//") {
        format!("https:{raw_url}")
    } else {
        raw_url.to_string()
    };
    ensure!(
        url.starts_with("https://"),
        "QQ attachment URL must use HTTPS"
    );
    if let Some(size) = attachment.size {
        ensure!(
            size as usize <= QQ_MAX_FILE_BYTES,
            "QQ attachment exceeds 20 MiB"
        );
    }
    let filename = sanitize_filename(
        attachment
            .filename
            .as_deref()
            .unwrap_or(&format!("attachment-{index}.bin")),
    );
    let filename = if filename.is_empty() {
        format!("attachment-{index}.bin")
    } else {
        filename
    };
    let directory = attachment_directory(host.profile_root_path(), "qq", session_id);
    tokio::fs::create_dir_all(&directory).await?;
    let destination = unique_attachment_path(&directory, &filename).await?;
    let response = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .context("download QQ attachment")?
        .error_for_status()
        .context("QQ attachment returned an HTTP error")?;
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read QQ attachment")?;
        ensure!(
            bytes.len() + chunk.len() <= QQ_MAX_FILE_BYTES,
            "QQ attachment exceeds 20 MiB"
        );
        bytes.extend_from_slice(&chunk);
    }
    tokio::fs::write(&destination, bytes).await?;
    Ok(destination)
}

async fn send_local_file(
    api: &BotApi,
    token: &Token,
    openid: &str,
    path: &Path,
    reply: Option<(String, u32)>,
) -> Result<()> {
    ensure!(path.is_file(), "file does not exist: {}", path.display());
    let metadata = tokio::fs::metadata(path).await?;
    ensure!(
        metadata.len() as usize <= QQ_MAX_FILE_BYTES,
        "QQ file exceeds 20 MiB"
    );
    let bytes = tokio::fs::read(path).await?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .map(sanitize_filename)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "attachment.bin".to_string());
    let file_type = qq_file_type(&file_name);
    let authorization = token
        .authorization_header()
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let client = reqwest::Client::new();
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&authorization).context("QQ authorization header")?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        "X-Union-Appid",
        HeaderValue::from_str(token.app_id()).context("QQ app ID header")?,
    );
    let upload_body = file_upload_body(file_type, &bytes, &file_name);
    let media = client
        .post(format!("{QQ_API_BASE}/v2/users/{openid}/files"))
        .headers(headers.clone())
        .json(&upload_body)
        .send()
        .await
        .context("upload QQ file")?
        .error_for_status()
        .context("QQ file upload returned an HTTP error")?
        .json::<Media>()
        .await
        .context("decode QQ file upload response")?;
    let (msg_id, msg_seq) = reply.map_or((None, None), |(id, seq)| (Some(id), Some(seq)));
    api.send_c2c_message(
        openid,
        C2CMessageParams {
            msg_type: 7,
            media: Some(media),
            msg_id,
            msg_seq,
            ..Default::default()
        },
    )
    .await
    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(())
}

fn file_upload_body(file_type: u32, bytes: &[u8], file_name: &str) -> serde_json::Value {
    let mut body = json!({
        "file_type": file_type,
        "file_data": STANDARD.encode(bytes),
        "srv_send_msg": false,
    });
    if file_type == 4 {
        body["file_name"] = json!(file_name);
    }
    body
}

fn qq_file_type(file_name: &str) -> u32 {
    match media_mime_type(Path::new(file_name), "application/octet-stream").as_str() {
        mime if mime.starts_with("image/") => 1,
        mime if mime.starts_with("video/") => 2,
        mime if mime.starts_with("audio/") => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_secret_decrypt_matches_official_iv_ciphertext_tag_layout() {
        let key = [0x21_u8; 32];
        let iv = [0x42_u8; 12];
        let cipher = aes_gcm::Aes256Gcm::new(GenericArray::from_slice(&key));
        let encrypted = cipher
            .encrypt(GenericArray::from_slice(&iv), b"qq-secret".as_slice())
            .unwrap();
        let mut wire = iv.to_vec();
        wire.extend_from_slice(&encrypted);
        let result = decrypt_bind_secret(&STANDARD.encode(wire), &STANDARD.encode(key)).unwrap();
        assert_eq!(result, "qq-secret");
    }

    #[test]
    fn text_chunks_preserve_message_content() {
        let text = format!("{}\n{}", "a".repeat(2000), "b".repeat(2000));
        assert_eq!(split_text(&text).concat(), text);
        assert!(
            split_text(&text)
                .iter()
                .all(|chunk| chunk.chars().count() <= QQ_TEXT_CHUNK_CHARS)
        );
    }

    #[test]
    fn permission_keyboard_uses_callback_actions() {
        let keyboard = Keyboard {
            id: None,
            content: Some(KeyboardContent {
                rows: Some(vec![KeyboardRow {
                    buttons: Some(vec![permission_button("allow", "允许", "已允许", "a", 1)]),
                }]),
                style: None,
            }),
        };
        let value = serde_json::to_value(keyboard).unwrap();
        assert_eq!(
            value["content"]["rows"][0]["buttons"][0]["action"]["type"],
            1
        );
        assert_eq!(
            value["content"]["rows"][0]["buttons"][0]["action"]["data"],
            "a"
        );
    }

    #[tokio::test]
    async fn consuming_one_permission_action_removes_its_sibling() {
        let approvals = Mutex::new(HashMap::from([
            (
                "allow".to_string(),
                PendingAction {
                    group_id: "group".to_string(),
                    session_id: SessionId::parse("session-test").unwrap(),
                    request_id: "request-test".to_string(),
                    allowed: true,
                    expires_at: Instant::now() + Duration::from_secs(60),
                },
            ),
            (
                "deny".to_string(),
                PendingAction {
                    group_id: "group".to_string(),
                    session_id: SessionId::parse("session-test").unwrap(),
                    request_id: "request-test".to_string(),
                    allowed: false,
                    expires_at: Instant::now() + Duration::from_secs(60),
                },
            ),
        ]));

        let action = take_pending_action(&approvals, "allow").await.unwrap();
        assert!(action.allowed);
        assert!(approvals.lock().await.is_empty());
    }

    #[test]
    fn generic_file_upload_includes_name_but_image_upload_does_not() {
        let file = file_upload_body(4, b"hello", "report.txt");
        assert_eq!(file["file_name"], "report.txt");
        assert_eq!(file["file_data"], STANDARD.encode(b"hello"));
        let image = file_upload_body(1, b"image", "photo.png");
        assert!(image.get("file_name").is_none());
    }
}
