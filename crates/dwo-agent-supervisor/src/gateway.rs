//! Supervisor-owned external activation gateway.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clawrs_feishu::{
    Channel, ChannelMessage, FeishuConfig as ClawFeishuConfig,
    FeishuConnectionMode as ClawFeishuConnectionMode, FeishuDomain as ClawFeishuDomain,
    create_channel,
};
use dwo_agent_core::automation::{
    AutomationNotificationRecord, AutomationNotificationStatus, AutomationNotifyChannel,
    load_automation_config, next_schedule_delay,
};
use dwo_agent_core::config::loader::{channel_secret_dir, resolve_agent_structure_dir};
use dwo_agent_core::ingress::channel_input::{
    extension_for_mime_type, image_mime_type_for_path, sanitize_filename, sanitize_filename_or,
};
use dwo_agent_core::ingress::config::{
    FeishuChannelConfig, FeishuChannelDomain, WeixinChannelConfig, load_channel_runtime_config,
};
use dwo_agent_core::protocol::dwo::{
    self, DwoChannelCommand, DwoIngressAttachment, DwoIngressChannel, DwoIngressConversation,
    DwoIngressEvent, DwoIngressSource, DwoOutboundAction, DwoOutboundActionNotification,
    DwoOutboundBody,
};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use weixin_agent::{
    MediaInfo, MediaType, MessageContext, MessageHandler, WeixinClient, WeixinConfig,
};

use super::{ResolvedProfile, SupervisorState};

const FEISHU_AUTH_FILE: &str = "auth.yaml";
const FEISHU_SECRET_SUBDIR: &str = "feishu";
const FEISHU_STATE_SUBDIR: &str = "feishu";
const WEIXIN_AUTH_FILE: &str = "auth.yaml";
const WEIXIN_CONTEXT_TOKENS_FILE: &str = "context_tokens.json";
const WEIXIN_SECRET_SUBDIR: &str = "weixin";
const WEIXIN_STATE_SUBDIR: &str = "weixin";
const WEIXIN_SYNC_BUF_FILE: &str = "sync_buf.txt";

pub(super) async fn spawn_gateways(state: Arc<SupervisorState>) -> Vec<JoinHandle<()>> {
    let mut tasks = Vec::new();
    for profile_config in state.config.profiles.clone() {
        let profile = ResolvedProfile {
            id: profile_config.id,
            path: profile_config.path,
        };
        let state = state.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(err) = run_profile_gateway(state, profile).await {
                tracing::warn!(error = %format!("{err:#}"), "profile gateway stopped");
            }
        }));
    }
    tasks
}

async fn run_profile_gateway(state: Arc<SupervisorState>, profile: ResolvedProfile) -> Result<()> {
    let agent_structure_dir = resolve_agent_structure_dir(&profile.path)?;
    let channel_config = load_channel_runtime_config(&agent_structure_dir)?;
    let automation_config = load_automation_config(&agent_structure_dir)?;
    if !channel_config.has_enabled_channels() && !automation_config.has_enabled_jobs() {
        return futures::future::pending::<Result<()>>().await;
    }

    let delivery = Arc::new(GatewayDelivery::new());
    let mut tasks = Vec::new();

    if channel_config.weixin.enabled {
        match WeixinGateway::new(
            state.clone(),
            profile.clone(),
            &agent_structure_dir,
            &channel_config.weixin,
        )
        .await
        {
            Ok(weixin) => {
                let weixin = Arc::new(weixin);
                delivery.set_weixin(weixin.clone()).await;
                tasks.push(tokio::spawn(async move { weixin.run().await }));
            }
            Err(err) => {
                tracing::warn!(profile = %profile.id, error = %format!("{err:#}"), "weixin gateway disabled");
            }
        }
    }

    if channel_config.feishu.enabled {
        match FeishuGateway::new(
            state.clone(),
            profile.clone(),
            &agent_structure_dir,
            &channel_config.feishu,
        )
        .await
        {
            Ok(feishu) => {
                let feishu = Arc::new(feishu);
                delivery.set_feishu(feishu.clone()).await;
                tasks.push(tokio::spawn(async move { feishu.run().await }));
            }
            Err(err) => {
                tracing::warn!(profile = %profile.id, error = %format!("{err:#}"), "feishu gateway disabled");
            }
        }
    }

    if automation_config.has_enabled_jobs() {
        for job in automation_config.jobs.into_iter().filter(|job| job.enabled) {
            let state = state.clone();
            let profile = profile.clone();
            let delivery = delivery.clone();
            tasks.push(tokio::spawn(async move {
                run_automation_job_loop(state, profile, delivery, job.id, job.schedule).await
            }));
        }
    }

    if tasks.is_empty() {
        return futures::future::pending::<Result<()>>().await;
    }

    let Some(result) = futures::future::select_all(tasks).await.0.ok() else {
        return Ok(());
    };
    result
}

async fn worker_request(
    state: &SupervisorState,
    profile: &ResolvedProfile,
    method: &str,
    params: Value,
) -> Result<Value> {
    let profile_id = profile.id.clone();
    state
        .worker_pool
        .request_with_events(profile, method, params, &state.config.pool, |event| {
            let profile_id = profile_id.clone();
            async move {
                state
                    .event_bus
                    .broadcast_worker_event(&profile_id, &event, None)
                    .await;
                Ok(())
            }
        })
        .await
}

async fn worker_notify(
    state: &SupervisorState,
    profile: &ResolvedProfile,
    method: &str,
    params: Value,
) -> Result<()> {
    state
        .worker_pool
        .notify(profile, method, params, &state.config.pool)
        .await
}

async fn run_automation_job_loop(
    state: Arc<SupervisorState>,
    profile: ResolvedProfile,
    delivery: Arc<GatewayDelivery>,
    job_id: String,
    schedule: dwo_agent_core::automation::AutomationSchedule,
) -> Result<()> {
    loop {
        let delay = next_schedule_delay(&schedule)?;
        tokio::time::sleep(delay).await;
        let response = worker_request(
            &state,
            &profile,
            "_dwo/automation/run_job",
            json!({ "job_id": job_id }),
        )
        .await?;
        let notifications = response
            .get("notifications")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(serde_json::from_value::<DwoOutboundAction>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let delivery_records = delivery.deliver_notifications(&notifications).await;
        if delivery_records.is_empty() {
            continue;
        }
        let Some(record) = response.get("record") else {
            continue;
        };
        let run_id = record.get("run_id").and_then(Value::as_str).unwrap_or("");
        let session_id = record
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        if run_id.is_empty() || session_id.is_empty() {
            continue;
        }
        let _ = worker_request(
            &state,
            &profile,
            "_dwo/automation/record_delivery",
            json!({
                "job_id": job_id,
                "run_id": run_id,
                "session_id": session_id,
                "notifications": delivery_records,
            }),
        )
        .await;
    }
}

struct GatewayDelivery {
    weixin: Mutex<Option<Arc<WeixinGateway>>>,
    feishu: Mutex<Option<Arc<FeishuGateway>>>,
}

impl GatewayDelivery {
    fn new() -> Self {
        Self {
            weixin: Mutex::new(None),
            feishu: Mutex::new(None),
        }
    }

    async fn set_weixin(&self, gateway: Arc<WeixinGateway>) {
        *self.weixin.lock().await = Some(gateway);
    }

    async fn set_feishu(&self, gateway: Arc<FeishuGateway>) {
        *self.feishu.lock().await = Some(gateway);
    }

    async fn deliver_notifications(&self, actions: &[DwoOutboundAction]) -> Vec<Value> {
        let mut records = Vec::new();
        for action in actions {
            let channel = match action.channel {
                DwoIngressChannel::Weixin => AutomationNotifyChannel::Weixin,
                DwoIngressChannel::Feishu => AutomationNotifyChannel::Feishu,
            };
            let recipient = (!action.target.is_empty()).then(|| action.target.clone());
            let result = match action.channel {
                DwoIngressChannel::Weixin => match self.weixin.lock().await.clone() {
                    Some(gateway) => gateway.deliver(action).await,
                    None => bail_action("weixin gateway is not running"),
                },
                DwoIngressChannel::Feishu => match self.feishu.lock().await.clone() {
                    Some(gateway) => gateway.deliver(action).await,
                    None => bail_action("feishu gateway is not running"),
                },
            };
            let record = match result {
                Ok(message_id) => AutomationNotificationRecord {
                    channel,
                    recipient,
                    status: AutomationNotificationStatus::Sent,
                    message_id: Some(message_id),
                    error: None,
                },
                Err(err) => AutomationNotificationRecord {
                    channel,
                    recipient,
                    status: AutomationNotificationStatus::Failed,
                    message_id: None,
                    error: Some(format!("{err:#}")),
                },
            };
            if let Ok(value) = serde_json::to_value(record) {
                records.push(value);
            }
        }
        records
    }
}

fn bail_action<T>(message: &str) -> Result<T> {
    bail!("{message}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WeixinAuth {
    bot_token: String,
    base_url: String,
    ilink_bot_id: String,
    bound_user_id: String,
    #[serde(default)]
    route_tag: Option<u32>,
}

struct WeixinGateway {
    client: Arc<WeixinClient>,
    initial_sync_buf: Option<String>,
}

#[derive(Clone)]
struct WeixinGatewayHandler {
    state: Arc<SupervisorState>,
    profile: ResolvedProfile,
    auth: WeixinAuth,
    state_dir: PathBuf,
    media_input: bool,
    client: Arc<OnceLock<Arc<WeixinClient>>>,
    message_lock: Arc<Mutex<()>>,
}

impl WeixinGateway {
    async fn new(
        state: Arc<SupervisorState>,
        profile: ResolvedProfile,
        agent_structure_dir: &Path,
        config: &WeixinChannelConfig,
    ) -> Result<Self> {
        let secret_dir = channel_secret_dir(agent_structure_dir).join(WEIXIN_SECRET_SUBDIR);
        let state_dir = agent_structure_dir
            .join("runtime")
            .join("channel_state")
            .join(WEIXIN_STATE_SUBDIR);
        let auth = read_weixin_auth(&secret_dir.join(WEIXIN_AUTH_FILE))?;
        let client_ref = Arc::new(OnceLock::new());
        let handler = WeixinGatewayHandler {
            state,
            profile,
            auth: auth.clone(),
            state_dir: state_dir.clone(),
            media_input: config.media_input,
            client: client_ref.clone(),
            message_lock: Arc::new(Mutex::new(())),
        };

        let mut builder = WeixinConfig::builder()
            .token(auth.bot_token)
            .base_url(auth.base_url)
            .markdown_filter(config.markdown_filter);
        if let Some(route_tag) = auth.route_tag {
            builder = builder.route_tag(route_tag);
        }
        let client = Arc::new(
            WeixinClient::builder(builder.build()?)
                .on_message(handler)
                .build()?,
        );
        let _ = client_ref.set(client.clone());
        let initial_sync_buf = read_optional_text(&state_dir.join(WEIXIN_SYNC_BUF_FILE))?;
        let _ = read_optional_text(&state_dir.join(WEIXIN_CONTEXT_TOKENS_FILE));
        Ok(Self {
            client,
            initial_sync_buf,
        })
    }

    async fn run(self: Arc<Self>) -> Result<()> {
        self.client.start(self.initial_sync_buf.clone()).await?;
        Ok(())
    }

    async fn deliver(&self, action: &DwoOutboundAction) -> Result<String> {
        match &action.body {
            DwoOutboundBody::Text { text } => {
                let result = self.client.send_text(&action.target, text, None).await?;
                Ok(result.message_id)
            }
            DwoOutboundBody::Media { path, .. } => {
                let result = self.client.send_media(&action.target, path, None).await?;
                Ok(result.message_id)
            }
            DwoOutboundBody::Card { .. } => bail!("Weixin gateway cannot send card actions"),
        }
    }
}

#[async_trait::async_trait]
impl MessageHandler for WeixinGatewayHandler {
    async fn on_message(&self, ctx: &MessageContext) -> weixin_agent::Result<()> {
        if ctx.to != self.auth.ilink_bot_id || ctx.from != self.auth.bound_user_id {
            return Ok(());
        }

        match confirmation_command(ctx.body.as_deref()) {
            Some(DwoChannelCommand::Approve { .. }) | Some(DwoChannelCommand::Deny { .. }) => {
                let result = self.handle_confirmation_command(ctx).await;
                if let Err(err) = result {
                    tracing::warn!(target: "weixin", error = %format!("{err:#}"), "failed to notify weixin confirmation command");
                    let _ = ctx.reply_text(&format!("处理确认指令失败：{err:#}")).await;
                }
                return Ok(());
            }
            Some(DwoChannelCommand::Usage(message)) => {
                let _ = ctx.reply_text(&message).await;
                return Ok(());
            }
            _ => {}
        }

        let _guard = self.message_lock.lock().await;
        let _ = ctx.send_typing().await;
        let result = self.handle_message(ctx).await;
        let _ = ctx.cancel_typing().await;
        if let Err(err) = result {
            tracing::warn!(target: "weixin", error = %format!("{err:#}"), "failed to handle weixin gateway message");
            let _ = ctx.reply_text(&format!("处理微信消息失败：{err:#}")).await;
        }
        Ok(())
    }

    async fn on_sync_buf_updated(&self, sync_buf: &str) -> weixin_agent::Result<()> {
        let sync_path = self.state_dir.join(WEIXIN_SYNC_BUF_FILE);
        if let Some(parent) = sync_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(sync_path, sync_buf)?;
        Ok(())
    }
}

impl WeixinGatewayHandler {
    async fn handle_confirmation_command(&self, ctx: &MessageContext) -> Result<()> {
        let event = self.build_event(ctx, Vec::new());
        worker_notify(
            &self.state,
            &self.profile,
            "_dwo/ingress/notify_event",
            json!({ "event": event }),
        )
        .await?;
        ctx.reply_text("已收到确认指令；若请求仍在等待，会继续处理。")
            .await?;
        Ok(())
    }

    async fn handle_message(&self, ctx: &MessageContext) -> Result<()> {
        let attachments = if self.media_input {
            match &ctx.media {
                Some(media) => vec![download_weixin_media(ctx, media, &self.state_dir).await?],
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        let event = self.build_event(ctx, attachments);
        let response = self
            .state
            .worker_pool
            .request_with_events(
                &self.profile,
                "_dwo/ingress/handle_event",
                json!({ "event": event }),
                &self.state.config.pool,
                |event| async move {
                    self.state
                        .event_bus
                        .broadcast_worker_event(&self.profile.id, &event, None)
                        .await;
                    match outbound_action_from_event(event) {
                        Some(Ok(action)) => {
                            if let Err(err) = self.deliver_context_action(ctx, action).await {
                                tracing::warn!(target: "weixin", error = %format!("{err:#}"), "failed to deliver weixin outbound event");
                            }
                        }
                        Some(Err(err)) => {
                            tracing::warn!(target: "weixin", error = %format!("{err:#}"), "failed to parse weixin outbound event");
                        }
                        None => {}
                    }
                    Ok(())
                },
            )
            .await?;
        let actions = parse_actions(response)?;
        for action in actions {
            self.deliver_context_action(ctx, action).await?;
        }
        Ok(())
    }

    fn build_event(
        &self,
        ctx: &MessageContext,
        attachments: Vec<DwoIngressAttachment>,
    ) -> DwoIngressEvent {
        DwoIngressEvent {
            channel: DwoIngressChannel::Weixin,
            source: DwoIngressSource {
                id: self.auth.bound_user_id.clone(),
                name: None,
            },
            conversation: DwoIngressConversation {
                id: self.auth.bound_user_id.clone(),
                kind: Some("user".to_string()),
                reply_to: Some(ctx.from.clone()),
                holder: Some(format!("weixin:user:{}", self.auth.bound_user_id)),
                state_key: Some("default".to_string()),
            },
            text: ctx.body.clone(),
            attachments,
            raw: json!({
                "message_id": ctx.message_id,
                "server_message_id": ctx.server_message_id,
                "has_media": ctx.media.is_some(),
            }),
        }
    }

    async fn deliver_context_action(
        &self,
        ctx: &MessageContext,
        action: DwoOutboundAction,
    ) -> Result<()> {
        match action.body {
            DwoOutboundBody::Text { text } => {
                if action.target == ctx.from {
                    ctx.reply_text(&text).await?;
                } else {
                    let client = self
                        .client
                        .get()
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("Weixin client is not initialized"))?;
                    client.send_text(&action.target, &text, None).await?;
                }
            }
            DwoOutboundBody::Media { path, .. } => {
                let client = self
                    .client
                    .get()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Weixin client is not initialized"))?;
                let context_token = (action.target == ctx.from)
                    .then(|| ctx.context_token.as_deref())
                    .flatten();
                client
                    .send_media(&action.target, &path, context_token)
                    .await?;
            }
            DwoOutboundBody::Card { .. } => {}
        }
        Ok(())
    }
}

async fn download_weixin_media(
    ctx: &MessageContext,
    media: &MediaInfo,
    state_dir: &Path,
) -> Result<DwoIngressAttachment> {
    let attachments_dir = state_dir
        .join("inbox")
        .join("weixin")
        .join(sanitize_filename_or(&weixin_message_key(ctx), "unknown"));
    tokio::fs::create_dir_all(&attachments_dir).await?;
    let filename = media
        .file_name
        .as_deref()
        .map(sanitize_filename)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default_weixin_media_filename(media.media_type));
    let dest = attachments_dir.join(filename);
    let path = ctx.download_media(media, &dest).await?;
    Ok(DwoIngressAttachment {
        name: path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string),
        mime_type: Some(mime_type_for_weixin_media(&path, media.media_type).to_string()),
        kind: Some(weixin_media_kind(media.media_type).to_string()),
        path,
    })
}

fn read_weixin_auth(path: &Path) -> Result<WeixinAuth> {
    if !path.is_file() {
        bail!(
            "Weixin auth file not found: {}. Run `dwo-agent channel login weixin` first.",
            path.display()
        );
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let auth: WeixinAuth =
        serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    if auth.bot_token.trim().is_empty()
        || auth.base_url.trim().is_empty()
        || auth.ilink_bot_id.trim().is_empty()
        || auth.bound_user_id.trim().is_empty()
    {
        bail!("invalid Weixin auth in {}", path.display());
    }
    Ok(auth)
}

fn weixin_message_key(ctx: &MessageContext) -> String {
    ctx.server_message_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| ctx.message_id.clone())
}

fn weixin_media_kind(media_type: MediaType) -> &'static str {
    match media_type {
        MediaType::Image => "image",
        MediaType::Video => "video",
        MediaType::Voice => "voice",
        MediaType::File => "file",
    }
}

fn default_weixin_media_filename(media_type: MediaType) -> String {
    match media_type {
        MediaType::Image => "image.jpg",
        MediaType::Video => "video.mp4",
        MediaType::Voice => "voice.dat",
        MediaType::File => "file.bin",
    }
    .to_string()
}

fn mime_type_for_weixin_media(path: &Path, media_type: MediaType) -> &'static str {
    if let Some(mime_type) = image_mime_type_for_path(path) {
        return mime_type;
    }
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("txt") => "text/plain",
        Some("md") | Some("markdown") => "text/markdown",
        Some("json") => "application/json",
        Some("yaml") | Some("yml") => "application/yaml",
        Some("pdf") => "application/pdf",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("ogg") => "audio/ogg",
        Some("m4a") => "audio/mp4",
        Some("mp4") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        _ => match media_type {
            MediaType::Voice => "audio/mpeg",
            MediaType::Video => "video/mp4",
            _ => "application/octet-stream",
        },
    }
}

fn read_optional_text(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

fn parse_actions(response: Value) -> Result<Vec<DwoOutboundAction>> {
    response
        .get("actions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(serde_json::from_value::<DwoOutboundAction>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn outbound_action_from_event(event: Value) -> Option<Result<DwoOutboundAction>> {
    if event.get("method").and_then(Value::as_str) != Some("_dwo/outbound/action") {
        return None;
    }
    let params = event.get("params").cloned().unwrap_or(Value::Null);
    Some(
        serde_json::from_value::<DwoOutboundActionNotification>(params)
            .map(|notification| notification.action)
            .map_err(Into::into),
    )
}

fn confirmation_command(text: Option<&str>) -> Option<DwoChannelCommand> {
    let text = text?.trim();
    if !text.starts_with("/approve") && !text.starts_with("/deny") {
        return None;
    }
    dwo::parse_channel_command(text)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FeishuAuth {
    app_id: String,
    app_secret: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeishuChatKind {
    Direct,
    Group,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeishuMediaKind {
    Image,
    File,
}

#[derive(Debug, Clone)]
struct FeishuInboundMedia {
    kind: FeishuMediaKind,
    file_key: String,
    file_name: Option<String>,
}

struct FeishuResourceDownload {
    bytes: Vec<u8>,
    mime_type: String,
}

struct FeishuGateway {
    state: Arc<SupervisorState>,
    profile: ResolvedProfile,
    channel: Arc<clawrs_feishu::FeishuChannelService>,
    rest: Arc<FeishuRestClient>,
    config: FeishuChannelConfig,
    state_dir: PathBuf,
    message_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

struct CachedTenantToken {
    value: String,
    expires_at: Instant,
}

struct FeishuRestClient {
    app_id: String,
    app_secret: String,
    base_url: String,
    http: reqwest::Client,
    token: Mutex<Option<CachedTenantToken>>,
}

impl FeishuGateway {
    async fn new(
        state: Arc<SupervisorState>,
        profile: ResolvedProfile,
        agent_structure_dir: &Path,
        config: &FeishuChannelConfig,
    ) -> Result<Self> {
        let secret_dir = channel_secret_dir(agent_structure_dir).join(FEISHU_SECRET_SUBDIR);
        let auth = read_feishu_auth(&secret_dir.join(FEISHU_AUTH_FILE))?;
        let domain = to_claw_domain(config.domain);
        let base_url = domain.base_url().to_string();
        let claw_config = ClawFeishuConfig {
            app_id: auth.app_id.clone(),
            app_secret: auth.app_secret.clone(),
            domain,
            connection_mode: ClawFeishuConnectionMode::WebSocket,
            allowed_users: vec!["*".to_string()],
            dm_policy: None,
            group_policy: None,
            allow_from: Some(vec!["*".to_string()]),
            group_allow_from: vec![],
            group_require_mention: config.group_require_mention,
            encrypt_key: None,
            verification_token: None,
            webhook_port: None,
        };
        Ok(Self {
            state,
            profile,
            channel: Arc::new(create_channel(claw_config)),
            rest: Arc::new(FeishuRestClient::new(auth, base_url)),
            config: config.clone(),
            state_dir: agent_structure_dir
                .join("runtime")
                .join("channel_state")
                .join(FEISHU_STATE_SUBDIR),
            message_locks: Mutex::new(HashMap::new()),
        })
    }

    async fn run(self: Arc<Self>) -> Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ChannelMessage>(256);
        let channel = self.channel.clone();
        let mut listen_task = tokio::spawn(async move { channel.listen(tx).await });
        loop {
            tokio::select! {
                result = &mut listen_task => {
                    return result?;
                }
                maybe_msg = rx.recv() => {
                    let Some(msg) = maybe_msg else {
                        return Ok(());
                    };
                    if let Err(err) = self.handle_message(msg).await {
                        tracing::warn!(target: "feishu", error = %format!("{err:#}"), "failed to handle feishu gateway message");
                    }
                }
            }
        }
    }

    async fn handle_message(&self, msg: ChannelMessage) -> Result<()> {
        let kind = feishu_chat_kind(&msg);
        let peer = feishu_peer_id(&msg, kind).to_string();
        let media = parse_feishu_media(&msg.content);
        let text = message_text(&msg.content, media.is_some());
        match confirmation_command(text.as_deref()) {
            Some(DwoChannelCommand::Approve { .. }) | Some(DwoChannelCommand::Deny { .. }) => {
                self.handle_confirmation_command(&msg, kind, &peer, text, media.is_some())
                    .await?;
                return Ok(());
            }
            Some(DwoChannelCommand::Usage(message)) => {
                self.channel.send(&message, &peer).await?;
                return Ok(());
            }
            _ => {}
        }

        let session_key = format!("{}:{peer}", feishu_chat_kind_name(kind));
        let lock = self.session_lock(&session_key).await;
        let _guard = lock.lock().await;
        let attachments = if self.config.media_input {
            match media.as_ref() {
                Some(media) => vec![self.download_message_media(&msg, &media).await?],
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        let event = self.build_event(&msg, kind, &peer, text, attachments, media.is_some());
        let response = self
            .state
            .worker_pool
            .request_with_events(
                &self.profile,
                "_dwo/ingress/handle_event",
                json!({ "event": event }),
                &self.state.config.pool,
                |event| async move {
                    self.state
                        .event_bus
                        .broadcast_worker_event(&self.profile.id, &event, None)
                        .await;
                    match outbound_action_from_event(event) {
                        Some(Ok(action)) => {
                            if let Err(err) = self.deliver(&action).await {
                                tracing::warn!(target: "feishu", error = %format!("{err:#}"), "failed to deliver feishu outbound event");
                            }
                        }
                        Some(Err(err)) => {
                            tracing::warn!(target: "feishu", error = %format!("{err:#}"), "failed to parse feishu outbound event");
                        }
                        None => {}
                    }
                    Ok(())
                },
            )
            .await?;
        let actions = parse_actions(response)?;
        for action in actions {
            if let Err(err) = self.deliver(&action).await {
                tracing::warn!(target: "feishu", error = %format!("{err:#}"), "failed to deliver feishu action");
            }
        }
        Ok(())
    }

    async fn handle_confirmation_command(
        &self,
        msg: &ChannelMessage,
        kind: FeishuChatKind,
        peer: &str,
        text: Option<String>,
        has_media: bool,
    ) -> Result<()> {
        let event = self.build_event(msg, kind, peer, text, Vec::new(), has_media);
        worker_notify(
            &self.state,
            &self.profile,
            "_dwo/ingress/notify_event",
            json!({ "event": event }),
        )
        .await?;
        self.channel
            .send("已收到确认指令；若请求仍在等待，会继续处理。", peer)
            .await?;
        Ok(())
    }

    fn build_event(
        &self,
        msg: &ChannelMessage,
        kind: FeishuChatKind,
        peer: &str,
        text: Option<String>,
        attachments: Vec<DwoIngressAttachment>,
        has_media: bool,
    ) -> DwoIngressEvent {
        let kind_name = feishu_chat_kind_name(kind);
        DwoIngressEvent {
            channel: DwoIngressChannel::Feishu,
            source: DwoIngressSource {
                id: msg.sender.clone(),
                name: Some(msg.sender.clone()),
            },
            conversation: DwoIngressConversation {
                id: peer.to_string(),
                kind: Some(if kind == FeishuChatKind::Group {
                    "group".to_string()
                } else {
                    "direct".to_string()
                }),
                reply_to: Some(peer.to_string()),
                holder: Some(format!("feishu:{kind_name}:{peer}")),
                state_key: Some(peer.to_string()),
            },
            text,
            attachments,
            raw: json!({
                "message_id": msg.id,
                "chat_type": msg.chat_type,
                "channel": msg.channel,
                "has_media": has_media,
            }),
        }
    }

    async fn session_lock(&self, session_key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.message_locks.lock().await;
        locks
            .entry(session_key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn download_message_media(
        &self,
        msg: &ChannelMessage,
        media: &FeishuInboundMedia,
    ) -> Result<DwoIngressAttachment> {
        let attachments_dir = self
            .state_dir
            .join("inbox")
            .join("feishu")
            .join(sanitize_filename_or(&msg.id, "unknown"));
        tokio::fs::create_dir_all(&attachments_dir).await?;
        let resource_type = match media.kind {
            FeishuMediaKind::Image => "image",
            FeishuMediaKind::File => "file",
        };
        let downloaded = self
            .rest
            .download_message_resource(&msg.id, &media.file_key, resource_type)
            .await?;
        let filename = media
            .file_name
            .as_deref()
            .map(sanitize_filename)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| default_feishu_media_filename(media.kind, &downloaded.mime_type));
        let path = attachments_dir.join(filename);
        tokio::fs::write(&path, &downloaded.bytes)
            .await
            .with_context(|| format!("write Feishu media {}", path.display()))?;
        Ok(DwoIngressAttachment {
            name: path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string),
            mime_type: Some(downloaded.mime_type),
            kind: Some(resource_type.to_string()),
            path,
        })
    }

    async fn deliver(&self, action: &DwoOutboundAction) -> Result<String> {
        match &action.body {
            DwoOutboundBody::Text { text } => {
                self.channel.send(text, &action.target).await?;
                Ok("feishu:message:sent".to_string())
            }
            DwoOutboundBody::Media {
                path,
                kind,
                file_type,
            } => {
                let send_as_image = match kind.as_deref() {
                    Some("image") => true,
                    Some("file") => false,
                    _ => is_feishu_image_path(path),
                };
                if send_as_image {
                    let image_key = self.rest.upload_image(path).await?;
                    self.rest
                        .send_message(&action.target, "image", json!({ "image_key": image_key }))
                        .await
                } else {
                    let file_key = self.rest.upload_file(path, file_type.as_deref()).await?;
                    self.rest
                        .send_message(&action.target, "file", json!({ "file_key": file_key }))
                        .await
                }
            }
            DwoOutboundBody::Card { card } => {
                self.rest
                    .send_message(&action.target, "interactive", card.clone())
                    .await
            }
        }
    }
}

impl FeishuRestClient {
    fn new(auth: FeishuAuth, base_url: String) -> Self {
        Self {
            app_id: auth.app_id,
            app_secret: auth.app_secret,
            base_url,
            http: reqwest::Client::new(),
            token: Mutex::new(None),
        }
    }

    async fn download_message_resource(
        &self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
    ) -> Result<FeishuResourceDownload> {
        let token = self.tenant_access_token().await?;
        let url = format!(
            "{}/open-apis/im/v1/messages/{}/resources/{}",
            self.base_url, message_id, file_key
        );
        let response = self
            .http
            .get(url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .query(&[("type", resource_type)])
            .send()
            .await?;
        let status = response.status();
        let mime_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or(value).trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| default_feishu_mime_type(resource_type).to_string());
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("Feishu media download failed: HTTP {status}: {body}");
        }
        Ok(FeishuResourceDownload {
            bytes: response.bytes().await?.to_vec(),
            mime_type,
        })
    }

    async fn upload_image(&self, path: &Path) -> Result<String> {
        let token = self.tenant_access_token().await?;
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("read Feishu image {}", path.display()))?;
        if bytes.is_empty() {
            bail!(
                "Feishu image upload requires a non-empty file: {}",
                path.display()
            );
        }
        let file_name = file_name_for_upload(path);
        let mut part = Part::bytes(bytes).file_name(file_name);
        if let Some(mime_type) = image_mime_type_for_path(path) {
            part = part.mime_str(mime_type)?;
        }
        let response = self
            .http
            .post(format!("{}/open-apis/im/v1/images", self.base_url))
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .multipart(
                Form::new()
                    .text("image_type", "message")
                    .part("image", part),
            )
            .send()
            .await?;
        let body = parse_feishu_json_response(response).await?;
        body.get("data")
            .and_then(|data| data.get("image_key"))
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("Feishu image upload response missing image_key"))
    }

    async fn upload_file(&self, path: &Path, file_type: Option<&str>) -> Result<String> {
        let token = self.tenant_access_token().await?;
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("read Feishu file {}", path.display()))?;
        if bytes.is_empty() {
            bail!(
                "Feishu file upload requires a non-empty file: {}",
                path.display()
            );
        }
        let file_name = file_name_for_upload(path);
        let file_type = file_type
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| feishu_file_type_for_path(path).to_string());
        let form = Form::new()
            .text("file_type", file_type)
            .text("file_name", file_name.clone())
            .part("file", Part::bytes(bytes).file_name(file_name));
        let response = self
            .http
            .post(format!("{}/open-apis/im/v1/files", self.base_url))
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .multipart(form)
            .send()
            .await?;
        let body = parse_feishu_json_response(response).await?;
        body.get("data")
            .and_then(|data| data.get("file_key"))
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("Feishu file upload response missing file_key"))
    }

    async fn send_message(
        &self,
        receive_id: &str,
        msg_type: &str,
        content: Value,
    ) -> Result<String> {
        let token = self.tenant_access_token().await?;
        let content = serde_json::to_string(&content)?;
        let response = self
            .http
            .post(format!("{}/open-apis/im/v1/messages", self.base_url))
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .query(&[("receive_id_type", infer_receive_id_type(receive_id))])
            .json(&json!({
                "receive_id": receive_id,
                "msg_type": msg_type,
                "content": content,
            }))
            .send()
            .await?;
        let body = parse_feishu_json_response(response).await?;
        body.get("data")
            .and_then(|data| data.get("message_id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("Feishu send message response missing message_id"))
    }

    async fn tenant_access_token(&self) -> Result<String> {
        {
            let guard = self.token.lock().await;
            if let Some(token) = guard.as_ref()
                && Instant::now() < token.expires_at
            {
                return Ok(token.value.clone());
            }
        }

        let response = self
            .http
            .post(format!(
                "{}/open-apis/auth/v3/tenant_access_token/internal",
                self.base_url
            ))
            .json(&json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret,
            }))
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await?;
        if !status.is_success() || body.get("code").and_then(Value::as_i64) != Some(0) {
            bail!("Feishu token request failed: HTTP {status}: {body}");
        }
        let value = body
            .get("tenant_access_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Feishu token response missing tenant_access_token"))?
            .to_string();
        let expire = body.get("expire").and_then(Value::as_u64).unwrap_or(7200);
        let expires_at = Instant::now() + Duration::from_secs(expire.saturating_sub(60).max(60));
        *self.token.lock().await = Some(CachedTenantToken {
            value: value.clone(),
            expires_at,
        });
        Ok(value)
    }
}

fn read_feishu_auth(path: &Path) -> Result<FeishuAuth> {
    if !path.is_file() {
        bail!(
            "Feishu auth file not found: {}. Run `dwo-agent channel login feishu` first.",
            path.display()
        );
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let auth: FeishuAuth =
        serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    if auth.app_id.trim().is_empty() || auth.app_secret.trim().is_empty() {
        bail!("invalid Feishu auth in {}", path.display());
    }
    Ok(auth)
}

fn to_claw_domain(domain: FeishuChannelDomain) -> ClawFeishuDomain {
    match domain {
        FeishuChannelDomain::Feishu => ClawFeishuDomain::Feishu,
        FeishuChannelDomain::Lark => ClawFeishuDomain::Lark,
    }
}

fn feishu_chat_kind(msg: &ChannelMessage) -> FeishuChatKind {
    if msg.chat_type.as_deref() == Some("group") {
        FeishuChatKind::Group
    } else {
        FeishuChatKind::Direct
    }
}

fn feishu_chat_kind_name(kind: FeishuChatKind) -> &'static str {
    match kind {
        FeishuChatKind::Direct => "dm",
        FeishuChatKind::Group => "group",
    }
}

fn feishu_peer_id(msg: &ChannelMessage, kind: FeishuChatKind) -> &str {
    match kind {
        FeishuChatKind::Direct => &msg.sender,
        FeishuChatKind::Group => &msg.channel,
    }
}

fn parse_feishu_media(content: &str) -> Option<FeishuInboundMedia> {
    let parsed = serde_json::from_str::<Value>(content).ok()?;
    if let Some(image_key) = parsed
        .get("image_key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
    {
        return Some(FeishuInboundMedia {
            kind: FeishuMediaKind::Image,
            file_key: image_key.to_string(),
            file_name: parsed
                .get("file_name")
                .or_else(|| parsed.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    if let Some(file_key) = parsed
        .get("file_key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
    {
        return Some(FeishuInboundMedia {
            kind: FeishuMediaKind::File,
            file_key: file_key.to_string(),
            file_name: parsed
                .get("file_name")
                .or_else(|| parsed.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    None
}

fn message_text(content: &str, has_media: bool) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    if has_media {
        return serde_json::from_str::<Value>(trimmed)
            .ok()
            .and_then(|value| {
                value
                    .get("text")
                    .or_else(|| value.get("caption"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
    }
    Some(trimmed.to_string())
}

async fn parse_feishu_json_response(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    let text = response.text().await?;
    let body: Value = serde_json::from_str(&text)
        .with_context(|| format!("parse Feishu response JSON: {text}"))?;
    if !status.is_success() || body.get("code").and_then(Value::as_i64) != Some(0) {
        bail!("Feishu API request failed: HTTP {status}: {body}");
    }
    Ok(body)
}

fn infer_receive_id_type(id: &str) -> &'static str {
    if id.starts_with("oc_") {
        "chat_id"
    } else if id.starts_with("on_") {
        "union_id"
    } else {
        "open_id"
    }
}

fn file_name_for_upload(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_filename)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "file.bin".to_string())
}

fn is_feishu_image_path(path: &Path) -> bool {
    image_mime_type_for_path(path).is_some()
}

fn feishu_file_type_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("opus") => "opus",
        Some("mp4") => "mp4",
        Some("pdf") => "pdf",
        Some("doc") | Some("docx") => "doc",
        Some("xls") | Some("xlsx") => "xls",
        Some("ppt") | Some("pptx") => "ppt",
        _ => "stream",
    }
}

fn default_feishu_media_filename(kind: FeishuMediaKind, mime_type: &str) -> String {
    let stem = match kind {
        FeishuMediaKind::Image => "image",
        FeishuMediaKind::File => "file",
    };
    let ext = extension_for_mime_type(mime_type).unwrap_or("bin");
    format!("{stem}.{ext}")
}

fn default_feishu_mime_type(resource_type: &str) -> &'static str {
    match resource_type {
        "image" => "image/jpeg",
        _ => "application/octet-stream",
    }
}
