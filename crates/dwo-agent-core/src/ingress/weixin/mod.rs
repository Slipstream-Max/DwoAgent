//! Weixin assistant channel.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::Mutex;
use weixin_agent::{
    LoginStatus, MediaInfo, MediaType, MessageContext, MessageHandler, StandaloneQrLogin,
    WeixinClient, WeixinConfig,
};

use super::channel_control::{ChannelControl, PendingConfirmationRegistry, SessionLeaseRegistry};
use super::channel_input::{
    append_channel_context, file_uri_from_path, image_mime_type_for_path,
    image_url_block_from_path as image_url_block_from_file, resolve_config_path,
    resource_link_block, sanitize_filename,
};
use super::config::WeixinChannelConfig;
use super::response::{ChannelCollectedUpdates, ChannelResponseDetail, ChannelUpdateCollector};
use crate::agent::constants::PERMISSION_REJECT_ONCE;
use crate::agent::service::AgentService;
use crate::agent::session_agent::SessionAgent;
use crate::automation::{AutomationNotificationSink, AutomationNotifyConfig};
use crate::config::loader::{channel_secret_dir, resolve_agent_structure_dir};
use crate::protocol::acp::mapper;
use crate::tools::PermissionRequester;
use crate::tools::weixin_tool_schemas;
use crate::tools::{WeixinReplyMediaResult, WeixinToolBridge, WeixinToolExecutor};
use crate::utils::files::{read_json_utf8, read_utf8_text, write_json_utf8};
use agent_client_protocol::schema::{ContentBlock, TextContent};

const WEIXIN_SECRET_SUBDIR: &str = "weixin";
const WEIXIN_STATE_SUBDIR: &str = "weixin";
const WEIXIN_AUTH_FILE: &str = "auth.yaml";
const WEIXIN_CONTEXT_TOKENS_FILE: &str = "context_tokens.json";
const WEIXIN_SYNC_BUF_FILE: &str = "sync_buf.txt";

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

pub struct WeixinChannel {
    client: Arc<WeixinClient>,
    notify_to: String,
    initial_sync_buf: Option<String>,
    context_tokens_path: PathBuf,
}

#[derive(Clone)]
struct WeixinHandler {
    channel_control: ChannelControl,
    confirmations: Arc<PendingConfirmationRegistry>,
    auth: WeixinAuth,
    workspace_dir: PathBuf,
    sync_buf_path: PathBuf,
    context_tokens_path: PathBuf,
    media_input: bool,
    media_output: bool,
    response_detail: ChannelResponseDetail,
    client: Arc<OnceLock<Arc<WeixinClient>>>,
    message_lock: Arc<Mutex<()>>,
}

struct WeixinReplyMediaBridge {
    client: Arc<WeixinClient>,
    to: String,
    context_token: Option<String>,
}

struct WeixinAutomationNotifier {
    client: Arc<WeixinClient>,
    to: String,
}

pub fn run_weixin_login_sync(agent_folder: PathBuf) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_weixin_login(&agent_folder))
}

pub async fn run_weixin_login(agent_folder: &Path) -> Result<()> {
    let agent_structure_dir = resolve_agent_structure_dir(agent_folder)?;
    let secret_dir = channel_secret_dir(&agent_structure_dir).join(WEIXIN_SECRET_SUBDIR);
    std::fs::create_dir_all(&secret_dir)
        .with_context(|| format!("create {}", secret_dir.display()))?;

    let config = WeixinConfig::builder().token("").build()?;
    let qr = StandaloneQrLogin::new(&config);
    let local_tokens = read_existing_token(&secret_dir.join(WEIXIN_AUTH_FILE))
        .into_iter()
        .collect::<Vec<_>>();
    let session = qr.start(None, &local_tokens).await?;

    println!("Scan this Weixin QR code URL:");
    println!("{}", session.qrcode_img_content);

    let mut verify_code: Option<String> = None;
    loop {
        match qr.poll_status(&session, verify_code.as_deref()).await? {
            LoginStatus::Confirmed {
                bot_token,
                ilink_bot_id,
                base_url,
                ilink_user_id,
            } => {
                let auth = WeixinAuth {
                    bot_token,
                    base_url,
                    ilink_bot_id,
                    bound_user_id: ilink_user_id,
                    route_tag: None,
                };
                validate_auth(&auth)?;
                write_auth(&secret_dir.join(WEIXIN_AUTH_FILE), &auth)?;
                println!(
                    "Weixin credentials saved to {}",
                    secret_dir.join(WEIXIN_AUTH_FILE).display()
                );
                return Ok(());
            }
            LoginStatus::Expired => bail!("Weixin QR code expired; run login again."),
            LoginStatus::NeedVerifyCode => {
                print!("Enter the verification code shown on your phone: ");
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                verify_code = Some(input.trim().to_string());
            }
            LoginStatus::VerifyCodeBlocked => {
                bail!("Too many wrong verification codes; run login again.")
            }
            LoginStatus::BindedRedirect => {
                bail!("Weixin reported this bot is already bound; no credentials were issued.")
            }
            LoginStatus::ScannedButRedirect { redirect_host } => {
                println!("QR scanned; waiting on redirected host: {redirect_host}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            LoginStatus::Scanned => {
                println!("QR scanned; confirm login on your phone.");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            LoginStatus::Wait => {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            _ => {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

impl WeixinChannel {
    pub async fn new(
        agent: Arc<AgentService>,
        leases: Arc<SessionLeaseRegistry>,
        confirmations: Arc<PendingConfirmationRegistry>,
        agent_structure_dir: &Path,
        config: &WeixinChannelConfig,
    ) -> Result<Self> {
        let secret_dir = channel_secret_dir(agent_structure_dir).join(WEIXIN_SECRET_SUBDIR);
        let state_dir = agent.channel_state_dir().join(WEIXIN_STATE_SUBDIR);
        let auth_path = secret_dir.join(WEIXIN_AUTH_FILE);
        let context_tokens_path = state_dir.join(WEIXIN_CONTEXT_TOKENS_FILE);
        let sync_buf_path = state_dir.join(WEIXIN_SYNC_BUF_FILE);
        let auth = read_auth(&auth_path)?;
        let workspace_dir = resolve_config_path(agent_structure_dir, &config.workspace_dir);

        let client_ref = Arc::new(OnceLock::new());
        let channel_control = ChannelControl::new(
            agent.clone(),
            leases,
            confirmations.clone(),
            format!("weixin:user:{}", auth.bound_user_id),
            &state_dir,
            workspace_dir.to_string_lossy().to_string(),
            config.default_session_id.as_deref(),
            config.override_model.as_deref(),
            config.override_reasoning_mode,
        );
        let handler = WeixinHandler {
            channel_control,
            confirmations,
            auth: auth.clone(),
            workspace_dir: workspace_dir.clone(),
            sync_buf_path: sync_buf_path.clone(),
            context_tokens_path: context_tokens_path.clone(),
            media_input: config.media_input,
            media_output: config.media_output,
            response_detail: config.response_detail,
            client: client_ref.clone(),
            message_lock: Arc::new(Mutex::new(())),
        };

        let mut builder = WeixinConfig::builder()
            .token(auth.bot_token.clone())
            .base_url(auth.base_url.clone())
            .markdown_filter(config.markdown_filter);
        if let Some(route_tag) = auth.route_tag {
            builder = builder.route_tag(route_tag);
        }
        let sdk_config = builder.build()?;
        let client = Arc::new(
            WeixinClient::builder(sdk_config)
                .on_message(handler)
                .build()?,
        );
        let _ = client_ref.set(client.clone());
        import_context_tokens(&client, &context_tokens_path)?;

        Ok(Self {
            client,
            notify_to: auth.bound_user_id,
            initial_sync_buf: read_sync_buf(&sync_buf_path)?,
            context_tokens_path,
        })
    }

    pub fn client(&self) -> Arc<WeixinClient> {
        self.client.clone()
    }

    pub fn automation_notifier(&self) -> Arc<dyn AutomationNotificationSink> {
        Arc::new(WeixinAutomationNotifier {
            client: self.client.clone(),
            to: self.notify_to.clone(),
        })
    }

    pub async fn run(self) -> Result<()> {
        self.client.start(self.initial_sync_buf).await?;
        export_context_tokens(&self.client, Some(&self.context_tokens_path))
    }
}

#[async_trait::async_trait]
impl MessageHandler for WeixinHandler {
    async fn on_message(&self, ctx: &MessageContext) -> weixin_agent::Result<()> {
        if ctx.to != self.auth.ilink_bot_id || ctx.from != self.auth.bound_user_id {
            return Ok(());
        }

        let _guard = self.message_lock.lock().await;
        let _ = ctx.send_typing().await;
        let result = self.handle_message(ctx).await;
        let _ = ctx.cancel_typing().await;

        if let Err(err) = result {
            tracing::warn!(target: "weixin", error = %err, "failed to handle weixin message");
            let _ = ctx.reply_text(&format!("处理微信消息失败：{err:#}")).await;
        }
        Ok(())
    }

    async fn on_sync_buf_updated(&self, sync_buf: &str) -> weixin_agent::Result<()> {
        if let Some(parent) = self.sync_buf_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.sync_buf_path, sync_buf)?;
        Ok(())
    }

    async fn on_shutdown(&self) -> weixin_agent::Result<()> {
        if let Some(client) = self.client.get() {
            if let Err(err) = export_context_tokens(client, Some(&self.context_tokens_path)) {
                tracing::warn!(target: "weixin", error = %err, "failed to export context tokens");
            }
        }
        Ok(())
    }
}

impl WeixinHandler {
    async fn handle_message(&self, ctx: &MessageContext) -> Result<()> {
        if let Some(text) = ctx.body.as_deref()
            && let Some(reply) = self.channel_control.handle_command(text).await?
        {
            ctx.reply_text(&reply).await?;
            return Ok(());
        }
        let agent = self.channel_control.active_session().await?;
        let Some((user_input, user_blocks)) =
            self.build_user_input(ctx, agent.session_dir()).await?
        else {
            return Ok(());
        };

        let collected = self
            .run_prompt_for_message(ctx, agent, user_input, user_blocks)
            .await?;
        self.reply_collected(ctx, &collected).await?;
        if let Some(client) = self.client.get() {
            export_context_tokens(client, Some(&self.context_tokens_path))?;
        }
        Ok(())
    }

    async fn run_prompt_for_message(
        &self,
        ctx: &MessageContext,
        agent: Arc<SessionAgent>,
        user_input: Value,
        user_blocks: Vec<Value>,
    ) -> Result<ChannelCollectedUpdates> {
        let tool_manager = agent.tool_manager().await;
        if self.media_output {
            let client = self
                .client
                .get()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Weixin client is not initialized."))?;
            let reply_media_bridge = Arc::new(WeixinReplyMediaBridge {
                client,
                to: ctx.from.clone(),
                context_token: ctx.context_token.clone(),
            });
            let reply_media = Arc::new(WeixinToolExecutor::new(
                reply_media_bridge,
                vec![
                    self.workspace_dir.clone(),
                    agent.session_dir().to_path_buf(),
                ],
            ));
            tool_manager
                .set_channel_tool_executor(Some(reply_media))
                .await;
        }

        let update_collector = ChannelUpdateCollector::new(self.response_detail);
        let emit_update = update_collector.emitter();
        let client = self
            .client
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Weixin client is not initialized."))?;
        let request_permission = confirming_permission_requester(
            self.confirmations.clone(),
            client,
            ctx.from.clone(),
            ctx.context_token.clone(),
            format!("weixin:user:{}", self.auth.bound_user_id),
        );
        let run_result = self
            .channel_control
            .run_prompt(
                agent.session_id(),
                user_input,
                user_blocks,
                emit_update,
                request_permission,
                if self.media_output {
                    weixin_tool_schemas()
                } else {
                    Vec::new()
                },
            )
            .await;

        if self.media_output {
            tool_manager.set_channel_tool_executor(None).await;
        }
        run_result?;

        let collected = update_collector.finish().await;
        Ok(collected)
    }

    async fn reply_collected(
        &self,
        ctx: &MessageContext,
        collected: &ChannelCollectedUpdates,
    ) -> Result<()> {
        if let Some(detail) = collected.detail_text.as_deref() {
            ctx.reply_text(detail).await?;
        }
        if !collected.response_text.is_empty() {
            ctx.reply_text(&collected.response_text).await?;
        }
        Ok(())
    }

    async fn build_user_input(
        &self,
        ctx: &MessageContext,
        active_session_dir: &Path,
    ) -> Result<Option<(Value, Vec<Value>)>> {
        let mut blocks: Vec<ContentBlock> = Vec::new();
        if let Some(text) = ctx.body.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            blocks.push(ContentBlock::Text(TextContent::new(text)));
        }

        if let Some(media) = &ctx.media {
            if !self.media_input {
                if blocks.is_empty() {
                    ctx.reply_text("当前微信通道未开启媒体输入。").await?;
                    return Ok(None);
                }
            } else {
                let path = download_message_media(ctx, media, active_session_dir).await?;
                let uri = file_uri_from_path(&path);
                let name = path.file_name().and_then(|name| name.to_str());
                let mime_type = mime_type_for_media(&path, media.media_type);
                blocks.push(resource_link_block(&uri, name, Some(mime_type))?);
                if media.media_type == MediaType::Image
                    && let Some(image) = image_url_block_from_file(&path)?
                {
                    blocks.push(image);
                }
            }
        }

        if blocks.is_empty() {
            return Ok(None);
        }
        let instructions = if self.media_output {
            vec!["本轮如需发送本地文件或图片，请使用 weixin_reply_media 回复当前微信对话。"]
        } else {
            Vec::new()
        };
        append_channel_context(&mut blocks, "Weixin", &instructions);

        Ok(Some(mapper::normalize_prompt_blocks(&blocks)?))
    }
}

#[async_trait::async_trait]
impl WeixinToolBridge for WeixinReplyMediaBridge {
    async fn reply_media(&self, path: &Path) -> Result<WeixinReplyMediaResult> {
        let result = self
            .client
            .send_media(&self.to, path, self.context_token.as_deref())
            .await?;
        Ok(WeixinReplyMediaResult {
            message_id: result.message_id,
        })
    }
}

#[async_trait::async_trait]
impl AutomationNotificationSink for WeixinAutomationNotifier {
    async fn send(&self, _notify: &AutomationNotifyConfig, text: &str) -> Result<String> {
        let result = self.client.send_text(&self.to, text, None).await?;
        Ok(result.message_id)
    }
}

fn confirming_permission_requester(
    confirmations: Arc<PendingConfirmationRegistry>,
    client: Arc<WeixinClient>,
    to: String,
    context_token: Option<String>,
    holder: String,
) -> PermissionRequester {
    Arc::new(move |session_id: String, payload: Map<String, Value>| {
        let confirmations = confirmations.clone();
        let client = client.clone();
        let to = to.clone();
        let context_token = context_token.clone();
        let holder = holder.clone();
        Box::pin(async move {
            let (snapshot, rx) = confirmations.create(session_id, payload, holder)?;
            let message = ChannelControl::confirmation_message(&snapshot);
            client
                .send_text(&to, &message, context_token.as_deref())
                .await?;
            match rx.await {
                Ok(decision) => Ok(decision),
                Err(_) => Ok(PERMISSION_REJECT_ONCE.to_string()),
            }
        })
    })
}

fn read_auth(path: &Path) -> Result<WeixinAuth> {
    if !path.is_file() {
        bail!(
            "Weixin auth file not found: {}. Run `dwo-agent channel login weixin` first.",
            path.display()
        );
    }
    let text = read_utf8_text(path)?;
    let auth: WeixinAuth =
        serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    validate_auth(&auth)?;
    Ok(auth)
}

fn write_auth(path: &Path, auth: &WeixinAuth) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_yaml::to_string(auth)?;
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn validate_auth(auth: &WeixinAuth) -> Result<()> {
    if auth.bot_token.trim().is_empty() {
        bail!("bot_token is required in Weixin auth.");
    }
    if auth.base_url.trim().is_empty() {
        bail!("base_url is required in Weixin auth.");
    }
    if auth.ilink_bot_id.trim().is_empty() {
        bail!("ilink_bot_id is required in Weixin auth.");
    }
    if auth.bound_user_id.trim().is_empty() {
        bail!("bound_user_id is required in Weixin auth.");
    }
    Ok(())
}

fn read_existing_token(path: &Path) -> Option<String> {
    read_auth(path).ok().map(|auth| auth.bot_token)
}

fn import_context_tokens(client: &WeixinClient, path: &Path) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let map = read_json_utf8(path)?;
    let tokens: HashMap<String, String> = serde_json::from_value(Value::Object(map))?;
    client.context_tokens().import(tokens);
    Ok(())
}

fn export_context_tokens(client: &WeixinClient, path: Option<&Path>) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = serde_json::to_value(client.context_tokens().export_all())?;
    write_json_utf8(path, &payload)
}

fn read_sync_buf(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = read_utf8_text(path)?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

async fn download_message_media(
    ctx: &MessageContext,
    media: &MediaInfo,
    session_dir: &Path,
) -> Result<PathBuf> {
    let attachments_dir = session_dir
        .join("attachments")
        .join("inbox")
        .join("weixin")
        .join(sanitize_filename(&message_key(ctx)));
    tokio::fs::create_dir_all(&attachments_dir).await?;
    let filename = media
        .file_name
        .as_deref()
        .map(sanitize_filename)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_media_filename(media.media_type));
    let dest = attachments_dir.join(filename);
    let path = ctx.download_media(media, &dest).await?;
    Ok(path)
}

fn message_key(ctx: &MessageContext) -> String {
    ctx.server_message_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| ctx.message_id.clone())
}

fn mime_type_for_media(path: &Path, media_type: MediaType) -> &'static str {
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

fn default_media_filename(media_type: MediaType) -> String {
    match media_type {
        MediaType::Image => "image.jpg",
        MediaType::Video => "video.mp4",
        MediaType::Voice => "voice.dat",
        MediaType::File => "file.bin",
    }
    .to_string()
}
