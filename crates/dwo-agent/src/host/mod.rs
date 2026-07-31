use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use dwo_agent_service::{
    AgentService, EndpointId, FsSessionRepository, LoadedAgentProfile, NewSession, SessionConfig,
    SessionConfigUpdate, SessionEventPayload, SessionId, SessionLlmSettings,
};
use dwo_mcp::McpRuntime;
use dwo_tools::{ConfirmationDecision, PolicyConfig, SessionMode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::automation::{AutomationRuntime, parse_config as parse_automation_config};
use crate::channels::{
    ChannelHub, ChannelKind, ChannelManager, FeishuBindProgress, TelegramBindProgress,
    WeixinLoginProgress,
};

pub struct Host {
    pub service: Arc<AgentService>,
    pub channels: Arc<ChannelManager>,
    pub channel_hub: Arc<ChannelHub>,
    pub mcp: Arc<McpRuntime>,
    pub automation: Arc<AutomationRuntime>,
    profile_root: PathBuf,
    config_path: PathBuf,
    default_model: String,
    default_mode: dwo_tools::SessionMode,
    default_max_model_steps: usize,
    model_options: Vec<SessionModelOption>,
    profile_name: String,
    profile_description: String,
    shutdown: CancellationToken,
}

#[derive(Deserialize)]
struct SessionIdParam {
    session_id: String,
}

#[derive(Deserialize)]
struct NewSessionParam {
    title: Option<String>,
    cwd: Option<PathBuf>,
}

#[derive(Deserialize)]
struct ListSessionParam {
    #[serde(default)]
    all: bool,
    caller_session_id: Option<String>,
}

#[derive(Deserialize)]
struct PromptParam {
    session_id: Option<String>,
    caller_session_id: Option<String>,
    endpoint_id: String,
    message: String,
    title: Option<String>,
    cwd: Option<PathBuf>,
    policy: Option<SessionMode>,
    model: Option<String>,
    reasoning: Option<String>,
}

#[derive(Deserialize)]
struct ReadSessionParam {
    session_id: String,
    cursor: Option<usize>,
    #[serde(default = "default_read_limit")]
    limit: usize,
}

fn default_read_limit() -> usize {
    3
}

#[derive(Deserialize)]
struct CancelParam {
    session_id: String,
    turn_id: Option<String>,
}

#[derive(Deserialize)]
struct ConfigParam {
    session_id: String,
    value: Value,
}

#[derive(Deserialize)]
struct SessionConfigOptionParam {
    session_id: String,
    config_id: String,
    value: Value,
}

#[derive(Deserialize)]
struct PermissionParam {
    session_id: String,
    endpoint_id: String,
    request_id: String,
    allowed: bool,
    reason: Option<String>,
}

#[derive(Deserialize)]
struct PollBindParam {
    binding_id: String,
    verify_code: Option<String>,
}

#[derive(Deserialize)]
struct TelegramPollBindParam {
    binding_id: String,
}

#[derive(Deserialize)]
struct SendMessageParam {
    text: String,
}

#[derive(Deserialize)]
struct SendFileParam {
    path: PathBuf,
}

#[derive(Clone, Copy)]
enum ManagedChannelAction {
    Status,
    SendMessage,
    SendFile,
    Remove,
    Token,
    ResetToken,
}

#[derive(Deserialize)]
struct McpSearchParam {
    query: String,
}

#[derive(Deserialize)]
struct McpCallParam {
    selector: String,
    arguments: Value,
}

#[derive(Deserialize)]
struct McpAuthParam {
    server: String,
}

#[derive(Deserialize)]
struct AutomationJobParam {
    job: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionModelOption {
    id: String,
    reasoning: Vec<String>,
    default_reasoning: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionOptionSnapshot {
    config: SessionConfig,
    models: Vec<SessionModelOption>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileSnapshot {
    name: String,
    description: String,
    policy: SessionMode,
    default_model: String,
    models: Vec<SessionModelOption>,
    session_count: usize,
}

impl Host {
    pub async fn load(config_path: &Path) -> Result<Arc<Self>> {
        let profile_root = profile_root(config_path)?;
        let mcp = Arc::new(McpRuntime::new(&profile_root));
        mcp.sync_and_start().await?;
        tracing::info!(event = "mcp.synchronized", "MCP configuration synchronized");
        let profile = LoadedAgentProfile::load(&profile_root)?;
        let default_model = profile.models.default_model_id.clone();
        let default_mode = profile.config.policy_mode;
        let default_max_model_steps = profile.config.max_model_steps;
        let profile_name = profile.config.name.clone();
        let profile_description = profile.config.description.clone();
        let model_options = profile
            .models
            .models
            .iter()
            .map(|(id, model)| SessionModelOption {
                id: id.clone(),
                reasoning: model.reasoning.keys().cloned().collect(),
                default_reasoning: model.default_reasoning_mode.clone(),
            })
            .collect();
        let automation_config = parse_automation_config(profile.config.automation.clone())?;
        let repository =
            Arc::new(FsSessionRepository::new(profile_root.join("runtime/sessions")).await?);
        let channels =
            Arc::new(ChannelManager::new(&profile_root, &profile.config.channels).await?);
        let service = Arc::new(AgentService::from_profile(
            repository,
            profile,
            PolicyConfig::default(),
        )?);
        let shutdown = CancellationToken::new();
        let automation = AutomationRuntime::new(
            service.clone(),
            profile_root.clone(),
            automation_config,
            default_model.clone(),
            default_mode,
            default_max_model_steps,
            shutdown.clone(),
        )?;
        let host = Arc::new(Self {
            service,
            channels,
            channel_hub: Arc::new(ChannelHub::new()),
            mcp,
            automation,
            profile_root,
            config_path: config_path.to_path_buf(),
            default_model,
            default_mode,
            default_max_model_steps,
            model_options,
            profile_name,
            profile_description,
            shutdown,
        });
        host.channel_hub.start_all(host.clone()).await;
        host.start_mcp_watcher();
        host.automation.start();
        Ok(host)
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    pub(crate) fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub async fn shutdown(&self) {
        tokio::join!(
            self.channel_hub.stop_all(),
            self.mcp.shutdown(),
            self.service.shutdown()
        );
    }

    pub async fn dispatch(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        if let Some((channel, action)) = managed_channel_action(method) {
            return self.dispatch_channel(channel, action, params).await;
        }
        if method.starts_with("channel.") {
            return self.dispatch_channel_binding(method, params).await;
        }
        if method.starts_with("automation.") {
            return self.dispatch_automation(method, params).await;
        }
        if method.starts_with("mcp.") {
            return self.dispatch_mcp(method, params).await;
        }
        match method {
            "daemon.status" => Ok(json!({
                "healthy": true,
                "profile_root": self.profile_root,
                "sessions": self.service.list().await?.len(),
                "channels": self.channels.list().await?.len(),
                "automationJobs": self.automation.list().await.len(),
            })),
            "daemon.shutdown" => {
                self.shutdown.cancel();
                Ok(json!({"stopping": true}))
            }
            "profile.list" => Ok(serde_json::to_value(ProfileSnapshot {
                name: self.profile_name.clone(),
                description: self.profile_description.clone(),
                policy: self.default_mode,
                default_model: self.default_model.clone(),
                models: self.model_options.clone(),
                session_count: self.service.list().await?.len(),
            })?),
            "session.list" => {
                let params: ListSessionParam = serde_json::from_value(params)?;
                let caller = parse_optional_session(params.caller_session_id.clone())?;
                let mut records = self.service.list().await?;
                if !params.all {
                    records.retain(|record| record.info.parent_session_id == caller);
                }
                Ok(serde_json::to_value(records)?)
            }
            "session.snapshot" => {
                let id = parse_session(params)?;
                Ok(serde_json::to_value(
                    self.service.load(&id).await?.snapshot().await?,
                )?)
            }
            "session.new" => {
                let params: NewSessionParam = serde_json::from_value(params)?;
                let agent = self.create_session(params.title, params.cwd).await?;
                let snapshot = agent.snapshot().await?;
                Ok(json!({
                    "session_id": agent.id(),
                    "usage": snapshot.usage,
                }))
            }
            "session.delete" => {
                let id = parse_session(params)?;
                self.delete_session(&id).await?;
                Ok(json!({"deleted": true}))
            }
            "session.prompt" => {
                let params: PromptParam = serde_json::from_value(params)?;
                let caller = parse_optional_session(params.caller_session_id.clone())?;
                let (agent, parent_id) = self.resolve_prompt_session(&params, caller).await?;
                let endpoint = EndpointId::parse(params.endpoint_id).map_err(anyhow::Error::msg)?;
                let subscription = agent.attach(EndpointId::new()).await?;
                let turn_id = agent.prompt(endpoint, params.message).await?;
                if let Some(parent_id) = parent_id {
                    self.spawn_result_delivery(
                        subscription,
                        agent.id().clone(),
                        parent_id,
                        turn_id.clone(),
                    );
                }
                Ok(json!({"session_id": agent.id(), "turn_id": turn_id}))
            }
            "session.read" => {
                let params: ReadSessionParam = serde_json::from_value(params)?;
                anyhow::ensure!(
                    params.limit > 0 && params.limit <= 100,
                    "limit must be between 1 and 100"
                );
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let snapshot = self.service.load(&id).await?.snapshot().await?;
                let total = snapshot.transcript.len();
                let content = snapshot
                    .transcript
                    .into_iter()
                    .enumerate()
                    .filter(|(_, event)| is_content_event(&event.payload))
                    .collect::<Vec<_>>();
                let messages = if let Some(cursor) = params.cursor {
                    content
                        .into_iter()
                        .filter(|(index, _)| *index >= cursor.min(total))
                        .take(params.limit)
                        .collect::<Vec<_>>()
                } else {
                    content
                        .into_iter()
                        .rev()
                        .take(params.limit)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                };
                let start = messages
                    .first()
                    .map_or(params.cursor.unwrap_or(total), |(index, _)| *index);
                let next_cursor = messages.last().map_or(start, |(index, _)| index + 1);
                Ok(json!({
                    "session_id": id,
                    "cursor": start,
                    "next_cursor": next_cursor,
                    "messages": messages.into_iter().map(|(cursor, event)| json!({"cursor": cursor, "event": event})).collect::<Vec<_>>(),
                }))
            }
            "session.cancel" => {
                let params: CancelParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let turn = params
                    .turn_id
                    .map(dwo_context::TurnId::parse)
                    .transpose()
                    .map_err(anyhow::Error::msg)?;
                self.service.load(&id).await?.cancel(turn).await?;
                Ok(json!({"cancelled": true}))
            }
            "session.set_model" => {
                let params: ConfigParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let model = params
                    .value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("model must be a string"))?;
                let agent = self.service.load(&id).await?;
                agent
                    .set_config(SessionConfigUpdate::Model(model.to_string()))
                    .await?;
                Ok(json!({
                    "updated": true,
                    "usage": agent.snapshot().await?.usage,
                }))
            }
            "session.set_reasoning" => {
                let params: ConfigParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let reasoning = params.value.as_str().map(str::to_string);
                self.service
                    .set_config(&id, SessionConfigUpdate::Reasoning(reasoning))
                    .await?;
                Ok(json!({"updated": true}))
            }
            "session.set_config_option" => {
                let params: SessionConfigOptionParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let update = match params.config_id.as_str() {
                    "model" => SessionConfigUpdate::Model(
                        params
                            .value
                            .as_str()
                            .context("model config value must be a string")?
                            .to_string(),
                    ),
                    "reasoning_mode" => SessionConfigUpdate::Reasoning(Some(
                        params
                            .value
                            .as_str()
                            .context("reasoning config value must be a string")?
                            .to_string(),
                    )),
                    "policy_mode" => {
                        SessionConfigUpdate::Mode(serde_json::from_value(params.value)?)
                    }
                    other => anyhow::bail!("unknown session config option: {other}"),
                };
                let agent = self.service.load(&id).await?;
                agent.set_config(update).await?;
                Ok(json!({
                    "updated": true,
                    "usage": agent.snapshot().await?.usage,
                }))
            }
            "session.options" => {
                let id = parse_session(params)?;
                let record = self
                    .service
                    .list()
                    .await?
                    .into_iter()
                    .find(|record| record.info.id == id)
                    .with_context(|| format!("session not found: {id}"))?;
                Ok(serde_json::to_value(SessionOptionSnapshot {
                    config: record.config(),
                    models: self.model_options.clone(),
                })?)
            }
            "session.permission" => {
                let params: PermissionParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let endpoint = EndpointId::parse(params.endpoint_id).map_err(anyhow::Error::msg)?;
                self.service
                    .load(&id)
                    .await?
                    .respond_permission(
                        endpoint,
                        params.request_id,
                        ConfirmationDecision {
                            allowed: params.allowed,
                            reason: params.reason,
                        },
                    )
                    .await?;
                Ok(json!({"resolved": true}))
            }
            other => anyhow::bail!("unknown RPC method: {other}"),
        }
    }

    async fn dispatch_automation(&self, method: &str, params: Value) -> Result<Value> {
        match method {
            "automation.list" | "automation.status" => {
                Ok(serde_json::to_value(self.automation.list().await)?)
            }
            "automation.run" => {
                let params: AutomationJobParam = serde_json::from_value(params)?;
                Ok(serde_json::to_value(
                    self.automation.run_now(&params.job).await?,
                )?)
            }
            other => anyhow::bail!("unknown RPC method: {other}"),
        }
    }

    async fn dispatch_mcp(&self, method: &str, params: Value) -> Result<Value> {
        match method {
            "mcp.search" => {
                let params: McpSearchParam = serde_json::from_value(params)?;
                let catalog = self.mcp.catalog_snapshot().await?;
                Ok(serde_json::to_value(catalog.search(&params.query))?)
            }
            "mcp.call" => {
                let params: McpCallParam = serde_json::from_value(params)?;
                Ok(serde_json::to_value(
                    self.mcp.call(&params.selector, params.arguments).await?,
                )?)
            }
            "mcp.auth.login" => {
                let params: McpAuthParam = serde_json::from_value(params)?;
                self.mcp.auth_login(&params.server).await?;
                Ok(json!({"authorized": true, "server": params.server}))
            }
            "mcp.auth.logout" => {
                let params: McpAuthParam = serde_json::from_value(params)?;
                self.mcp.auth_logout(&params.server).await?;
                Ok(json!({"authorized": false, "server": params.server}))
            }
            other => anyhow::bail!("unknown RPC method: {other}"),
        }
    }

    async fn dispatch_channel_binding(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        match method {
            "channel.list" => Ok(serde_json::to_value(self.channels.list().await?)?),
            "channel.weixin.begin" => Ok(serde_json::to_value(
                self.channels.begin_weixin_login().await?,
            )?),
            "channel.weixin.poll" => {
                let params: PollBindParam = serde_json::from_value(params)?;
                let progress = self
                    .channels
                    .poll_weixin_login(&params.binding_id, params.verify_code.as_deref())
                    .await?;
                if let WeixinLoginProgress::Confirmed { channel } = &progress {
                    self.channel_hub.stop(ChannelKind::Weixin).await;
                    if channel.enabled {
                        self.channel_hub
                            .start(ChannelKind::Weixin, self.clone())
                            .await?;
                    }
                }
                Ok(serde_json::to_value(progress)?)
            }
            "channel.telegram.begin" => {
                let start = self.channels.begin_telegram_bind().await?;
                self.channel_hub.stop(ChannelKind::Telegram).await;
                Ok(serde_json::to_value(start)?)
            }
            "channel.telegram.poll" => {
                let params: TelegramPollBindParam = serde_json::from_value(params)?;
                let progress = self.channels.poll_telegram_bind(&params.binding_id).await?;
                if let TelegramBindProgress::Confirmed { channel } = &progress
                    && channel.enabled
                {
                    self.channel_hub
                        .start(ChannelKind::Telegram, self.clone())
                        .await?;
                }
                Ok(serde_json::to_value(progress)?)
            }
            "channel.feishu.begin" => {
                self.channel_hub.stop(ChannelKind::Feishu).await;
                Ok(serde_json::to_value(
                    self.channels.begin_feishu_bind().await?,
                )?)
            }
            "channel.feishu.poll" => {
                let params: TelegramPollBindParam = serde_json::from_value(params)?;
                let progress = self.channels.poll_feishu_bind(&params.binding_id).await?;
                if let FeishuBindProgress::Confirmed { channel } = &progress
                    && channel.enabled
                {
                    self.channel_hub
                        .start(ChannelKind::Feishu, self.clone())
                        .await?;
                }
                Ok(serde_json::to_value(progress)?)
            }
            other => anyhow::bail!("unknown RPC method: {other}"),
        }
    }

    async fn dispatch_channel(
        self: &Arc<Self>,
        channel: ChannelKind,
        action: ManagedChannelAction,
        params: Value,
    ) -> Result<Value> {
        match action {
            ManagedChannelAction::Status => {
                let mut value = serde_json::to_value(self.channels.summary(channel).await?)?;
                if channel == ChannelKind::Websocket {
                    let runtime = self.channels.load_websocket().await?;
                    let object = value.as_object_mut().expect("channel summary is an object");
                    object.insert(
                        "running".to_string(),
                        json!(self.channel_hub.is_running(channel).await),
                    );
                    object.insert(
                        "listen".to_string(),
                        json!(format!("0.0.0.0:{}", runtime.config.port)),
                    );
                    object.insert("path".to_string(), json!("/acp"));
                    object.insert("authentication".to_string(), json!("token"));
                }
                Ok(value)
            }
            ManagedChannelAction::SendMessage => {
                let params: SendMessageParam = serde_json::from_value(params)?;
                let target = self.channels.bound_target(channel).await?;
                self.channel_hub.send_message(channel, &params.text).await?;
                Ok(json!({"sent": true, "to": target}))
            }
            ManagedChannelAction::SendFile => {
                let params: SendFileParam = serde_json::from_value(params)?;
                let target = self.channels.bound_target(channel).await?;
                self.channel_hub.send_file(channel, &params.path).await?;
                Ok(json!({"sent": true, "to": target, "path": params.path}))
            }
            ManagedChannelAction::Remove => {
                self.channel_hub.stop(channel).await;
                Ok(json!({"removed": self.channels.remove(channel).await?}))
            }
            ManagedChannelAction::Token => {
                anyhow::ensure!(
                    channel == ChannelKind::Websocket,
                    "token is only available for WebSocket"
                );
                let runtime = self.channels.load_websocket().await?;
                Ok(json!({"token": runtime.token, "port": runtime.config.port, "path": "/acp"}))
            }
            ManagedChannelAction::ResetToken => {
                anyhow::ensure!(
                    channel == ChannelKind::Websocket,
                    "reset-token is only available for WebSocket"
                );
                self.channel_hub.stop(channel).await;
                let token = self.channels.reset_websocket_token().await?;
                let summary = self.channels.summary(channel).await?;
                if summary.enabled {
                    self.channel_hub.start(channel, self.clone()).await?;
                }
                Ok(json!({"token": token, "reset": true}))
            }
        }
    }

    pub async fn create_session(
        &self,
        title: Option<String>,
        cwd: Option<PathBuf>,
    ) -> Result<Arc<dwo_agent_service::SessionAgent>> {
        let session_id = SessionId::new();
        let uses_generated_workspace = cwd.is_none();
        let cwd = match cwd {
            Some(cwd) if cwd.is_absolute() => cwd,
            Some(cwd) => self.profile_root.join(cwd),
            None => {
                let workspace = self
                    .profile_root
                    .join("runtime")
                    .join("workspaces")
                    .join(session_id.as_str());
                tokio::fs::create_dir_all(&workspace)
                    .await
                    .with_context(|| format!("create default workspace {}", workspace.display()))?;
                workspace
            }
        };
        let cleanup_path = uses_generated_workspace.then(|| cwd.clone());
        let created = self
            .service
            .create(NewSession {
                id: Some(session_id),
                parent_session_id: None,
                title,
                cwd,
                mode: self.default_mode,
                max_model_steps: self.default_max_model_steps,
                llm: SessionLlmSettings {
                    model: self.default_model.clone(),
                    reasoning: None,
                },
            })
            .await;
        match created {
            Ok(session) => Ok(session),
            Err(error) => {
                if let Some(path) = cleanup_path
                    && path.is_dir()
                {
                    let _ = tokio::fs::remove_dir_all(path).await;
                }
                Err(error.into())
            }
        }
    }

    async fn resolve_prompt_session(
        &self,
        params: &PromptParam,
        caller: Option<SessionId>,
    ) -> Result<(Arc<dwo_agent_service::SessionAgent>, Option<SessionId>)> {
        let records = self.service.list().await?;
        let caller_record = caller
            .as_ref()
            .map(|id| {
                records
                    .iter()
                    .find(|record| &record.info.id == id)
                    .cloned()
                    .with_context(|| format!("caller session not found: {id}"))
            })
            .transpose()?;

        if let Some(target) = &params.session_id {
            anyhow::ensure!(
                params.title.is_none() && params.cwd.is_none(),
                "--title and --cwd can only be used when creating a subsession"
            );
            let id = SessionId::parse(target.clone()).map_err(anyhow::Error::msg)?;
            let record = records
                .iter()
                .find(|record| record.info.id == id)
                .with_context(|| format!("session not found: {id}"))?;
            if let Some(caller) = &caller {
                anyhow::ensure!(
                    record.info.parent_session_id.as_ref() == Some(caller),
                    "session {id} is not a direct subsession of {caller}"
                );
            }
            if let (Some(parent), Some(mode)) = (&caller_record, params.policy) {
                ensure_policy_ceiling(mode, parent.info.mode)?;
            }
            let agent = self.service.load(&id).await?;
            if let Some(mode) = params.policy {
                agent.set_config(SessionConfigUpdate::Mode(mode)).await?;
            }
            if let Some(model) = &params.model {
                agent
                    .set_config(SessionConfigUpdate::Model(model.clone()))
                    .await?;
            }
            if let Some(reasoning) = &params.reasoning {
                agent
                    .set_config(SessionConfigUpdate::Reasoning(Some(reasoning.clone())))
                    .await?;
            }
            return Ok((agent, record.info.parent_session_id.clone()));
        }

        let inherited_mode = caller_record
            .as_ref()
            .map_or(self.default_mode, |record| record.info.mode);
        let mode = params.policy.unwrap_or(inherited_mode);
        if let Some(parent) = &caller_record {
            ensure_policy_ceiling(mode, parent.info.mode)?;
        }
        let model = params.model.clone().unwrap_or_else(|| {
            caller_record.as_ref().map_or_else(
                || self.default_model.clone(),
                |record| record.llm.model.clone(),
            )
        });
        let reasoning = params.reasoning.clone().or_else(|| {
            caller_record
                .as_ref()
                .and_then(|record| record.llm.reasoning.clone())
        });
        let session_id = SessionId::new();
        let requested_cwd = params
            .cwd
            .clone()
            .or_else(|| caller_record.as_ref().map(|record| record.info.cwd.clone()));
        let uses_generated_workspace = requested_cwd.is_none();
        let cwd = match requested_cwd {
            Some(cwd) if cwd.is_absolute() => cwd,
            Some(cwd) => self.profile_root.join(cwd),
            None => {
                let workspace = self
                    .profile_root
                    .join("runtime/workspaces")
                    .join(session_id.as_str());
                tokio::fs::create_dir_all(&workspace).await?;
                workspace
            }
        };
        let cleanup_path = uses_generated_workspace.then(|| cwd.clone());
        let created = self
            .service
            .create(NewSession {
                id: Some(session_id),
                parent_session_id: caller.clone(),
                title: params.title.clone(),
                cwd,
                mode,
                max_model_steps: self.default_max_model_steps,
                llm: SessionLlmSettings { model, reasoning },
            })
            .await;
        match created {
            Ok(agent) => Ok((agent, caller)),
            Err(error) => {
                if let Some(path) = cleanup_path
                    && path.is_dir()
                {
                    let _ = tokio::fs::remove_dir_all(path).await;
                }
                Err(error.into())
            }
        }
    }

    fn spawn_result_delivery(
        &self,
        subscription: dwo_agent_service::SessionSubscription,
        child_id: SessionId,
        parent_id: SessionId,
        watched_turn: dwo_agent_service::TurnId,
    ) {
        spawn_result_delivery(
            self.service.clone(),
            subscription,
            child_id,
            parent_id,
            watched_turn,
        );
    }

    pub async fn delete_session(&self, id: &SessionId) -> Result<()> {
        let record = self
            .service
            .list()
            .await?
            .into_iter()
            .find(|record| &record.info.id == id);
        self.service.delete(id).await?;

        let generated_workspace = self
            .profile_root
            .join("runtime")
            .join("workspaces")
            .join(id.as_str());
        let resolved_generated_workspace = std::fs::canonicalize(&generated_workspace)
            .unwrap_or_else(|_| generated_workspace.clone());
        if record
            .as_ref()
            .is_some_and(|record| record.info.cwd == resolved_generated_workspace)
            && generated_workspace.is_dir()
        {
            tokio::fs::remove_dir_all(&generated_workspace).await?;
        }
        remove_session_attachment_dirs(
            &self
                .profile_root
                .join("runtime")
                .join("attachments")
                .join("weixin"),
            id.as_str(),
        )
        .await?;
        remove_session_attachment_dirs(
            &self
                .profile_root
                .join("runtime")
                .join("attachments")
                .join("telegram"),
            id.as_str(),
        )
        .await?;
        remove_session_attachment_dirs(
            &self
                .profile_root
                .join("runtime")
                .join("attachments")
                .join("feishu"),
            id.as_str(),
        )
        .await
    }

    pub fn profile_root_path(&self) -> &Path {
        &self.profile_root
    }

    fn start_mcp_watcher(self: &Arc<Self>) {
        let runtime = self.mcp.clone();
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = interval.tick() => {
                        if let Err(error) = runtime.sync_and_start().await {
                            tracing::warn!(
                                event = "mcp.synchronization_failed",
                                error = %format!("{error:#}"),
                                "synchronize MCP configuration failed"
                            );
                        }
                    }
                }
            }
        });
    }

    pub async fn watch(
        &self,
        session_id: &str,
        endpoint_id: &str,
    ) -> Result<dwo_agent_service::SessionSubscription> {
        let id = SessionId::parse(session_id.to_string()).map_err(anyhow::Error::msg)?;
        let endpoint = EndpointId::parse(endpoint_id.to_string()).map_err(anyhow::Error::msg)?;
        Ok(self.service.load(&id).await?.attach(endpoint).await?)
    }
}

fn spawn_result_delivery(
    service: Arc<AgentService>,
    mut subscription: dwo_agent_service::SessionSubscription,
    child_id: SessionId,
    parent_id: SessionId,
    watched_turn: dwo_agent_service::TurnId,
) {
    tokio::spawn(async move {
        let mut content = String::new();
        let (status, error) = loop {
            let Some(event) = subscription.events.recv().await else {
                break ("closed", None);
            };
            match event.payload {
                SessionEventPayload::AssistantCompleted {
                    turn_id,
                    content: completed,
                    ..
                } if turn_id == watched_turn => content = completed,
                SessionEventPayload::TurnCompleted { turn_id } if turn_id == watched_turn => {
                    break ("completed", None);
                }
                SessionEventPayload::TurnCancelled { turn_id } if turn_id == watched_turn => {
                    break ("cancelled", None);
                }
                SessionEventPayload::TurnFailed { turn_id, error } if turn_id == watched_turn => {
                    break ("failed", Some(error));
                }
                _ => {}
            }
        };
        let notification = format!(
            "<subsession_result>\n{}\n</subsession_result>",
            json!({
                "session_id": child_id,
                "status": status,
                "content": content,
                "error": error,
            })
        );
        let parent = match service.load(&parent_id).await {
            Ok(parent) => parent,
            Err(error) => {
                tracing::error!(
                    event = "subsession.parent_load_failed",
                    parent_session_id = %parent_id,
                    error = %format!("{error:#}"),
                    "load subsession parent failed"
                );
                return;
            }
        };
        let parent_subscription = match parent.attach(EndpointId::new()).await {
            Ok(subscription) => subscription,
            Err(error) => {
                tracing::error!(
                    event = "subsession.parent_observe_failed",
                    parent_session_id = %parent_id,
                    error = %format!("{error:#}"),
                    "observe subsession parent failed"
                );
                return;
            }
        };
        let grandparent_id = parent_subscription
            .snapshot
            .record
            .info
            .parent_session_id
            .clone();
        match parent.notify_internal(notification).await {
            Ok(Some(parent_turn)) => {
                if let Some(grandparent_id) = grandparent_id {
                    spawn_result_delivery(
                        service,
                        parent_subscription,
                        parent_id,
                        grandparent_id,
                        parent_turn,
                    );
                }
            }
            Ok(None) => {}
            Err(error) => tracing::error!(
                event = "subsession.result_delivery_failed",
                parent_session_id = %parent_id,
                error = %format!("{error:#}"),
                "deliver subsession result failed"
            ),
        }
    });
}

async fn remove_session_attachment_dirs(root: &Path, session_id: &str) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let mut entries = tokio::fs::read_dir(&directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            if !file_type.is_dir() {
                continue;
            }
            if entry.file_name() == std::ffi::OsStr::new(session_id) {
                tokio::fs::remove_dir_all(entry.path()).await?;
            } else {
                directories.push(entry.path());
            }
        }
    }
    Ok(())
}

fn managed_channel_action(method: &str) -> Option<(ChannelKind, ManagedChannelAction)> {
    let (channel, action) = method.strip_prefix("channel.")?.split_once('.')?;
    let channel = ChannelKind::parse(channel)?;
    let action = match action {
        "status" => ManagedChannelAction::Status,
        "send_message" => ManagedChannelAction::SendMessage,
        "send_file" => ManagedChannelAction::SendFile,
        "remove" => ManagedChannelAction::Remove,
        "token" => ManagedChannelAction::Token,
        "reset_token" => ManagedChannelAction::ResetToken,
        _ => return None,
    };
    Some((channel, action))
}

fn parse_session(params: Value) -> Result<SessionId> {
    let params: SessionIdParam = serde_json::from_value(params)?;
    SessionId::parse(params.session_id).map_err(anyhow::Error::msg)
}

fn parse_optional_session(value: Option<String>) -> Result<Option<SessionId>> {
    value
        .map(SessionId::parse)
        .transpose()
        .map_err(anyhow::Error::msg)
}

fn ensure_policy_ceiling(requested: SessionMode, parent: SessionMode) -> Result<()> {
    let rank = |mode| match mode {
        SessionMode::Watch => 0,
        SessionMode::Confirm => 1,
        SessionMode::FullAccess => 2,
    };
    anyhow::ensure!(
        rank(requested) <= rank(parent),
        "subsession policy {requested:?} exceeds parent policy {parent:?}"
    );
    Ok(())
}

fn is_content_event(payload: &SessionEventPayload) -> bool {
    matches!(
        payload,
        SessionEventPayload::UserPromptSubmitted { .. }
            | SessionEventPayload::AssistantCompleted { .. }
            | SessionEventPayload::ToolStarted { .. }
            | SessionEventPayload::ToolCompleted { .. }
            | SessionEventPayload::PermissionRequested { .. }
            | SessionEventPayload::PermissionResolved { .. }
            | SessionEventPayload::TurnCancelled { .. }
            | SessionEventPayload::TurnFailed { .. }
    )
}

pub fn profile_root(config_path: &Path) -> Result<PathBuf> {
    let path = if config_path.is_dir() {
        config_path.to_path_buf()
    } else {
        config_path
            .parent()
            .context("config path has no parent")?
            .to_path_buf()
    };
    std::fs::canonicalize(&path).with_context(|| format!("resolve profile root {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_channel_actions_share_one_rpc_route() {
        for channel in ChannelKind::ALL {
            let method = |action| format!("channel.{}.{action}", channel.as_str());
            assert!(matches!(
                managed_channel_action(&method("status")),
                Some((found, ManagedChannelAction::Status)) if found == channel
            ));
            assert!(matches!(
                managed_channel_action(&method("send_message")),
                Some((found, ManagedChannelAction::SendMessage)) if found == channel
            ));
            assert!(matches!(
                managed_channel_action(&method("send_file")),
                Some((found, ManagedChannelAction::SendFile)) if found == channel
            ));
            assert!(matches!(
                managed_channel_action(&method("remove")),
                Some((found, ManagedChannelAction::Remove)) if found == channel
            ));
            assert!(managed_channel_action(&method("begin")).is_none());
        }
    }

    #[test]
    fn subsession_policy_cannot_exceed_parent() {
        assert!(ensure_policy_ceiling(SessionMode::Watch, SessionMode::Confirm).is_ok());
        assert!(ensure_policy_ceiling(SessionMode::Confirm, SessionMode::Confirm).is_ok());
        assert!(ensure_policy_ceiling(SessionMode::FullAccess, SessionMode::Confirm).is_err());
        assert!(ensure_policy_ceiling(SessionMode::Confirm, SessionMode::Watch).is_err());
    }

    fn yaml_keys(value: &serde_yaml::Value) -> Vec<&str> {
        let mut keys = value
            .as_mapping()
            .expect("profile section must be a mapping")
            .keys()
            .map(|key| key.as_str().expect("profile keys must be strings"))
            .collect::<Vec<_>>();
        keys.sort_unstable();
        keys
    }

    #[tokio::test]
    async fn complete_profile_template_loads_every_host_section() {
        let source = include_str!("../../../dwo-agent-service/profile.full.yaml");
        let document: serde_yaml::Value = serde_yaml::from_str(source).unwrap();
        assert_eq!(
            yaml_keys(&document),
            [
                "automation",
                "channels",
                "description",
                "logging",
                "model",
                "name",
                "policyMode",
            ]
        );
        assert_eq!(yaml_keys(&document["logging"]), ["level", "retentionDays"]);
        assert_eq!(
            yaml_keys(&document["channels"]["weixin"]),
            ["enabled", "markdownFilter", "mediaInput", "replayTurns"]
        );
        assert_eq!(
            yaml_keys(&document["channels"]["telegram"]),
            [
                "botTokenEnv",
                "enabled",
                "mediaInput",
                "replayTurns",
                "tgProxy"
            ]
        );
        assert_eq!(
            yaml_keys(&document["channels"]["feishu"]),
            [
                "appIdEnv",
                "appSecretEnv",
                "enabled",
                "mediaInput",
                "platform",
                "replayTurns"
            ]
        );
        assert_eq!(yaml_keys(&document["automation"]), ["enabled", "jobs"]);
        assert_eq!(
            yaml_keys(&document["automation"]["jobs"][0]),
            ["enabled", "name", "prompt", "schedule", "session"]
        );
        assert_eq!(
            yaml_keys(&document["automation"]["jobs"][0]["schedule"]),
            ["cron", "timezone"]
        );
        assert_eq!(
            yaml_keys(&document["automation"]["jobs"][0]["session"]),
            ["cwd", "mode", "title"]
        );
        assert_eq!(
            yaml_keys(&document["automation"]["jobs"][1]["session"]),
            ["mode", "sessionId"]
        );
        assert_eq!(
            yaml_keys(&document["model"]),
            ["defaultModelId", "models", "providers"]
        );
        assert_eq!(
            yaml_keys(&document["model"]["providers"]["deepseek"]),
            ["apiKey", "apiKeyEnv", "baseUrl", "type"]
        );
        assert_eq!(
            yaml_keys(&document["model"]["models"][0]),
            [
                "compactThreshold",
                "contextWindowTokens",
                "defaultReasoningMode",
                "maxOutputTokens",
                "modelId",
                "modelName",
                "provider",
            ]
        );

        let profile = dwo_agent_service::AgentProfileConfig::from_yaml(source).unwrap();
        let models = profile
            .resolve_models(&dwo_agent_service::ModelCatalog::builtin().unwrap())
            .unwrap();
        assert_eq!(models.models.len(), 2);

        let root = tempfile::tempdir().unwrap();
        let channels = ChannelManager::new(root.path(), &profile.channels)
            .await
            .unwrap();
        let channel = channels.list().await.unwrap();
        assert_eq!(channel.len(), 4);
        assert_eq!(channel[0].name, "weixin");
        assert!(!channel[0].enabled);
        assert_eq!(channel[1].name, "telegram");
        assert!(!channel[1].enabled);
        assert_eq!(channel[2].name, "feishu");
        assert!(!channel[2].enabled);
        assert_eq!(channel[3].name, "websocket");
        assert!(!channel[3].enabled);

        let automation = parse_automation_config(profile.automation).unwrap();
        assert!(!automation.enabled);
        assert_eq!(automation.jobs.len(), 2);
    }

    fn write_test_profile(root: &Path) -> PathBuf {
        std::fs::create_dir_all(root.join("resource/prompts")).unwrap();
        std::fs::write(
            root.join("resource/prompts/System.md"),
            "You are a test agent.",
        )
        .unwrap();
        let config = root.join("profile.yaml");
        std::fs::write(
            &config,
            r#"name: test
description: test agent
policyMode: confirm
model:
  defaultModelId: deepseek-v4-pro
  providers:
    deepseek:
      type: deepseek
  models:
    - modelName: deepseek-v4-pro
      provider: deepseek
      modelId: deepseek-v4-pro
"#,
        )
        .unwrap();
        config
    }

    #[tokio::test]
    async fn sessions_use_runtime_workspaces_unless_cwd_is_explicit() {
        let profile = tempfile::tempdir().unwrap();
        let config = write_test_profile(profile.path());
        let host = Host::load(&config).await.unwrap();

        let generated = host.create_session(None, None).await.unwrap();
        let generated_id = generated.id().clone();
        let generated_cwd = generated
            .attach(EndpointId::new())
            .await
            .unwrap()
            .snapshot
            .record
            .info
            .cwd;
        assert_eq!(
            generated_cwd,
            std::fs::canonicalize(
                profile
                    .path()
                    .join("runtime/workspaces")
                    .join(generated_id.as_str())
            )
            .unwrap()
        );
        assert!(generated_cwd.is_dir());

        let explicit = profile.path().join("projects/demo");
        std::fs::create_dir_all(&explicit).unwrap();
        let custom = host
            .create_session(None, Some(PathBuf::from("projects/demo")))
            .await
            .unwrap();
        let custom_id = custom.id().clone();
        let custom_cwd = custom
            .attach(EndpointId::new())
            .await
            .unwrap()
            .snapshot
            .record
            .info
            .cwd;
        assert_eq!(custom_cwd, std::fs::canonicalize(&explicit).unwrap());

        for date in ["2026/07/15", "2026/07/16"] {
            let attachment = profile
                .path()
                .join("runtime/attachments/weixin")
                .join(date)
                .join(generated_id.as_str())
                .join("image.jpg");
            std::fs::create_dir_all(attachment.parent().unwrap()).unwrap();
            std::fs::write(attachment, b"image").unwrap();
        }

        host.delete_session(&generated_id).await.unwrap();
        assert!(!generated_cwd.exists());
        assert!(
            !profile
                .path()
                .join("runtime/attachments/weixin/2026/07/15")
                .join(generated_id.as_str())
                .exists()
        );
        host.delete_session(&custom_id).await.unwrap();
        assert!(explicit.is_dir(), "an explicit cwd must never be deleted");

        host.shutdown.cancel();
        host.service.shutdown().await;
    }
}
