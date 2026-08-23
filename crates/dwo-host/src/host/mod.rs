use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use dwo_agent_service::{
    AgentService, EndpointId, FsSessionRepository, LoadedAgentProfile, NewSession,
    NotificationLevel, SessionConfig, SessionConfigUpdate, SessionEventPayload, SessionId,
    SessionLlmSettings, SessionRecord, SessionSubscription, TurnId,
};
use dwo_context::{MessageContent, RuleSource};
use dwo_mcp::McpRuntime;
use dwo_project::{CreateProject, Project, ProjectService};
use dwo_tools::{ConfirmationDecision, PolicyConfig, SessionMode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::automation::{AutomationRuntime, parse_config as parse_automation_config};
use dwo_channels::{ChannelGateway, ChannelKind, ChannelManager};
mod automation_api;
mod channel_api;
mod channel_host;
mod config_api;
mod config_manager;
pub mod events;
pub(crate) mod management_api;
mod mcp_api;
mod model_api;
mod project_api;
mod prompt_api;
mod session_api;
mod skill_api;
mod websocket_api;
use channel_api::{ManagedChannelAction, managed_channel_action};
use config_manager::ConfigManager;
use events::{EventReadResult, HostEventHub};
pub use websocket_api::WebsocketRuntime;

pub struct Host {
    service: Arc<AgentService>,
    pub channel_gateway: Arc<ChannelGateway>,
    pub mcp: Arc<McpRuntime>,
    pub automation: Arc<AutomationRuntime>,
    projects: Arc<ProjectService>,
    channels: RwLock<Arc<ChannelManager>>,
    profile_root: PathBuf,
    config_manager: ConfigManager,
    profile: RwLock<RuntimeProfile>,
    profile_reload: tokio::sync::Mutex<()>,
    shutdown: CancellationToken,
    events: Arc<HostEventHub>,
    request_cache: tokio::sync::Mutex<HashMap<String, CachedRequest>>,
    websocket_running: AtomicBool,
}

struct CachedRequest {
    method: String,
    params: Value,
    result: Value,
    expires_at: std::time::Instant,
}

struct RuntimeProfile {
    source: String,
    config: dwo_agent_service::AgentProfileConfig,
    model_options: Vec<SessionModelOption>,
}

#[derive(Deserialize)]
struct SessionIdParam {
    session_id: String,
}

#[derive(Deserialize)]
struct NewSessionParam {
    title: Option<String>,
    cwd: Option<PathBuf>,
    project_id: Option<String>,
    topic_id: Option<String>,
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
    from_session_id: Option<String>,
    caller_session_id: Option<String>,
    endpoint_id: String,
    message: PromptMessage,
    title: Option<String>,
    cwd: Option<PathBuf>,
    policy: Option<SessionMode>,
    model: Option<String>,
    reasoning: Option<String>,
    #[serde(default)]
    ephemeral: bool,
}

#[derive(Deserialize)]
struct SessionCommandParam {
    session_id: String,
    endpoint_id: String,
}

#[derive(Deserialize)]
struct NotificationParam {
    session_id: String,
    endpoint_id: Option<String>,
    category: String,
    level: NotificationLevel,
    text: String,
    #[serde(default)]
    data: Value,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PromptMessage {
    Text(String),
    Content(MessageContent),
}

impl PromptMessage {
    fn into_content(self) -> MessageContent {
        match self {
            Self::Text(text) => MessageContent::text(text),
            Self::Content(content) => content,
        }
    }
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
#[serde(rename_all = "camelCase")]
struct EventReadParam {
    cursor: Option<u64>,
    #[serde(default = "default_event_limit")]
    limit: usize,
    event: Option<String>,
}

fn default_event_limit() -> usize {
    50
}

#[derive(Deserialize)]
struct CancelParam {
    session_id: String,
    turn_id: Option<String>,
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
struct SendMessageParam {
    text: String,
}

#[derive(Deserialize)]
struct SendFileParam {
    path: PathBuf,
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
struct McpServerParam {
    server: String,
}

#[derive(Deserialize)]
pub(crate) struct McpInstallParam {
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    config: Option<Value>,
    #[serde(default)]
    servers: Option<serde_json::Map<String, Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfigUpdateParam {
    #[serde(default)]
    max_model_steps: Option<usize>,
    #[serde(default)]
    logging: Option<dwo_agent_service::LoggingConfig>,
    #[serde(default)]
    external_skills_dirs: Option<Vec<PathBuf>>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionModelOption {
    id: String,
    name: String,
    provider: String,
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
pub(crate) struct ConfigSnapshot {
    policy: SessionMode,
    default_model: String,
    default_reasoning: Option<String>,
    models: Vec<SessionModelOption>,
    max_model_steps: usize,
    session_count: usize,
}

impl Host {
    pub async fn load(config_path: &Path) -> Result<Arc<Self>> {
        let profile_root = profile_root(config_path)?;
        let config_manager = ConfigManager::new(profile_root.clone());
        let mcp = Arc::new(McpRuntime::new(&profile_root));
        mcp.sync_and_start().await?;
        tracing::info!(event = "mcp.synchronized", "MCP configuration synchronized");
        let profile = LoadedAgentProfile::load(&profile_root)?;
        let source = config_manager.fingerprint()?;
        let default_model = profile.models.default_model.clone();
        let default_reasoning = profile.models.default_reasoning.clone();
        let default_mode = profile.config.policy_mode;
        let profile_config = profile.config.clone();
        let model_options = profile
            .models
            .models
            .iter()
            .map(|(id, model)| SessionModelOption {
                id: id.clone(),
                name: model.model_name.clone(),
                provider: model.provider.clone(),
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
        let projects = Arc::new(ProjectService::open(profile_root.join("runtime/projects"))?);
        let automation = AutomationRuntime::new(
            service.clone(),
            projects.clone(),
            profile_root.clone(),
            automation_config,
            default_model.clone(),
            default_reasoning,
            default_mode,
            shutdown.clone(),
        )?;
        let host = Arc::new(Self {
            service,
            channel_gateway: Arc::new(ChannelGateway::new()),
            mcp,
            automation,
            projects,
            channels: RwLock::new(channels),
            profile_root: profile_root.clone(),
            config_manager,
            profile: RwLock::new(RuntimeProfile {
                source,
                config: profile_config,
                model_options,
            }),
            profile_reload: tokio::sync::Mutex::new(()),
            shutdown,
            events: Arc::new(HostEventHub::new()),
            request_cache: tokio::sync::Mutex::new(HashMap::new()),
            websocket_running: AtomicBool::new(false),
        });
        host.restore_ephemeral_cleanup().await;
        host.channel_gateway.start_all(host.clone()).await;
        host.start_mcp_watcher();
        host.start_profile_watcher();
        host.automation.start();
        Ok(host)
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    pub async fn shutdown(&self) {
        let ephemeral = self
            .service
            .list()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|record| record.info.ephemeral)
            .map(|record| record.info.id)
            .collect::<Vec<_>>();
        tokio::join!(
            self.channel_gateway.stop_all(),
            self.mcp.shutdown(),
            self.service.shutdown()
        );
        for id in ephemeral {
            let _ = delete_session_resources(&self.service, &self.profile_root, &id).await;
            let _ = self.projects.unassign_session_everywhere(id.as_str());
        }
    }

    pub async fn handle_method(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        if method.starts_with("project.") {
            return self.dispatch_project(method, params).await;
        }
        if method.starts_with("websocket.") {
            return self.dispatch_websocket(method, params).await;
        }
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
        if method.starts_with("skill.") {
            return self.dispatch_skill(method, params).await;
        }
        if method.starts_with("model.") || method.starts_with("provider.") {
            return self.dispatch_model(method, params).await;
        }
        if method.starts_with("prompt.") || method.starts_with("rule.") {
            return self.dispatch_prompt_resource(method, params).await;
        }
        match method {
            "dwo.capabilities" => Ok(serde_json::to_value(self.management_capabilities())?),
            "event.read" => {
                let params: EventReadParam = serde_json::from_value(params)?;
                let result: EventReadResult = self
                    .events
                    .read(params.cursor, params.limit, params.event.as_deref())
                    .await;
                Ok(serde_json::to_value(result)?)
            }
            "daemon.status" => Ok(json!({
                "healthy": true,
                "profile_root": self.profile_root,
                "sessions": self.service.list().await?.len(),
                "channels": self.channels().list().await?.len(),
                "automationJobs": self.automation.list().await.len(),
            })),
            "daemon.shutdown" => {
                self.shutdown.cancel();
                Ok(json!({"stopping": true}))
            }
            "config.snapshot" => Ok(serde_json::to_value(
                self.config_snapshot(self.service.list().await?.len()),
            )?),
            "config.update" => {
                let params: ConfigUpdateParam = serde_json::from_value(params)?;
                self.update_config(params).await?;
                Ok(json!({"updated": true}))
            }
            "session.list" => {
                let params: ListSessionParam = serde_json::from_value(params)?;
                let caller = parse_optional_session(params.caller_session_id.clone())?;
                Ok(serde_json::to_value(
                    self.list_sessions(params.all, caller.as_ref()).await?,
                )?)
            }
            "session.status-list" => {
                let params: ListSessionParam = serde_json::from_value(params)?;
                let caller = parse_optional_session(params.caller_session_id.clone())?;
                Ok(serde_json::to_value(
                    self.list_session_statuses(params.all, caller.as_ref())
                        .await?,
                )?)
            }
            "session.status" => {
                let id = parse_session(params)?;
                Ok(serde_json::to_value(self.session_status(&id).await?)?)
            }
            "session.snapshot" => {
                let id = parse_session(params)?;
                Ok(serde_json::to_value(self.session_snapshot(&id).await?)?)
            }
            "session.prompt-directives" => {
                let id = parse_session(params)?;
                self.prompt_directive_options(&id).await
            }
            "session.notify" => {
                let params: NotificationParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let origin = params
                    .endpoint_id
                    .map(EndpointId::parse)
                    .transpose()
                    .map_err(anyhow::Error::msg)?;
                let message_id = self
                    .publish_session_notification(
                        &id,
                        origin,
                        params.category,
                        params.level,
                        params.text,
                        params.data,
                    )
                    .await?;
                Ok(json!({"session_id": id, "message_id": message_id}))
            }
            "session.new" => {
                let params: NewSessionParam = serde_json::from_value(params)?;
                anyhow::ensure!(
                    params.project_id.is_none() || params.cwd.is_none(),
                    "cwd cannot be supplied with project_id"
                );
                let snapshot = if let Some(project_id) = params.project_id {
                    self.setup_project_session(
                        params.title,
                        &project_id,
                        params.topic_id.as_deref(),
                    )
                    .await?
                } else {
                    anyhow::ensure!(params.topic_id.is_none(), "topic_id requires project_id");
                    self.setup_session(params.title, params.cwd).await?
                };
                Ok(json!({
                    "session_id": snapshot.record.info.id,
                    "usage": snapshot.usage,
                }))
            }
            "session.fork" => {
                let source_id = parse_session(params)?;
                let snapshot = self.fork_session(&source_id).await?;
                let id = snapshot.record.info.id;
                let message_id = self
                    .publish_session_notification(
                        &source_id,
                        None,
                        "fork_completed".to_string(),
                        NotificationLevel::Success,
                        format!("Forked session {id}."),
                        json!({"forkedSessionId": id}),
                    )
                    .await?;
                Ok(json!({
                    "accepted": false,
                    "session_id": id.clone(),
                    "forked_session_id": id,
                    "message_id": message_id,
                    "usage": snapshot.usage,
                }))
            }
            "session.delete" => {
                let id = parse_session(params)?;
                self.delete_session(&id).await?;
                Ok(json!({"deleted": true}))
            }
            "session.keep" => {
                let id = parse_session(params)?;
                let changed = self.service.keep(&id).await?;
                Ok(json!({"session_id": id, "persistent": true, "changed": changed}))
            }
            "session.close" => {
                let id = parse_session(params)?;
                self.close_session(&id).await?;
                Ok(json!({"closed": true}))
            }
            "session.prompt" => {
                let params: PromptParam = serde_json::from_value(params)?;
                let caller = parse_optional_session(params.caller_session_id.clone())?;
                let (agent, parent_id) = self.resolve_prompt_session(&params, caller).await?;
                let endpoint = EndpointId::parse(params.endpoint_id).map_err(anyhow::Error::msg)?;
                let snapshot = agent.snapshot().await?;
                let content = self
                    .expand_prompt_directives(
                        &snapshot.record.info.cwd,
                        params.message.into_content(),
                    )
                    .await?;
                let subscription = agent.attach(EndpointId::new()).await?;
                let cleanup_subscription = if snapshot.record.info.ephemeral {
                    Some(agent.attach(EndpointId::new()).await?)
                } else {
                    None
                };
                let accepted = self.service.prompt(agent.id(), endpoint, content).await?;
                if let Some(parent_id) = parent_id {
                    self.spawn_result_delivery(
                        subscription,
                        agent.id().clone(),
                        parent_id,
                        accepted.turn_id.clone(),
                    );
                }
                if let Some(subscription) = cleanup_subscription {
                    self.spawn_ephemeral_cleanup(
                        subscription,
                        agent.id().clone(),
                        accepted.turn_id.clone(),
                    );
                }
                Ok(json!({
                    "session_id": agent.id(),
                    "message_id": accepted.message_id,
                    "turn_id": accepted.turn_id,
                }))
            }
            "session.compact" => {
                let params: SessionCommandParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let endpoint = EndpointId::parse(params.endpoint_id).map_err(anyhow::Error::msg)?;
                let accepted = self.compact_session(&id, endpoint).await?;
                Ok(json!({
                    "session_id": id,
                    "message_id": accepted.message_id,
                    "turn_id": accepted.turn_id,
                }))
            }
            "session.resume-turn" => {
                let params: SessionCommandParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let endpoint = EndpointId::parse(params.endpoint_id).map_err(anyhow::Error::msg)?;
                let accepted = self.resume_session_turn(&id, endpoint).await?;
                Ok(match accepted {
                    Some(accepted) => json!({
                        "accepted": true,
                        "session_id": id,
                        "message_id": accepted.message_id,
                        "turn_id": accepted.turn_id,
                    }),
                    None => json!({
                        "accepted": false,
                        "session_id": id,
                    }),
                })
            }
            "session.read" => {
                let params: ReadSessionParam = serde_json::from_value(params)?;
                anyhow::ensure!(
                    params.limit > 0 && params.limit <= 100,
                    "limit must be between 1 and 100"
                );
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let snapshot = self.session_snapshot(&id).await?;
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
                self.cancel_session(&id, turn).await?;
                Ok(json!({"cancelled": true}))
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
                let snapshot = self.set_session_config(&id, update).await?;
                Ok(json!({
                    "updated": true,
                    "usage": snapshot.usage,
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
                    models: self
                        .profile
                        .read()
                        .expect("profile lock poisoned")
                        .model_options
                        .clone(),
                })?)
            }
            "session.permission" => {
                let params: PermissionParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let endpoint = EndpointId::parse(params.endpoint_id).map_err(anyhow::Error::msg)?;
                self.resolve_session_permission(
                    &id,
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

    async fn dispatch_websocket(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        match method {
            "websocket.status" => self.websocket_status().await,
            "websocket.enable" => self.websocket_set_enabled(true).await,
            "websocket.disable" => self.websocket_set_enabled(false).await,
            "websocket.config" => {
                let update = params.get("config").cloned();
                self.websocket_config(update).await
            }
            "websocket.token" => self.websocket_token().await,
            "websocket.reset_token" => self.websocket_reset_token().await,
            other => anyhow::bail!("unknown RPC method: {other}"),
        }
    }

    /// Dispatch a transport request with client-scoped retry de-duplication.
    ///
    /// Only side-effecting methods are cached. A request id reused with a different method or
    /// payload is rejected instead of returning an unrelated result.
    pub async fn handle_request(
        self: &Arc<Self>,
        client_id: &str,
        request_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let cacheable = management_api::is_side_effect_method(method);
        let cache_key = format!("{client_id}:{request_id}");
        if cacheable {
            let now = std::time::Instant::now();
            let mut cache = self.request_cache.lock().await;
            cache.retain(|_, entry| entry.expires_at > now);
            if let Some(entry) = cache.get(&cache_key) {
                if entry.method != method || entry.params != params {
                    anyhow::bail!(
                        "request id {request_id} was reused with different method or params"
                    );
                }
                return Ok(entry.result.clone());
            }
        }
        let request_params = params.clone();
        let result = self.handle_method(method, params).await?;
        if cacheable {
            let mut cache = self.request_cache.lock().await;
            if cache.len() >= 1024
                && let Some(key) = cache.keys().next().cloned()
            {
                cache.remove(&key);
            }
            cache.insert(
                cache_key,
                CachedRequest {
                    method: method.to_string(),
                    params: request_params,
                    result: result.clone(),
                    expires_at: std::time::Instant::now() + std::time::Duration::from_secs(300),
                },
            );
        }
        Ok(result)
    }

    async fn dispatch_automation(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        match method {
            "automation.list" => self.automation_list().await,
            "automation.status" => {
                let params: automation_api::JobParam = serde_json::from_value(params)?;
                self.automation_status(params.job).await
            }
            "automation.update" => {
                let params: automation_api::UpdateParam = serde_json::from_value(params)?;
                self.automation_update(params).await
            }
            "automation.history" => {
                let params: automation_api::HistoryParam = serde_json::from_value(params)?;
                self.automation_history(params.job, params.limit).await
            }
            "automation.add" => {
                let params: automation_api::AddParam = serde_json::from_value(params)?;
                self.automation_add(params.job).await
            }
            "automation.enable" | "automation.disable" => {
                let params: automation_api::ToggleParam = serde_json::from_value(params)?;
                self.automation_set_enabled(params.job, params.all, method == "automation.enable")
                    .await
            }
            "automation.delete" => {
                let params: automation_api::ToggleParam = serde_json::from_value(params)?;
                self.automation_delete(params.job, params.all).await
            }
            "automation.run" => {
                let params: automation_api::RunParam = serde_json::from_value(params)?;
                self.automation_run(params.job, params.caller_session_id)
                    .await
            }
            other => anyhow::bail!("unknown RPC method: {other}"),
        }
    }

    async fn dispatch_mcp(&self, method: &str, params: Value) -> Result<Value> {
        match method {
            "mcp.list" => self.mcp_list().await,
            "mcp.config" => self.mcp_config(),
            "mcp.search" => {
                let params: McpSearchParam = serde_json::from_value(params)?;
                self.mcp_search(params.query).await
            }
            "mcp.call" => {
                let params: McpCallParam = serde_json::from_value(params)?;
                self.mcp_call(params.selector, params.arguments).await
            }
            "mcp.auth.login" => {
                let params: McpAuthParam = serde_json::from_value(params)?;
                self.mcp_auth(params.server, true).await
            }
            "mcp.auth.logout" | "mcp.auth.unauth" => {
                let params: McpAuthParam = serde_json::from_value(params)?;
                self.mcp_auth(params.server, false).await
            }
            "mcp.enable" | "mcp.disable" => {
                let params: McpServerParam = serde_json::from_value(params)?;
                self.mcp_set_enabled(params.server, method == "mcp.enable")
                    .await
            }
            "mcp.install" => {
                let params: McpInstallParam = serde_json::from_value(params)?;
                self.mcp_install(params).await
            }
            "mcp.uninstall" => {
                let params: McpServerParam = serde_json::from_value(params)?;
                self.mcp_uninstall(params.server).await
            }
            other => anyhow::bail!("unknown RPC method: {other}"),
        }
    }

    async fn dispatch_skill(&self, method: &str, params: Value) -> Result<Value> {
        let action = method.strip_prefix("skill.").unwrap_or_default();
        match action {
            "list" => self.skill_list(),
            "enable" | "disable" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .context("skill name is required")?
                    .to_string();
                self.skill_set_enabled(name, action == "enable").await
            }
            "install" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .context("skill name is required")?
                    .to_string();
                let content = params
                    .get("content")
                    .and_then(Value::as_str)
                    .context("skill content is required")?
                    .to_string();
                self.skill_install(name, content).await
            }
            "uninstall" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .context("skill name is required")?
                    .to_string();
                self.skill_uninstall(name).await
            }
            _ => anyhow::bail!("unknown skill action: {action}"),
        }
    }

    async fn dispatch_model(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        if method.starts_with("model.catalog.") {
            return self.dispatch_model_catalog(method, params).await;
        }
        let mut parts = method.split('.');
        let domain = parts.next().unwrap_or_default();
        let action = parts.next().unwrap_or_default();
        anyhow::ensure!(
            parts.next().is_none(),
            "invalid model/provider method: {method}"
        );
        match (domain, action) {
            ("model", "list") => self.model_list(),
            ("provider", "list") => self.provider_list(),
            ("model", "set_default") => {
                let default: dwo_agent_service::DefaultModelConfig =
                    serde_json::from_value(params)?;
                self.model_set_default(default).await
            }
            ("model", "upsert") => {
                let entry: dwo_agent_service::AgentModelEntry = serde_json::from_value(
                    params.get("model").cloned().context("model is required")?,
                )?;
                let provider = params
                    .get("provider")
                    .and_then(Value::as_str)
                    .context("provider is required")?
                    .to_string();
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .context("model display name is required")?
                    .to_string();
                self.model_upsert(provider, name, entry).await
            }
            ("model", "remove") => {
                let provider = params
                    .get("provider")
                    .and_then(Value::as_str)
                    .context("provider is required")?
                    .to_string();
                let model_id = params
                    .get("modelId")
                    .and_then(Value::as_str)
                    .context("modelId is required")?
                    .to_string();
                self.model_remove(provider, model_id).await
            }
            ("provider", "upsert") => {
                let provider: dwo_agent_service::AgentProviderConfig = serde_json::from_value(
                    params.get("provider").cloned().unwrap_or(params.clone()),
                )?;
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .context("provider name is required")?
                    .to_string();
                self.provider_upsert(name, provider).await
            }
            ("provider", "remove") => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .context("provider name is required")?
                    .to_string();
                self.provider_remove(name).await
            }
            _ => anyhow::bail!("unknown model/provider method: {method}"),
        }
    }

    async fn dispatch_model_catalog(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        match method {
            "model.catalog.list" => self.model_catalog_list(),
            "model.catalog.upsert" => {
                let family = params
                    .get("family")
                    .and_then(Value::as_str)
                    .context("model family is required")?
                    .to_string();
                validate_resource_name(&family)?;
                let spec: dwo_agent_service::ModelFamilySpec = serde_json::from_value(
                    params
                        .get("spec")
                        .cloned()
                        .context("model family spec is required")?,
                )?;
                self.model_catalog_upsert(family, spec).await
            }
            "model.catalog.remove" => {
                let family = params
                    .get("family")
                    .and_then(Value::as_str)
                    .context("model family is required")?
                    .to_string();
                self.model_catalog_remove(family).await
            }
            _ => anyhow::bail!("unknown model catalog method: {method}"),
        }
    }

    async fn dispatch_prompt_resource(&self, method: &str, params: Value) -> Result<Value> {
        let (domain, action) = method
            .split_once('.')
            .context("invalid prompt/rule method")?;
        match action {
            "list" => self.prompt_list().await,
            "get" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or(Self::default_prompt_name(domain)?);
                self.prompt_get(domain, name).await
            }
            "set" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or(Self::default_prompt_name(domain)?);
                let content = params
                    .get("content")
                    .and_then(Value::as_str)
                    .context("content is required")?
                    .to_string();
                self.prompt_set(domain, name, content).await
            }
            _ => anyhow::bail!("unknown prompt/rule action: {action}"),
        }
    }

    async fn dispatch_channel_binding(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        if method == "channel.list" {
            return self.channel_list().await;
        }
        let mut parts = method.split('.');
        anyhow::ensure!(
            parts.next() == Some("channel"),
            "invalid channel method: {method}"
        );
        let channel_name = parts
            .next()
            .with_context(|| format!("missing channel name in method: {method}"))?;
        let action = parts
            .next()
            .with_context(|| format!("missing channel action in method: {method}"))?;
        anyhow::ensure!(parts.next().is_none(), "invalid channel method: {method}");
        let channel = ChannelKind::parse(channel_name)
            .with_context(|| format!("unknown channel in method: {method}"))?;
        match action {
            "begin" | "bind" => self.channel_begin_bind(channel).await,
            "poll" => {
                let params: dwo_channels::ChannelPollParams = serde_json::from_value(params)?;
                self.channel_poll_bind(channel, params).await
            }
            "unbind" => self.channel_unbind(channel).await,
            other => anyhow::bail!("unknown channel action: {other}"),
        }
    }

    async fn dispatch_channel(
        self: &Arc<Self>,
        channel: ChannelKind,
        action: ManagedChannelAction,
        params: Value,
    ) -> Result<Value> {
        match action {
            ManagedChannelAction::Status => self.channel_status(channel).await,
            ManagedChannelAction::Enable | ManagedChannelAction::Disable => {
                self.channel_set_enabled(channel, matches!(action, ManagedChannelAction::Enable))
                    .await
            }
            ManagedChannelAction::Config => {
                self.channel_config(channel, params.get("config").cloned())
                    .await
            }
            ManagedChannelAction::SendMessage => {
                let params: SendMessageParam = serde_json::from_value(params)?;
                self.channel_send_message(channel, params.text).await
            }
            ManagedChannelAction::SendFile => {
                let params: SendFileParam = serde_json::from_value(params)?;
                self.channel_send_file(channel, params.path).await
            }
            ManagedChannelAction::Remove => self.channel_remove(channel).await,
        }
    }

    pub async fn create_session(
        &self,
        title: Option<String>,
        cwd: Option<PathBuf>,
    ) -> Result<Arc<dwo_agent_service::SessionAgent>> {
        let project = self.resolve_or_create_session_project(title.clone(), cwd)?;
        self.create_project_session(title, &project.id, &project.board.uncategorized_topic_id)
            .await
    }

    fn resolve_or_create_session_project(
        &self,
        title: Option<String>,
        cwd: Option<PathBuf>,
    ) -> Result<Project> {
        let Some(pwd) = cwd.map(|cwd| {
            if cwd.is_absolute() {
                cwd
            } else {
                self.profile_root.join(cwd)
            }
        }) else {
            return Ok(self.projects.create(CreateProject {
                name: title.unwrap_or_else(|| "Untitled Project".to_string()),
                pwd: None,
            })?);
        };
        let project_name = title
            .or_else(|| {
                pwd.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "Untitled Project".to_string());
        Ok(self.projects.get_or_create_by_pwd(project_name, &pwd)?)
    }

    pub async fn create_project_session(
        &self,
        title: Option<String>,
        project_id: &str,
        topic_id: &str,
    ) -> Result<Arc<dwo_agent_service::SessionAgent>> {
        let (default_model, default_reasoning, default_mode) = self.defaults();
        self.create_project_session_with(
            title,
            project_id,
            topic_id,
            None,
            default_mode,
            SessionLlmSettings::new(default_model, default_reasoning),
            false,
        )
        .await
    }

    async fn create_project_session_with(
        &self,
        title: Option<String>,
        project_id: &str,
        topic_id: &str,
        parent_session_id: Option<SessionId>,
        mode: SessionMode,
        llm: SessionLlmSettings,
        ephemeral: bool,
    ) -> Result<Arc<dwo_agent_service::SessionAgent>> {
        let project = self.projects.get(project_id)?;
        anyhow::ensure!(
            project
                .board
                .topics
                .iter()
                .any(|topic| topic.id == topic_id),
            "topic not found in project: {topic_id}"
        );
        let agents_path = self.projects.agents_path(project_id, topic_id)?;
        let session_id = SessionId::new();
        let created = self
            .service
            .create(NewSession {
                id: Some(session_id.clone()),
                parent_session_id,
                title,
                automation_job: None,
                cwd: project.pwd.clone(),
                rule_sources: vec![RuleSource::new(agents_path, project.pwd)],
                mode,
                ephemeral,
                llm,
            })
            .await;
        match created {
            Ok(session) => {
                if let Err(error) =
                    self.projects
                        .assign_session(project_id, topic_id, session_id.to_string())
                {
                    let _ = self.service.delete(&session_id).await;
                    return Err(error.into());
                }
                Ok(session)
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn resolve_prompt_session(
        &self,
        params: &PromptParam,
        caller: Option<SessionId>,
    ) -> Result<(Arc<dwo_agent_service::SessionAgent>, Option<SessionId>)> {
        anyhow::ensure!(
            params.session_id.is_none() || params.from_session_id.is_none(),
            "--from cannot be used with --to"
        );
        let (default_model, default_reasoning, default_mode) = self.defaults();
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
            anyhow::ensure!(
                !params.ephemeral,
                "--ephemeral can only be used when creating a new session"
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
            apply_prompt_config(&agent, params).await?;
            return Ok((agent, record.info.parent_session_id.clone()));
        }

        if let Some(source) = &params.from_session_id {
            anyhow::ensure!(
                params.cwd.is_none(),
                "--cwd cannot be used when forking a session"
            );
            anyhow::ensure!(
                !params.ephemeral,
                "--ephemeral can only be used when creating a new session"
            );
            let source_id = SessionId::parse(source.clone()).map_err(anyhow::Error::msg)?;
            let source_record = records
                .iter()
                .find(|record| record.info.id == source_id)
                .cloned()
                .with_context(|| format!("session not found: {source_id}"))?;
            if let Some(caller) = &caller {
                anyhow::ensure!(
                    source_record.info.parent_session_id.as_ref() == Some(caller),
                    "session {source_id} is not a direct subsession of {caller}"
                );
            }
            let mode = params.policy.unwrap_or(source_record.info.mode);
            if let Some(parent) = &caller_record {
                ensure_policy_ceiling(mode, parent.info.mode)?;
            }
            let parent_id = source_record.info.parent_session_id.clone();
            let agent = self.service.fork(&source_id, params.title.clone()).await?;
            if let Err(error) = apply_prompt_config(&agent, params).await {
                let id = agent.id().clone();
                let _ = self.service.delete(&id).await;
                return Err(error.into());
            }
            if let Some((project, topic)) = self.projects.locate_session(source_id.as_str()) {
                self.projects
                    .assign_session(&project.id, &topic.id, agent.id().to_string())?;
            }
            return Ok((agent, parent_id));
        }

        let inherited_mode = caller_record
            .as_ref()
            .map_or(default_mode, |record| record.info.mode);
        let mode = params.policy.unwrap_or(inherited_mode);
        if let Some(parent) = &caller_record {
            ensure_policy_ceiling(mode, parent.info.mode)?;
        }
        let model = params.model.clone().unwrap_or_else(|| {
            caller_record
                .as_ref()
                .map_or_else(|| default_model.clone(), |record| record.llm.model.clone())
        });
        let reasoning = params
            .reasoning
            .clone()
            .or_else(|| {
                caller_record
                    .as_ref()
                    .and_then(|record| record.llm.reasoning.clone())
            })
            .or_else(|| {
                (caller_record.is_none() && params.model.is_none())
                    .then(|| default_reasoning.clone())
                    .flatten()
            });
        let requested_cwd = params
            .cwd
            .clone()
            .or_else(|| caller_record.as_ref().map(|record| record.info.cwd.clone()));
        let inherited_topic = if params.cwd.is_none() {
            caller
                .as_ref()
                .and_then(|id| self.projects.locate_session(id.as_str()))
        } else {
            None
        };
        let (project, topic_id) = match inherited_topic {
            Some((project, topic)) => (project, topic.id),
            None => {
                let project =
                    self.resolve_or_create_session_project(params.title.clone(), requested_cwd)?;
                let topic_id = project.board.uncategorized_topic_id.clone();
                (project, topic_id)
            }
        };
        let agent = self
            .create_project_session_with(
                params.title.clone(),
                &project.id,
                &topic_id,
                caller.clone(),
                mode,
                SessionLlmSettings::new(model, reasoning),
                params.ephemeral,
            )
            .await?;
        Ok((agent, caller))
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

    fn spawn_ephemeral_cleanup(
        &self,
        mut subscription: SessionSubscription,
        child_id: SessionId,
        watched_turn: TurnId,
    ) {
        let service = self.service.clone();
        let profile_root = self.profile_root.clone();
        let projects = self.projects.clone();
        tokio::spawn(async move {
            while let Some(event) = subscription.events.recv().await {
                let terminal = match event.payload {
                    SessionEventPayload::TurnCompleted { turn_id } if turn_id == watched_turn => {
                        true
                    }
                    SessionEventPayload::TurnFailed { turn_id, .. } if turn_id == watched_turn => {
                        true
                    }
                    SessionEventPayload::TurnCancelled { turn_id } if turn_id == watched_turn => {
                        true
                    }
                    _ => false,
                };
                if terminal {
                    let Some(deadline) = service
                        .status(&child_id)
                        .await
                        .ok()
                        .and_then(|status| status.record.info.delete_after_ms)
                    else {
                        continue;
                    };
                    spawn_ephemeral_expiry(service, projects, profile_root, child_id, deadline);
                    break;
                }
            }
        });
    }

    async fn restore_ephemeral_cleanup(&self) {
        let schedule = self
            .service
            .recover_ephemeral_sessions(now_ms(), EPHEMERAL_GRACE_MS)
            .await
            .unwrap_or_default();
        for (id, deadline) in schedule {
            spawn_ephemeral_expiry(
                self.service.clone(),
                self.projects.clone(),
                self.profile_root.clone(),
                id,
                deadline,
            );
        }
    }

    pub async fn delete_session(&self, id: &SessionId) -> Result<()> {
        delete_session_resources(&self.service, &self.profile_root, id).await?;
        self.projects.unassign_session_everywhere(id.as_str())?;
        Ok(())
    }

    pub fn profile_root_path(&self) -> &Path {
        &self.profile_root
    }

    pub fn channels(&self) -> Arc<ChannelManager> {
        self.channels
            .read()
            .expect("channel manager lock poisoned")
            .clone()
    }

    fn defaults(&self) -> (String, Option<String>, SessionMode) {
        let profile = self.profile.read().expect("profile lock poisoned");
        (
            profile.config.model.default.model.clone(),
            profile.config.model.default.reasoning.clone(),
            profile.config.policy_mode,
        )
    }

    fn start_profile_watcher(self: &Arc<Self>) {
        let host = self.clone();
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = interval.tick() => {
                        if let Err(error) = host.reload_profile_if_changed().await {
                            let _ = host
                                .events
                                .publish(
                                    "config.apply_failed",
                                    json!({"source": "profile", "error": format!("{error:#}")}),
                                )
                                .await;
                            tracing::warn!(
                                event = "config.apply_failed",
                                error = %format!("{error:#}"),
                                "reload profile configuration failed; keeping previous configuration"
                            );
                        }
                    }
                }
            }
        });
    }

    async fn reload_profile_if_changed(self: &Arc<Self>) -> Result<bool> {
        self.reload_profile(false).await
    }

    async fn reload_profile(self: &Arc<Self>, force: bool) -> Result<bool> {
        let _reload = self.profile_reload.lock().await;
        let source = self.config_manager.fingerprint()?;
        if !force && self.profile.read().expect("profile lock poisoned").source == source {
            return Ok(false);
        }

        let loaded = self.config_manager.load()?;
        let automation_config = parse_automation_config(loaded.config.automation.clone())?;
        let channels_changed = self
            .profile
            .read()
            .expect("profile lock poisoned")
            .config
            .channels
            != loaded.config.channels;
        let replacement_channels = if channels_changed {
            Some(Arc::new(
                ChannelManager::new(&self.profile_root, &loaded.config.channels).await?,
            ))
        } else {
            None
        };
        let model_options = loaded
            .models
            .models
            .iter()
            .map(|(id, model)| SessionModelOption {
                id: id.clone(),
                name: model.model_name.clone(),
                provider: model.provider.clone(),
                reasoning: model.reasoning.keys().cloned().collect(),
                default_reasoning: model.default_reasoning_mode.clone(),
            })
            .collect();
        let default_model = loaded.models.default_model.clone();
        let default_reasoning = loaded.models.default_reasoning.clone();
        let default_mode = loaded.config.policy_mode;

        self.service.replace_models(loaded.models)?;
        self.service
            .replace_max_model_steps(loaded.config.max_model_steps);
        self.service
            .replace_external_skill_dirs(loaded.external_skill_dirs);
        self.automation
            .apply_profile(
                automation_config,
                default_model,
                default_reasoning,
                default_mode,
            )
            .await?;
        crate::logging::reload(&loaded.config.logging)?;

        if let Some(channels) = replacement_channels {
            self.channel_gateway.stop_all().await;
            *self
                .channels
                .write()
                .expect("channel manager lock poisoned") = channels;
            self.channel_gateway.start_all(self.clone()).await;
        }

        *self.profile.write().expect("profile lock poisoned") = RuntimeProfile {
            source,
            config: loaded.config,
            model_options,
        };
        tracing::info!(event = "profile.reloaded", "profile configuration reloaded");
        tracing::info!(
            event = "config.changed",
            source = "profile",
            "host configuration applied"
        );
        self.events
            .publish("config.changed", json!({"source": "profile"}))
            .await;
        Ok(true)
    }

    fn start_mcp_watcher(self: &Arc<Self>) {
        let host = self.clone();
        let runtime = self.mcp.clone();
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            let mut last_error: Option<String> = None;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = interval.tick() => {
                        if let Err(error) = runtime.sync_and_start().await {
                            let message = format!("{error:#}");
                            if last_error.as_deref() != Some(message.as_str()) {
                                let _ = host
                                    .events
                                    .publish(
                                        "mcp.status",
                                        json!({"status": "apply_failed", "error": message}),
                                    )
                                    .await;
                                last_error = Some(message.clone());
                            }
                            tracing::warn!(
                                event = "mcp.synchronization_failed",
                                error = %message,
                                "synchronize MCP configuration failed"
                            );
                        } else if last_error.take().is_some() {
                            let _ = host
                                .events
                                .publish("mcp.status", json!({"status": "synchronized"}))
                                .await;
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
        checkpoint_cursor: Option<usize>,
    ) -> Result<dwo_agent_service::SessionSubscription> {
        let id = SessionId::parse(session_id.to_string()).map_err(anyhow::Error::msg)?;
        let endpoint = EndpointId::parse(endpoint_id.to_string()).map_err(anyhow::Error::msg)?;
        self.subscribe_session(&id, endpoint, checkpoint_cursor)
            .await
    }

    pub async fn subscribe_events(
        &self,
        cursor: Option<u64>,
        limit: usize,
        event: Option<&str>,
    ) -> (
        EventReadResult,
        tokio::sync::broadcast::Receiver<events::HostEvent>,
    ) {
        let receiver = self.events.subscribe();
        let replay = self.events.read(cursor, limit, event).await;
        (replay, receiver)
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

async fn delete_session_resources(
    service: &AgentService,
    profile_root: &Path,
    id: &SessionId,
) -> Result<()> {
    let record = service
        .list()
        .await?
        .into_iter()
        .find(|record| &record.info.id == id);
    service.delete(id).await?;

    if let Some(record) = record {
        cleanup_deleted_session_resources(profile_root, id, &record).await?;
    }
    Ok(())
}

const EPHEMERAL_GRACE_MS: u64 = 5 * 60 * 1000;

fn spawn_ephemeral_expiry(
    service: Arc<AgentService>,
    projects: Arc<ProjectService>,
    profile_root: PathBuf,
    id: SessionId,
    deadline: u64,
) {
    tokio::spawn(async move {
        let delay = deadline.saturating_sub(now_ms());
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        let Ok(Some(record)) = service.delete_if_ephemeral_expired(&id, now_ms()).await else {
            return;
        };
        let _ = cleanup_deleted_session_resources(&profile_root, &id, &record).await;
        let _ = projects.unassign_session_everywhere(id.as_str());
    });
}

async fn cleanup_deleted_session_resources(
    profile_root: &Path,
    id: &SessionId,
    _record: &SessionRecord,
) -> Result<()> {
    for channel in ["weixin", "telegram", "feishu"] {
        remove_session_attachment_dirs(
            &profile_root.join("runtime/attachments").join(channel),
            id.as_str(),
        )
        .await?;
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
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

fn validate_resource_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty() && name != "." && name != "..",
        "resource name must not be empty"
    );
    anyhow::ensure!(
        name == Path::new(name)
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or_default(),
        "resource name must be a single path component"
    );
    anyhow::ensure!(
        !name.contains(['/', '\\']),
        "resource name must not contain path separators"
    );
    Ok(())
}

fn validate_markdown_name(name: &str) -> Result<()> {
    validate_resource_name(name)?;
    anyhow::ensure!(name.ends_with(".md"), "resource name must end with .md");
    Ok(())
}

fn read_mcp_document(path: &Path) -> Result<Value> {
    if !path.is_file() {
        return Ok(json!({"mcpServers": {}}));
    }
    let source = std::fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&source)?;
    anyhow::ensure!(
        value.get("mcpServers").and_then(Value::as_object).is_some(),
        "mcpServers must be an object"
    );
    Ok(value)
}

fn redacted_model_config(config: &dwo_agent_service::AgentModelConfig) -> Result<Value> {
    let mut value = serde_json::to_value(config)?;
    if let Some(providers) = value.get_mut("providers").and_then(Value::as_object_mut) {
        for provider in providers.values_mut() {
            if let Some(object) = provider.as_object_mut() {
                let configured = object
                    .remove("apiKey")
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .is_some_and(|value| !value.is_empty());
                object.insert("apiKeyConfigured".to_string(), Value::Bool(configured));
            }
        }
    }
    Ok(value)
}

fn redacted_mcp_config(document: &Value) -> Value {
    let servers = document
        .get("mcpServers")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|servers| servers.iter())
        .map(|(name, value)| {
            let object = value.as_object();
            json!({
                "name": name,
                "enabled": object.and_then(|v| v.get("enabled")).and_then(Value::as_bool).unwrap_or(true),
                "type": object.and_then(|v| v.get("type")).and_then(Value::as_str).unwrap_or("stdio"),
                "description": object.and_then(|v| v.get("description")).and_then(Value::as_str),
                "authConfigured": object.and_then(|v| v.get("auth")).is_some(),
                "credentialsConfigured": object.is_some_and(|v| v.contains_key("env") || v.contains_key("headers")),
            })
        })
        .collect::<Vec<_>>();
    json!({"servers": servers})
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

async fn apply_prompt_config(
    agent: &dwo_agent_service::SessionAgent,
    params: &PromptParam,
) -> std::result::Result<(), dwo_agent_service::AgentServiceError> {
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
    Ok(())
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
            | SessionEventPayload::AssistantInterrupted { .. }
            | SessionEventPayload::Notification { .. }
            | SessionEventPayload::ToolStarted { .. }
            | SessionEventPayload::ToolUpdated { .. }
            | SessionEventPayload::ToolCompleted { .. }
            | SessionEventPayload::TerminalOpened { .. }
            | SessionEventPayload::TerminalExited { .. }
            | SessionEventPayload::FileRead { .. }
            | SessionEventPayload::FileChanged { .. }
            | SessionEventPayload::PermissionRequested { .. }
            | SessionEventPayload::PermissionResolved { .. }
            | SessionEventPayload::PlanUpdated { .. }
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
    fn prompt_message_accepts_text_and_structured_content() {
        let text: PromptMessage = serde_json::from_value(json!("hello")).unwrap();
        assert_eq!(text.into_content(), MessageContent::text("hello"));

        let content: PromptMessage = serde_json::from_value(json!([
            {"type": "text", "text": "inspect"},
            {"type": "image", "mimeType": "image/png", "data": "aGVsbG8="}
        ]))
        .unwrap();
        let content = content.into_content();
        assert_eq!(content.as_blocks().len(), 2);
        assert!(content.contains_images());
    }

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

    #[tokio::test]
    async fn complete_profile_template_configures_each_host_domain() {
        let source = include_str!("../../../dwo-agent-service/profile.full.yaml");
        let profile = dwo_agent_service::AgentProfileConfig::from_yaml(source).unwrap();
        assert_eq!(profile.policy_mode, SessionMode::Confirm);
        assert_eq!(profile.max_model_steps, 100);
        assert_eq!(profile.websocket.bind, "127.0.0.1");
        assert_eq!(profile.websocket.port, 8787);
        assert!(!profile.websocket.enabled);
        let models = profile
            .resolve_models(&dwo_agent_service::ModelCatalog::builtin().unwrap())
            .unwrap();
        assert_eq!(models.models.len(), 2);

        let root = tempfile::tempdir().unwrap();
        let channels = ChannelManager::new(root.path(), &profile.channels)
            .await
            .unwrap();
        let channel = channels.list().await.unwrap();
        let channel_names = channel
            .iter()
            .map(|status| status.name.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            channel_names,
            std::collections::HashSet::from(["weixin", "telegram", "feishu", "qq"])
        );
        assert!(channel.iter().all(|status| !status.enabled));

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
            r#"policyMode: confirm
model:
  default:
    model: deepseek/deepseek-v4-pro
  providers:
    deepseek:
"#,
        )
        .unwrap();
        config
    }

    #[tokio::test]
    async fn management_capabilities_report_only_live_contracts() {
        let root = tempfile::tempdir().unwrap();
        let host = Host::load(&write_test_profile(root.path())).await.unwrap();
        let capabilities = host
            .handle_method("dwo.capabilities", json!({}))
            .await
            .unwrap();
        assert_eq!(capabilities["protocolVersion"], 3);
        assert_eq!(capabilities["route"], "dwo");
        assert_eq!(capabilities["eventCursor"], true);
        assert!(
            capabilities["methods"]
                .as_array()
                .unwrap()
                .iter()
                .any(|method| method == "mcp.auth.login")
        );
        assert!(
            capabilities["methods"]
                .as_array()
                .unwrap()
                .iter()
                .any(|method| method == "skill.install")
        );
        host.shutdown().await;
    }

    #[tokio::test]
    async fn project_board_composes_topics_sessions_labels_and_rules() {
        let root = tempfile::tempdir().unwrap();
        let config = write_test_profile(root.path());
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let host = Host::load(&config).await.unwrap();

        let project = host
            .handle_method("project.create", json!({"name": "Demo", "pwd": workspace}))
            .await
            .unwrap();
        let project_id = project["id"].as_str().unwrap();
        let section_id = project["board"]["uncategorizedSectionId"].as_str().unwrap();
        let topic = host
            .handle_method(
                "project.topic.create",
                json!({
                    "project_id": project_id,
                    "section_id": section_id,
                    "title": "Project API"
                }),
            )
            .await
            .unwrap();
        let topic_id = topic["id"].as_str().unwrap();
        host.handle_method(
            "project.topic.agents.set",
            json!({
                "project_id": project_id,
                "topic_id": topic_id,
                "content": "Keep changes inside the project API."
            }),
        )
        .await
        .unwrap();
        let label = host
            .handle_method(
                "project.label.create",
                json!({
                    "project_id": project_id,
                    "name": "Backend",
                    "color": "#388E3C"
                }),
            )
            .await
            .unwrap();
        host.handle_method(
            "project.label.assign",
            json!({
                "project_id": project_id,
                "topic_id": topic_id,
                "label_id": label["id"]
            }),
        )
        .await
        .unwrap();
        let created = host
            .handle_method(
                "session.new",
                json!({"project_id": project_id, "topic_id": topic_id}),
            )
            .await
            .unwrap();
        let session_id = created["session_id"].as_str().unwrap();

        let detail = host
            .handle_method(
                "project.topic.get",
                json!({"project_id": project_id, "topic_id": topic_id}),
            )
            .await
            .unwrap();
        assert_eq!(detail["sessions"][0]["record"]["info"]["id"], session_id);
        assert_eq!(detail["labels"][0]["name"], "Backend");
        let snapshot = host
            .service
            .load(&SessionId::parse(session_id.to_string()).unwrap())
            .await
            .unwrap()
            .snapshot()
            .await
            .unwrap();
        assert!(
            snapshot
                .record
                .context
                .system_prompt
                .content
                .contains("Keep changes inside the project API.")
        );
        assert_eq!(snapshot.record.context.rule_sources.len(), 1);
        assert_eq!(
            snapshot.record.context.rule_sources[0].pwd,
            std::fs::canonicalize(workspace).unwrap()
        );
        let second = host
            .handle_method(
                "project.topic.create",
                json!({
                    "project_id": project_id,
                    "section_id": section_id,
                    "title": "Review"
                }),
            )
            .await
            .unwrap();
        let second_id = second["id"].as_str().unwrap();
        host.handle_method(
            "project.topic.agents.set",
            json!({
                "project_id": project_id,
                "topic_id": second_id,
                "content": "Review the completed implementation."
            }),
        )
        .await
        .unwrap();
        host.handle_method(
            "project.topic.session.assign",
            json!({
                "project_id": project_id,
                "topic_id": second_id,
                "session_id": session_id
            }),
        )
        .await
        .unwrap();
        let moved = host
            .service
            .load(&SessionId::parse(session_id.to_string()).unwrap())
            .await
            .unwrap()
            .snapshot()
            .await
            .unwrap();
        assert!(moved.record.context.messages.iter().any(|message| {
            message.kind == dwo_context::MessageKind::EnvWatcher
                && message
                    .content
                    .contains("Review the completed implementation.")
        }));
        assert!(
            host.projects
                .get(project_id)
                .unwrap()
                .board
                .topics
                .iter()
                .find(|topic| topic.id == topic_id)
                .unwrap()
                .session_ids
                .is_empty()
        );
        host.handle_method(
            "project.topic.task.create",
            json!({
                "project_id": project_id,
                "topic_id": second_id,
                "job": {
                    "name": "topic-review",
                    "enabled": true,
                    "schedule": {"cron": "0 9 * * *", "timezone": "Asia/Shanghai"},
                    "session": {"mode": "new", "behavior": "every_time", "cwd": "."},
                    "prompt": "Review now"
                }
            }),
        )
        .await
        .unwrap();
        let run = host
            .handle_method(
                "automation.run",
                json!({"job": "topic-review", "caller_session_id": null}),
            )
            .await
            .unwrap();
        let automation_session_id = run["sessionId"].as_str().unwrap();
        let (_, automation_topic) = host
            .projects
            .locate_session(automation_session_id)
            .expect("topic automation session is assigned to its topic");
        assert_eq!(automation_topic.id, second_id);
        let automation_snapshot = host
            .service
            .load(&SessionId::parse(automation_session_id.to_string()).unwrap())
            .await
            .unwrap()
            .snapshot()
            .await
            .unwrap();
        assert!(
            automation_snapshot
                .record
                .context
                .system_prompt
                .content
                .contains("Review the completed implementation.")
        );
        host.shutdown().await;
    }

    #[tokio::test]
    async fn prompt_directives_use_the_effective_session_skill_catalog() {
        let root = tempfile::tempdir().unwrap();
        let config = write_test_profile(root.path());
        let profile_skill = root.path().join("resource/skills/shared");
        std::fs::create_dir_all(&profile_skill).unwrap();
        std::fs::write(
            profile_skill.join("SKILL.md"),
            "---\nname: shared\ndescription: profile version\n---\nProfile instructions",
        )
        .unwrap();
        let project = root.path().join("project");
        let project_skill = project.join(".agents/skills/shared");
        std::fs::create_dir_all(&project_skill).unwrap();
        std::fs::write(
            project_skill.join("SKILL.md"),
            "---\nname: shared\ndescription: project version\n---\nProject instructions",
        )
        .unwrap();

        let host = Host::load(&config).await.unwrap();
        let expanded = host
            .expand_prompt_directives(
                &project,
                MessageContent::text(
                    "use /skill shared now; keep /skill missing and bare /mcp unchanged",
                ),
            )
            .await
            .unwrap();
        let text = expanded.as_text().unwrap();
        let expected_path = std::fs::canonicalize(project_skill.join("SKILL.md")).unwrap();
        assert!(text.contains(&expected_path.display().to_string()));
        assert!(!text.contains(&profile_skill.display().to_string()));
        assert!(text.contains("/skill missing"));
        assert!(text.contains("bare /mcp unchanged"));

        let session = host.setup_session(None, Some(project)).await.unwrap();
        let options = host
            .prompt_directive_options(&session.record.info.id)
            .await
            .unwrap();
        assert_eq!(options["skills"][0]["name"], "shared");
        assert_eq!(options["skills"][0]["description"], "project version");
        host.shutdown().await;
    }

    #[tokio::test]
    async fn prompt_from_forks_a_direct_child_and_rejects_to() {
        let root = tempfile::tempdir().unwrap();
        let host = Host::load(&write_test_profile(root.path())).await.unwrap();
        let parent = host
            .create_session(Some("parent".to_string()), None)
            .await
            .unwrap();
        let create = PromptParam {
            session_id: None,
            from_session_id: None,
            caller_session_id: None,
            endpoint_id: "test".to_string(),
            message: PromptMessage::Text("unused".to_string()),
            title: Some("child".to_string()),
            cwd: None,
            policy: None,
            model: None,
            reasoning: None,
            ephemeral: false,
        };
        let (source, _) = host
            .resolve_prompt_session(&create, Some(parent.id().clone()))
            .await
            .unwrap();
        let source_snapshot = source.snapshot().await.unwrap();
        let fork = PromptParam {
            from_session_id: Some(source.id().to_string()),
            title: Some("forked child".to_string()),
            ..create
        };

        let (forked, parent_id) = host
            .resolve_prompt_session(&fork, Some(parent.id().clone()))
            .await
            .unwrap();
        let forked_snapshot = forked.snapshot().await.unwrap();

        assert_ne!(forked.id(), source.id());
        assert_eq!(parent_id.as_ref(), Some(parent.id()));
        assert_eq!(
            forked_snapshot.record.info.parent_session_id.as_ref(),
            Some(parent.id())
        );
        assert_eq!(forked_snapshot.record.info.title, "forked child");
        assert_eq!(
            forked_snapshot.record.context,
            source_snapshot.record.context
        );

        let slash_fork = host
            .handle_method(
                "session.fork",
                json!({"session_id": source.id().to_string()}),
            )
            .await
            .unwrap();
        assert_eq!(slash_fork["accepted"], false);
        assert_ne!(slash_fork["session_id"], source.id().as_str());

        let invalid = PromptParam {
            session_id: Some(source.id().to_string()),
            from_session_id: Some(source.id().to_string()),
            ..fork
        };
        let error = host
            .resolve_prompt_session(&invalid, Some(parent.id().clone()))
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "--from cannot be used with --to");
        host.shutdown().await;
    }

    #[tokio::test]
    async fn profile_yaml_reloads_all_host_configuration_atomically() {
        let root = tempfile::tempdir().unwrap();
        let config = write_test_profile(root.path());
        let host = Host::load(&config).await.unwrap();
        let existing = host.create_session(None, None).await.unwrap();
        std::fs::write(
            &config,
            r#"policyMode: watch
maxModelSteps: 17
logging:
  level: debug
  retentionDays: 7
model:
  default:
    model: deepseek/deepseek-v4-flash
  providers:
    deepseek:
websocket:
  enabled: false
  bind: 127.0.0.1
  port: 19000
automation:
  enabled: false
  jobs: []
"#,
        )
        .unwrap();

        assert!(host.reload_profile_if_changed().await.unwrap());
        let snapshot = host
            .handle_method("config.snapshot", json!({}))
            .await
            .unwrap();
        assert_eq!(snapshot["policy"], "watch");
        assert_eq!(snapshot["defaultModel"], "deepseek/deepseek-v4-flash");
        assert_eq!(snapshot["models"].as_array().unwrap().len(), 2);
        assert!(host.channels().list().await.unwrap().is_empty());
        existing
            .set_config(SessionConfigUpdate::Model(
                "deepseek/deepseek-v4-flash".to_string(),
            ))
            .await
            .unwrap();

        let session = host.create_session(None, None).await.unwrap();
        let record = session
            .attach(EndpointId::new())
            .await
            .unwrap()
            .snapshot
            .record;
        assert_eq!(record.info.mode, SessionMode::Watch);
        assert_eq!(record.llm.model, "deepseek/deepseek-v4-flash");
        assert_eq!(host.service.max_model_steps(), 17);

        let invalid = std::fs::read_to_string(&config).unwrap().replacen(
            "policyMode: watch",
            "policyMode: invalid",
            1,
        );
        std::fs::write(&config, invalid).unwrap();
        assert!(host.reload_profile_if_changed().await.is_err());
        let snapshot = host
            .handle_method("config.snapshot", json!({}))
            .await
            .unwrap();
        assert_eq!(snapshot["defaultModel"], "deepseek/deepseek-v4-flash");

        host.shutdown().await;
    }

    #[tokio::test]
    async fn automation_crud_updates_profile_and_runtime_together() {
        let root = tempfile::tempdir().unwrap();
        let config = write_test_profile(root.path());
        let host = Host::load(&config).await.unwrap();
        let job = json!({
            "name": "daily-report",
            "enabled": true,
            "schedule": {"cron": "0 9 * * *", "timezone": "Asia/Shanghai"},
            "session": {"mode": "new", "behavior": "every_time", "cwd": "."},
            "prompt": "summarize the project"
        });

        let added = host
            .handle_method("automation.add", json!({"job": job}))
            .await
            .unwrap();
        assert_eq!(added["job"]["name"], "daily-report");
        assert_eq!(host.automation.list().await.len(), 1);

        host.handle_method(
            "automation.disable",
            json!({"job": "daily-report", "all": false}),
        )
        .await
        .unwrap();
        assert!(
            !host
                .automation
                .status("daily-report")
                .await
                .unwrap()
                .job
                .enabled
        );

        host.handle_method("automation.enable", json!({"job": null, "all": true}))
            .await
            .unwrap();
        assert!(
            host.automation
                .status("daily-report")
                .await
                .unwrap()
                .job
                .enabled
        );

        host.handle_method(
            "automation.delete",
            json!({"job": "daily-report", "all": false}),
        )
        .await
        .unwrap();
        assert!(host.automation.list().await.is_empty());
        let profile = dwo_agent_service::AgentProfileConfig::load(root.path()).unwrap();
        assert!(
            parse_automation_config(profile.automation)
                .unwrap()
                .jobs
                .is_empty()
        );

        host.shutdown().await;
    }

    #[tokio::test]
    async fn management_domains_mutate_through_host_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let config = write_test_profile(root.path());
        let host = Host::load(&config).await.unwrap();

        host.handle_method(
            "skill.install",
            json!({"name": "review", "content": "---\nname: review\ndescription: Review changes\n---\nRead the diff."}),
        )
        .await
        .unwrap();
        let listed = host.handle_method("skill.list", json!({})).await.unwrap();
        assert_eq!(listed["skills"][0]["name"], "review");
        host.handle_method("skill.disable", json!({"name": "review"}))
            .await
            .unwrap();
        let listed = host.handle_method("skill.list", json!({})).await.unwrap();
        assert_eq!(listed["disabled"][0], "review");
        host.handle_method("skill.enable", json!({"name": "review"}))
            .await
            .unwrap();
        host.handle_method("skill.uninstall", json!({"name": "review"}))
            .await
            .unwrap();

        host.handle_method(
            "prompt.set",
            json!({"name": "System.md", "content": "Updated system prompt."}),
        )
        .await
        .unwrap();
        let prompt = host
            .handle_method("prompt.get", json!({"name": "System.md"}))
            .await
            .unwrap();
        assert_eq!(prompt["content"], "Updated system prompt.");

        host.handle_method(
            "provider.upsert",
            json!({
                "name": "private",
                "provider": {
                    "baseUrl": "https://private.example.com/v1",
                    "apiKey": "secret",
                    "models": {
                        "Private GPT": {
                            "modelId": "private-gpt",
                            "profile": "openai/gpt-5.6-terra"
                        }
                    }
                }
            }),
        )
        .await
        .unwrap();
        let providers = host
            .handle_method("provider.list", json!({}))
            .await
            .unwrap();
        assert_eq!(providers["private"]["apiKeyConfigured"], true);
        assert!(providers["private"].get("apiKey").is_none());

        host.handle_method(
            "model.upsert",
            json!({
                "provider": "private",
                "name": "Private Grok",
                "model": {
                    "modelId": "private-grok",
                    "profile": "grok/grok-4.6"
                }
            }),
        )
        .await
        .unwrap();
        host.handle_method(
            "model.set_default",
            json!({"model": "private/private-grok", "reasoning": "High"}),
        )
        .await
        .unwrap();
        let session = host
            .create_session(Some("model default".to_string()), None)
            .await
            .unwrap();
        let record = session.snapshot().await.unwrap().record;
        assert_eq!(record.llm.model, "private/private-grok");
        assert_eq!(record.llm.reasoning.as_deref(), Some("High"));

        host.handle_method(
            "model.catalog.upsert",
            json!({
                "family": "minimax",
                "spec": {
                    "models": {
                        "minimax-m2.5": {
                            "contextWindowTokens": 200000,
                            "maxOutputTokens": 32000
                        }
                    }
                }
            }),
        )
        .await
        .unwrap();
        let catalog = host
            .handle_method("model.catalog.list", json!({}))
            .await
            .unwrap();
        assert_eq!(
            catalog["families"]["minimax"]["models"]["minimax-m2.5"]["maxOutputTokens"],
            32000
        );
        host.handle_method("model.catalog.remove", json!({"family": "minimax"}))
            .await
            .unwrap();

        host.handle_method(
            "mcp.install",
            json!({"server": "disabled-tool", "config": {"command": "missing-dwo-mcp"}}),
        )
        .await
        .unwrap();
        host.handle_method("mcp.disable", json!({"server": "disabled-tool"}))
            .await
            .unwrap();
        let mcp = host.handle_method("mcp.config", json!({})).await.unwrap();
        assert_eq!(mcp["servers"][0]["enabled"], false);

        host.handle_method("config.update", json!({"maxModelSteps": 17}))
            .await
            .unwrap();
        let config = host
            .handle_method("config.snapshot", json!({}))
            .await
            .unwrap();
        assert_eq!(config["maxModelSteps"], 17);
        host.shutdown().await;
    }

    #[tokio::test]
    async fn transport_request_ids_deduplicate_side_effects() {
        let root = tempfile::tempdir().unwrap();
        let host = Host::load(&write_test_profile(root.path())).await.unwrap();
        let first = host
            .handle_request(
                "client-a",
                "retry-1",
                "session.new",
                json!({"title": "one"}),
            )
            .await
            .unwrap();
        let second = host
            .handle_request(
                "client-a",
                "retry-1",
                "session.new",
                json!({"title": "one"}),
            )
            .await
            .unwrap();
        assert_eq!(first["session_id"], second["session_id"]);
        assert_eq!(host.service.list().await.unwrap().len(), 1);

        let different_client = host
            .handle_request(
                "client-b",
                "retry-1",
                "session.new",
                json!({"title": "two"}),
            )
            .await
            .unwrap();
        assert_ne!(first["session_id"], different_client["session_id"]);

        let reused = host
            .handle_request(
                "client-a",
                "retry-1",
                "session.new",
                json!({"title": "different"}),
            )
            .await;
        assert!(reused.is_err());
        assert_eq!(host.service.list().await.unwrap().len(), 2);
        host.shutdown().await;
    }

    #[tokio::test]
    async fn transport_request_cache_stays_bounded_under_client_load() {
        let root = tempfile::tempdir().unwrap();
        let host = Host::load(&write_test_profile(root.path())).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for index in 0..2048 {
                host.handle_request(
                    "load-client",
                    &format!("request-{index}"),
                    "daemon.shutdown",
                    json!({}),
                )
                .await
                .unwrap();
            }
        })
        .await
        .expect("request cache load took more than five seconds");

        assert_eq!(host.request_cache.lock().await.len(), 1024);
        host.shutdown().await;
    }

    #[tokio::test]
    async fn sessions_use_project_workspaces_and_topic_rule_sources() {
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
        let (generated_project, generated_topic) = host
            .projects
            .locate_session(generated_id.as_str())
            .expect("generated session belongs to the uncategorized topic");
        assert_eq!(generated_cwd, generated_project.pwd);
        assert_eq!(
            generated_topic.id,
            generated_project.board.uncategorized_topic_id
        );
        assert!(generated_cwd.is_dir());
        let snapshot = generated.snapshot().await.unwrap();
        assert_eq!(snapshot.record.context.rule_sources.len(), 1);
        assert_eq!(
            snapshot.record.context.rule_sources[0].pwd,
            generated_project.pwd
        );
        assert_eq!(
            snapshot.record.context.rule_sources[0].path,
            host.projects
                .agents_path(&generated_project.id, &generated_topic.id)
                .unwrap()
        );

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
        let second_custom = host
            .create_session(
                Some("second".to_string()),
                Some(PathBuf::from("projects/demo")),
            )
            .await
            .unwrap();
        let (custom_project, custom_topic) =
            host.projects.locate_session(custom_id.as_str()).unwrap();
        let (second_project, second_topic) = host
            .projects
            .locate_session(second_custom.id().as_str())
            .unwrap();
        assert_eq!(custom_project.id, second_project.id);
        assert_eq!(custom_topic.id, second_topic.id);
        assert_eq!(second_topic.session_ids.len(), 2);
        assert_eq!(host.projects.list().len(), 2);

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
        assert!(
            generated_cwd.exists(),
            "the workspace belongs to Project, not Session"
        );
        assert!(
            host.projects
                .locate_session(generated_id.as_str())
                .is_none()
        );
        assert!(
            !profile
                .path()
                .join("runtime/attachments/weixin/2026/07/15")
                .join(generated_id.as_str())
                .exists()
        );
        host.delete_session(&custom_id).await.unwrap();
        host.delete_session(second_custom.id()).await.unwrap();
        assert!(explicit.is_dir(), "an explicit cwd must never be deleted");

        host.shutdown.cancel();
        host.service.shutdown().await;
    }

    #[tokio::test]
    async fn automation_run_returns_after_the_run_is_queued() {
        let profile = tempfile::tempdir().unwrap();
        let config = write_test_profile(profile.path());
        let mut source = std::fs::read_to_string(&config).unwrap();
        source.push_str(
            r#"
automation:
  enabled: false
  jobs:
    - name: background-failure
      schedule: { cron: "0 9 * * *", timezone: Asia/Shanghai }
      session: { mode: new, behavior: every_time, cwd: definitely-missing }
      prompt: this must be submitted in the background
    - name: valid-start
      schedule: { cron: "0 9 * * *", timezone: Asia/Shanghai }
      session: { mode: new, behavior: every_time, cwd: . }
      prompt: this starts before the command returns
"#,
        );
        std::fs::write(&config, source).unwrap();
        let host = Host::load(&config).await.unwrap();

        let error = host
            .handle_method(
                "automation.run",
                json!({"job": "background-failure", "caller_session_id": null}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cannot find the file"));

        let value = host
            .handle_method(
                "automation.run",
                json!({"job": "valid-start", "caller_session_id": null}),
            )
            .await
            .unwrap();
        let record: crate::automation::AutomationRunRecord = serde_json::from_value(value).unwrap();
        assert_eq!(
            record.status,
            crate::automation::AutomationRunStatus::Queued
        );
        assert!(record.run_id.starts_with("run-"));
        assert!(record.session_id.is_some());
        assert!(record.turn_id.is_none());

        host.shutdown.cancel();
        host.service.shutdown().await;
    }
}
