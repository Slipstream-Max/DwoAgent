//! Feishu assistant channel.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine;
use clawrs_feishu::{
    Channel, ChannelMessage, FeishuConfig as ClawFeishuConfig,
    FeishuConnectionMode as ClawFeishuConnectionMode, FeishuDomain as ClawFeishuDomain,
    create_channel,
};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use super::channel_control::{ChannelControl, PendingConfirmationRegistry, SessionLeaseRegistry};
use super::config::{FeishuAccessPolicy, FeishuChannelConfig, FeishuChannelDomain};
use super::response::ChannelUpdateCollector;
use crate::agent::constants::PERMISSION_REJECT_ONCE;
use crate::agent::service::AgentService;
use crate::automation::{AutomationNotificationSink, AutomationNotifyConfig};
use crate::config::loader::{channel_secret_dir, resolve_agent_structure_dir};
use crate::protocol::acp::mapper;
use crate::tools::PermissionRequester;
use crate::tools::{
    FeishuReplyCardResult, FeishuReplyMediaKind, FeishuReplyMediaResult, FeishuToolBridge,
    FeishuToolExecutor, feishu_tool_schemas,
};
use crate::utils::files::read_utf8_text;
use agent_client_protocol::schema::{ContentBlock, ImageContent, ResourceLink, TextContent};

const FEISHU_SECRET_SUBDIR: &str = "feishu";
const FEISHU_AUTH_FILE: &str = "auth.yaml";
const FEISHU_STATE_SUBDIR: &str = "feishu";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FeishuAuth {
    app_id: String,
    app_secret: String,
}

pub struct FeishuChannel {
    agent: Arc<AgentService>,
    leases: Arc<SessionLeaseRegistry>,
    confirmations: Arc<PendingConfirmationRegistry>,
    channel: Arc<clawrs_feishu::FeishuChannelService>,
    rest: Arc<FeishuRestClient>,
    config: FeishuChannelConfig,
    workspace_dir: PathBuf,
    message_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
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

struct DownloadedFeishuMedia {
    path: PathBuf,
    mime_type: String,
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

struct FeishuReplyBridge {
    rest: Arc<FeishuRestClient>,
    to: String,
}

struct FeishuAutomationNotifier {
    rest: Arc<FeishuRestClient>,
}

pub fn run_feishu_login_sync(
    agent_folder: PathBuf,
    app_id: Option<String>,
    app_secret: Option<String>,
) -> Result<()> {
    run_feishu_login(&agent_folder, app_id, app_secret)
}

pub fn run_feishu_login(
    agent_folder: &Path,
    app_id: Option<String>,
    app_secret: Option<String>,
) -> Result<()> {
    let agent_structure_dir = resolve_agent_structure_dir(agent_folder)?;
    let secret_dir = channel_secret_dir(&agent_structure_dir).join(FEISHU_SECRET_SUBDIR);
    std::fs::create_dir_all(&secret_dir)
        .with_context(|| format!("create {}", secret_dir.display()))?;

    let auth = FeishuAuth {
        app_id: app_id
            .or_else(|| std::env::var("FEISHU_APP_ID").ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("Feishu app_id is required. Pass --app-id or set FEISHU_APP_ID.")
            })?,
        app_secret: app_secret
            .or_else(|| std::env::var("FEISHU_APP_SECRET").ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Feishu app_secret is required. Pass --app-secret or set FEISHU_APP_SECRET."
                )
            })?,
    };
    validate_auth(&auth)?;
    write_auth(&secret_dir.join(FEISHU_AUTH_FILE), &auth)?;
    println!(
        "Feishu credentials saved to {}",
        secret_dir.join(FEISHU_AUTH_FILE).display()
    );
    Ok(())
}

impl FeishuChannel {
    pub async fn new(
        agent: Arc<AgentService>,
        leases: Arc<SessionLeaseRegistry>,
        confirmations: Arc<PendingConfirmationRegistry>,
        agent_structure_dir: &Path,
        config: &FeishuChannelConfig,
    ) -> Result<Self> {
        let secret_dir = channel_secret_dir(agent_structure_dir).join(FEISHU_SECRET_SUBDIR);
        let auth_path = secret_dir.join(FEISHU_AUTH_FILE);
        let auth = read_auth(&auth_path)?;
        let workspace_dir = resolve_config_path(agent_structure_dir, &config.workspace_dir);
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
            agent,
            leases,
            confirmations,
            channel: Arc::new(create_channel(claw_config)),
            rest: Arc::new(FeishuRestClient::new(auth, base_url)),
            config: config.clone(),
            workspace_dir,
            message_locks: Mutex::new(HashMap::new()),
        })
    }

    pub async fn run(self) -> Result<()> {
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
                        tracing::warn!(target: "feishu", error = %err, "failed to handle feishu message");
                    }
                }
            }
        }
    }

    pub fn automation_notifier(&self) -> Arc<dyn AutomationNotificationSink> {
        Arc::new(FeishuAutomationNotifier {
            rest: self.rest.clone(),
        })
    }

    async fn handle_message(&self, msg: ChannelMessage) -> Result<()> {
        let kind = chat_kind(&msg);
        let peer_id = peer_id(&msg, kind);
        if !self.is_allowed(&msg, kind) {
            return Ok(());
        }

        let session_key = format!("{}:{peer_id}", chat_kind_name(kind));
        let lock = self.session_lock(&session_key).await;
        let _guard = lock.lock().await;

        let state_dir = self
            .agent
            .channel_state_dir()
            .join(FEISHU_STATE_SUBDIR)
            .join(chat_kind_name(kind))
            .join(sanitize_filename(peer_id));
        let holder = format!("feishu:{}:{peer_id}", chat_kind_name(kind));
        let channel_control = ChannelControl::new(
            self.agent.clone(),
            self.leases.clone(),
            self.confirmations.clone(),
            holder.clone(),
            &state_dir,
            self.workspace_dir.to_string_lossy().to_string(),
            self.config.default_session_id.as_deref(),
            self.config.override_model.as_deref(),
            self.config.override_reasoning_mode,
        );
        if let Some(command_text) = command_text(&msg.content)
            && let Some(reply) = channel_control.handle_command(&command_text).await?
        {
            self.channel.send(&reply, reply_target(&msg, kind)).await?;
            return Ok(());
        }

        let session = channel_control.active_session().await?;
        let Some((user_input, user_blocks)) = self
            .build_user_input(&msg, kind, session.session_dir())
            .await?
        else {
            return Ok(());
        };

        let expose_output_tools = self.config.media_output || self.config.card_output;
        let tool_manager = session.tool_manager().await;
        if expose_output_tools {
            let bridge = Arc::new(FeishuReplyBridge {
                rest: self.rest.clone(),
                to: reply_target(&msg, kind).to_string(),
            });
            let executor = Arc::new(FeishuToolExecutor::new(
                bridge,
                vec![
                    self.workspace_dir.clone(),
                    session.session_dir().to_path_buf(),
                ],
                self.config.media_output,
                self.config.card_output,
            ));
            tool_manager.set_channel_tool_executor(Some(executor)).await;
        }

        let update_collector = ChannelUpdateCollector::new(self.config.response_detail);
        let emit_update = update_collector.emitter();
        let request_permission = confirming_permission_requester(
            self.confirmations.clone(),
            self.channel.clone(),
            reply_target(&msg, kind).to_string(),
            holder,
        );
        let run_result = self
            .agent
            .run_prompt_with_extra_tools(
                session.session_id(),
                user_input,
                user_blocks,
                emit_update,
                request_permission,
                feishu_tool_schemas(self.config.media_output, self.config.card_output),
            )
            .await;
        if expose_output_tools {
            tool_manager.set_channel_tool_executor(None).await;
        }
        run_result?;

        let collected = update_collector.finish().await;
        let target = reply_target(&msg, kind);
        if let Some(detail) = collected.detail_text.as_deref() {
            self.channel.send(detail, target).await?;
        }
        if !collected.response_text.is_empty() {
            self.channel.send(&collected.response_text, target).await?;
        }
        Ok(())
    }

    async fn build_user_input(
        &self,
        msg: &ChannelMessage,
        kind: FeishuChatKind,
        session_dir: &Path,
    ) -> Result<Option<(Value, Vec<Value>)>> {
        let mut blocks: Vec<ContentBlock> = Vec::new();
        let media = parse_media(&msg.content);
        let text = message_text(&msg.content, media.is_some());

        if let Some(text) = text
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            let text = if kind == FeishuChatKind::Group {
                format!("Feishu group message from {}:\n{text}", msg.sender)
            } else {
                text.to_string()
            };
            blocks.push(ContentBlock::Text(TextContent::new(text)));
        }

        if let Some(media) = media {
            if self.config.media_input {
                let downloaded = self
                    .download_message_media(msg, &media, session_dir)
                    .await?;
                let uri = file_uri_from_path(&downloaded.path);
                let name = downloaded.path.file_name().and_then(|name| name.to_str());
                blocks.push(resource_link_block(
                    &uri,
                    name,
                    Some(downloaded.mime_type.as_str()),
                )?);
                if media.kind == FeishuMediaKind::Image
                    && let Some(image) =
                        image_url_block_from_file(&downloaded.path, &downloaded.mime_type)?
                {
                    blocks.push(image);
                }
            } else if blocks.is_empty() {
                self.channel
                    .send("当前飞书通道未开启媒体输入。", reply_target(msg, kind))
                    .await?;
                return Ok(None);
            }
        }

        if blocks.is_empty() {
            return Ok(None);
        }
        append_channel_context(
            &mut blocks,
            "Feishu",
            self.config.media_output,
            self.config.card_output,
        )?;

        Ok(Some(mapper::normalize_prompt_blocks(&blocks)?))
    }

    async fn download_message_media(
        &self,
        msg: &ChannelMessage,
        media: &FeishuInboundMedia,
        session_dir: &Path,
    ) -> Result<DownloadedFeishuMedia> {
        let attachments_dir = session_dir
            .join("attachments")
            .join("inbox")
            .join("feishu")
            .join(sanitize_filename(&msg.id));
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
            .unwrap_or_else(|| default_media_filename(media.kind, &downloaded.mime_type));
        let path = attachments_dir.join(filename);
        tokio::fs::write(&path, &downloaded.bytes)
            .await
            .with_context(|| format!("write Feishu media {}", path.display()))?;
        Ok(DownloadedFeishuMedia {
            path,
            mime_type: downloaded.mime_type,
        })
    }

    async fn session_lock(&self, session_key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.message_locks.lock().await;
        locks
            .entry(session_key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn is_allowed(&self, msg: &ChannelMessage, kind: FeishuChatKind) -> bool {
        match kind {
            FeishuChatKind::Direct => match self.config.dm_policy {
                FeishuAccessPolicy::AllowAll => true,
                FeishuAccessPolicy::WhiteList => {
                    allow_list_contains(&self.config.allow_from, &msg.sender)
                }
            },
            FeishuChatKind::Group => match self.config.group_policy {
                FeishuAccessPolicy::AllowAll => true,
                FeishuAccessPolicy::WhiteList => {
                    allow_list_contains(&self.config.group_allow_from, &msg.channel)
                }
            },
        }
    }
}

struct FeishuResourceDownload {
    bytes: Vec<u8>,
    mime_type: String,
}

#[async_trait::async_trait]
impl FeishuToolBridge for FeishuReplyBridge {
    async fn reply_media(
        &self,
        path: &Path,
        kind: FeishuReplyMediaKind,
        file_type: Option<&str>,
    ) -> Result<FeishuReplyMediaResult> {
        let send_as_image = match kind {
            FeishuReplyMediaKind::Image => true,
            FeishuReplyMediaKind::File => false,
            FeishuReplyMediaKind::Auto => is_feishu_image_path(path),
        };

        if send_as_image {
            let image_key = self.rest.upload_image(path).await?;
            let message_id = self
                .rest
                .send_message(&self.to, "image", json!({ "image_key": image_key }))
                .await?;
            Ok(FeishuReplyMediaResult {
                message_id,
                resource_key: image_key,
                msg_type: "image".to_string(),
            })
        } else {
            let file_key = self.rest.upload_file(path, file_type).await?;
            let message_id = self
                .rest
                .send_message(&self.to, "file", json!({ "file_key": file_key }))
                .await?;
            Ok(FeishuReplyMediaResult {
                message_id,
                resource_key: file_key,
                msg_type: "file".to_string(),
            })
        }
    }

    async fn reply_card(&self, card: Value) -> Result<FeishuReplyCardResult> {
        let message_id = self
            .rest
            .send_message(&self.to, "interactive", card)
            .await?;
        Ok(FeishuReplyCardResult { message_id })
    }
}

#[async_trait::async_trait]
impl AutomationNotificationSink for FeishuAutomationNotifier {
    async fn send(&self, notify: &AutomationNotifyConfig, text: &str) -> Result<String> {
        let recipient = notify
            .recipient
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("feishu automation notify requires recipient"))?;
        self.rest
            .send_message(&recipient.id, "text", json!({ "text": text }))
            .await
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
            .unwrap_or_else(|| default_mime_type(resource_type).to_string());
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
        let form = Form::new()
            .text("image_type", "message")
            .part("image", part);
        let url = format!("{}/open-apis/im/v1/images", self.base_url);
        let response = self
            .http
            .post(url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .multipart(form)
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
        let url = format!("{}/open-apis/im/v1/files", self.base_url);
        let response = self
            .http
            .post(url)
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
        let url = format!("{}/open-apis/im/v1/messages", self.base_url);
        let response = self
            .http
            .post(url)
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

        let url = format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            self.base_url
        );
        let response = self
            .http
            .post(url)
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
        let mut guard = self.token.lock().await;
        *guard = Some(CachedTenantToken {
            value: value.clone(),
            expires_at,
        });
        Ok(value)
    }
}

fn confirming_permission_requester(
    confirmations: Arc<PendingConfirmationRegistry>,
    channel: Arc<clawrs_feishu::FeishuChannelService>,
    target: String,
    holder: String,
) -> PermissionRequester {
    Arc::new(move |session_id: String, payload: Map<String, Value>| {
        let confirmations = confirmations.clone();
        let channel = channel.clone();
        let target = target.clone();
        let holder = holder.clone();
        Box::pin(async move {
            let (snapshot, rx) = confirmations.create(session_id, payload, holder)?;
            let message = ChannelControl::confirmation_message(&snapshot);
            channel.send(&message, &target).await?;
            match rx.await {
                Ok(decision) => Ok(decision),
                Err(_) => Ok(PERMISSION_REJECT_ONCE.to_string()),
            }
        })
    })
}

fn read_auth(path: &Path) -> Result<FeishuAuth> {
    if !path.is_file() {
        bail!(
            "Feishu auth file not found: {}. Run `dwo-agent channel login feishu` first.",
            path.display()
        );
    }
    let text = read_utf8_text(path)?;
    let auth: FeishuAuth =
        serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    validate_auth(&auth)?;
    Ok(auth)
}

fn write_auth(path: &Path, auth: &FeishuAuth) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_yaml::to_string(auth)?;
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

fn validate_auth(auth: &FeishuAuth) -> Result<()> {
    if auth.app_id.trim().is_empty() {
        bail!("app_id is required in Feishu auth.");
    }
    if auth.app_secret.trim().is_empty() {
        bail!("app_secret is required in Feishu auth.");
    }
    Ok(())
}

fn to_claw_domain(domain: FeishuChannelDomain) -> ClawFeishuDomain {
    match domain {
        FeishuChannelDomain::Feishu => ClawFeishuDomain::Feishu,
        FeishuChannelDomain::Lark => ClawFeishuDomain::Lark,
    }
}

fn chat_kind(msg: &ChannelMessage) -> FeishuChatKind {
    if msg.chat_type.as_deref() == Some("group") {
        FeishuChatKind::Group
    } else {
        FeishuChatKind::Direct
    }
}

fn chat_kind_name(kind: FeishuChatKind) -> &'static str {
    match kind {
        FeishuChatKind::Direct => "dm",
        FeishuChatKind::Group => "group",
    }
}

fn peer_id(msg: &ChannelMessage, kind: FeishuChatKind) -> &str {
    match kind {
        FeishuChatKind::Direct => &msg.sender,
        FeishuChatKind::Group => &msg.channel,
    }
}

fn reply_target(msg: &ChannelMessage, kind: FeishuChatKind) -> &str {
    peer_id(msg, kind)
}

fn command_text(content: &str) -> Option<String> {
    let has_media = parse_media(content).is_some();
    let text = message_text(content, has_media)?;
    let trimmed = text.trim();
    trimmed
        .starts_with('/')
        .then(|| trimmed.to_string())
        .filter(|text| !text.is_empty())
}

fn allow_list_contains(list: &[String], value: &str) -> bool {
    !value.trim().is_empty()
        && list
            .iter()
            .map(|entry| entry.trim())
            .any(|entry| entry == "*" || entry == value)
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

fn image_mime_type_for_path(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("png") => Some("image/png"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        Some("bmp") => Some("image/bmp"),
        Some("ico") => Some("image/x-icon"),
        Some("tiff") | Some("tif") => Some("image/tiff"),
        Some("heic") => Some("image/heic"),
        _ => None,
    }
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

fn parse_media(content: &str) -> Option<FeishuInboundMedia> {
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

fn resolve_config_path(agent_structure_dir: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    let resolved = if path.is_absolute() {
        path
    } else {
        agent_structure_dir.join(path)
    };
    std::fs::canonicalize(&resolved).unwrap_or(resolved)
}

fn image_url_block_from_file(path: &Path, mime_type: &str) -> Result<Option<ContentBlock>> {
    if !mime_type.starts_with("image/") {
        return Ok(None);
    }
    let data = std::fs::read(path).with_context(|| format!("read image {}", path.display()))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    Ok(Some(ContentBlock::Image(ImageContent::new(
        encoded, mime_type,
    ))))
}

fn append_channel_context(
    blocks: &mut Vec<ContentBlock>,
    channel: &str,
    media_output: bool,
    card_output: bool,
) -> Result<()> {
    let mut lines = vec![
        "<channel_context>".to_string(),
        format!("当前消息来自 {channel} 频道。"),
    ];
    if media_output {
        lines.push(
            "本轮如需发送本地文件或图片，请使用 feishu_reply_media 回复当前飞书对话。".to_string(),
        );
    }
    if card_output {
        lines.push(
            "本轮如需发送飞书交互卡片，请使用 feishu_reply_card 回复当前飞书对话。".to_string(),
        );
    }
    lines.push("</channel_context>".to_string());
    blocks.push(ContentBlock::Text(TextContent::new(lines.join("\n"))));
    Ok(())
}

fn resource_link_block(
    uri: &str,
    name: Option<&str>,
    mime_type: Option<&str>,
) -> Result<ContentBlock> {
    let uri = uri.trim();
    if uri.is_empty() {
        bail!("resource_link block must provide uri");
    }
    let name = name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("attachment");
    let mut link = ResourceLink::new(name, uri);
    if let Some(mime_type) = mime_type.map(str::trim).filter(|value| !value.is_empty()) {
        link = link.mime_type(mime_type.to_string());
    }
    Ok(ContentBlock::ResourceLink(link))
}

fn file_uri_from_path(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let encoded = percent_encode_file_path(&normalized);
    if encoded.starts_with("//") {
        format!("file:{encoded}")
    } else if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

fn percent_encode_file_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn default_media_filename(kind: FeishuMediaKind, mime_type: &str) -> String {
    let stem = match kind {
        FeishuMediaKind::Image => "image",
        FeishuMediaKind::File => "file",
    };
    let ext = extension_for_mime_type(mime_type).unwrap_or("bin");
    format!("{stem}.{ext}")
}

fn default_mime_type(resource_type: &str) -> &'static str {
    match resource_type {
        "image" => "image/jpeg",
        _ => "application/octet-stream",
    }
}

fn extension_for_mime_type(mime_type: &str) -> Option<&'static str> {
    match mime_type {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/bmp" => Some("bmp"),
        "text/plain" => Some("txt"),
        "text/markdown" => Some("md"),
        "application/json" => Some("json"),
        "application/pdf" => Some("pdf"),
        "application/zip" => Some("zip"),
        "audio/mpeg" => Some("mp3"),
        "audio/wav" => Some("wav"),
        "audio/ogg" => Some("ogg"),
        "video/mp4" => Some("mp4"),
        "video/quicktime" => Some("mov"),
        "video/webm" => Some("webm"),
        _ => None,
    }
}

fn sanitize_filename(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('.').trim().to_string();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}
