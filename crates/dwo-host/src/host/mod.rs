use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use dwo_agent_service::{
    FsSessionRepository, LoadedAgentProfile, SessionConfig, SessionId, SessionLlmSettings,
    SessionService,
};
use dwo_context::ExternalRuleFile;
use dwo_mcp::McpRuntime;
use dwo_project::ProjectService;
use dwo_protocol::ReasoningOption;
use dwo_tools::{PolicyConfig, SessionMode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::automation::AutomationRuntime;
use dwo_channels::{ChannelGateway, ChannelManager};
mod automation_api;
mod channel_api;
mod channel_host;
mod config_api;
mod config_manager;
pub mod events;
mod git;
mod mcp_api;
mod model_api;
mod project_api;
mod prompt_api;
mod session_api;
mod skill_api;
mod websocket_api;
use config_manager::ConfigManager;
use events::{EventReadResult, HostEventHub};
pub use websocket_api::WebsocketRuntime;

pub struct Host {
    service: Arc<SessionService>,
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

#[derive(Default)]
pub struct HostSessionOptions {
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
    pub project_id: Option<String>,
    pub topic_id: Option<String>,
    pub worktree_id: Option<String>,
    pub from: Option<SessionId>,
    pub parent_session_id: Option<SessionId>,
    pub mode: Option<SessionMode>,
    pub llm: Option<SessionLlmSettings>,
    pub ephemeral: bool,
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
    available_models: Vec<AvailableModel>,
}

impl RuntimeProfile {
    fn from_loaded(source: String, loaded: &LoadedAgentProfile) -> Self {
        Self {
            source,
            config: loaded.config.clone(),
            model_options: loaded
                .models
                .models
                .iter()
                .map(|(id, model)| SessionModelOption {
                    id: id.clone(),
                    name: model.model_name.clone(),
                    provider: model.provider.clone(),
                    reasoning: model
                        .reasoning_efforts
                        .iter()
                        .map(|effort| ReasoningOption {
                            id: effort.as_str().to_string(),
                            name: effort.display_name().to_string(),
                        })
                        .collect(),
                    default_reasoning: model.default_reasoning_effort.as_str().to_string(),
                })
                .collect(),
            available_models: loaded
                .models
                .models
                .iter()
                .map(|(id, model)| {
                    let mut hosted_tools = model
                        .hosted_tools
                        .iter()
                        .filter_map(|tool| {
                            tool.get("type").and_then(Value::as_str).map(str::to_owned)
                        })
                        .collect::<Vec<_>>();
                    hosted_tools.sort();
                    hosted_tools.dedup();
                    AvailableModel {
                        id: id.clone(),
                        name: model.model_name.clone(),
                        provider: model.provider.clone(),
                        capabilities: AvailableModelCapabilities {
                            image_input: model.capabilities.image_input,
                            tool_calls: model.capabilities.tool_calls,
                            hosted_tools,
                        },
                        reasoning: model
                            .reasoning_efforts
                            .iter()
                            .map(|effort| ReasoningOption {
                                id: effort.as_str().to_string(),
                                name: effort.display_name().to_string(),
                            })
                            .collect(),
                        default_reasoning: model.default_reasoning_effort.as_str().to_string(),
                    }
                })
                .collect(),
        }
    }

    fn defaults(&self) -> (String, Option<String>, SessionMode) {
        (
            self.config.model.default.model.clone(),
            self.config.model.default.reasoning.clone(),
            self.config.policy_mode,
        )
    }
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
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfigUpdateParam {
    #[serde(default)]
    max_model_steps: Option<usize>,
    #[serde(default)]
    logging: Option<dwo_agent_service::LoggingConfig>,
    #[serde(default)]
    external_skills_dirs: Option<Vec<PathBuf>>,
    #[serde(default)]
    external_rule_files: Option<Vec<PathBuf>>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionModelOption {
    id: String,
    name: String,
    provider: String,
    reasoning: Vec<ReasoningOption>,
    default_reasoning: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AvailableModel {
    id: String,
    name: String,
    provider: String,
    capabilities: AvailableModelCapabilities,
    reasoning: Vec<ReasoningOption>,
    default_reasoning: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AvailableModelCapabilities {
    image_input: bool,
    tool_calls: bool,
    hosted_tools: Vec<String>,
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
    pub async fn build(config_path: impl AsRef<Path>) -> Result<Arc<Self>> {
        let profile_root = profile_root(config_path.as_ref())?;
        let config_manager = ConfigManager::new(profile_root.clone());
        let mcp = Arc::new(McpRuntime::new(&profile_root));
        mcp.sync_and_start().await?;
        tracing::info!(event = "mcp.synchronized", "MCP configuration synchronized");
        let profile = LoadedAgentProfile::load(&profile_root)?;
        let source = config_manager.fingerprint()?;
        let runtime_profile = RuntimeProfile::from_loaded(source, &profile);
        let (default_model, default_reasoning, default_mode) = runtime_profile.defaults();
        let repository =
            Arc::new(FsSessionRepository::new(profile_root.join("runtime/sessions")).await?);
        let channels =
            Arc::new(ChannelManager::new(&profile_root, &runtime_profile.config.channels).await?);
        let service = Arc::new(SessionService::from_profile(
            repository,
            profile,
            PolicyConfig::default(),
        )?);
        let shutdown = CancellationToken::new();
        let projects = Arc::new(ProjectService::open(profile_root.join("runtime/projects"))?);
        for project in projects.list() {
            for topic in &project.board.topics {
                let rule_file = ExternalRuleFile::new(
                    projects.agents_path(&project.id, &topic.id)?,
                    project.pwd.clone(),
                );
                for session_id in &topic.session_ids {
                    let session_id =
                        SessionId::parse(session_id.clone()).map_err(anyhow::Error::msg)?;
                    service.set_external_rule_files(&session_id, vec![rule_file.clone()]);
                }
            }
        }
        let automation = AutomationRuntime::new(
            service.clone(),
            projects.clone(),
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
            profile: RwLock::new(runtime_profile),
            profile_reload: tokio::sync::Mutex::new(()),
            shutdown,
            events: Arc::new(HostEventHub::new()),
            request_cache: tokio::sync::Mutex::new(HashMap::new()),
            websocket_running: AtomicBool::new(false),
        });
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
        tokio::join!(
            self.channel_gateway.stop_all(),
            self.mcp.shutdown(),
            self.service.shutdown()
        );
    }

    async fn handle_method(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        if method.starts_with("project.") {
            return self.dispatch_project(method, params).await;
        }
        if method.starts_with("websocket.") {
            return self.dispatch_websocket(method, params).await;
        }
        if method.starts_with("channel.") {
            return self.dispatch_channel(method, params).await;
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
        if method.starts_with("session.") {
            return self.dispatch_session(method, params).await;
        }
        match method {
            "dwo.capabilities" => Ok(serde_json::to_value(dwo_protocol::capabilities())?),
            "event.read" => {
                let params: EventReadParam = serde_json::from_value(params)?;
                let result: EventReadResult = self
                    .events
                    .read(params.cursor, params.limit, params.event.as_deref())
                    .await;
                Ok(serde_json::to_value(result)?)
            }
            "daemon.status" => {
                let session_count = self.session_count().await?;
                Ok(json!({
                    "healthy": true,
                    "profile_root": self.profile_root,
                    "sessions": session_count,
                    "channels": self.channels().list().await?.len(),
                    "automationJobs": self.automation.list(None).await.len(),
                }))
            }
            "daemon.shutdown" => {
                self.shutdown.cancel();
                Ok(json!({"stopping": true}))
            }
            "config.snapshot" => {
                let session_count = self.session_count().await?;
                Ok(serde_json::to_value(self.config_snapshot(session_count))?)
            }
            "config.update" => {
                let params: ConfigUpdateParam = serde_json::from_value(params)?;
                self.update_config(params).await?;
                Ok(json!({"updated": true}))
            }
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
        let cacheable = dwo_protocol::is_side_effect_method(method);
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
        let source = self.config_manager.fingerprint()?;
        if self.profile.read().expect("profile lock poisoned").source == source {
            return Ok(false);
        }
        let loaded = self.config_manager.load()?;
        self.apply_profile(loaded).await?;
        Ok(true)
    }

    pub(crate) async fn edit_profile<F>(self: &Arc<Self>, update: F) -> Result<()>
    where
        F: FnOnce(&mut dwo_agent_service::AgentProfileConfig) -> Result<()>,
    {
        self.config_manager.update(update).await?;
        self.reload_profile_if_changed().await?;
        Ok(())
    }

    pub async fn apply_profile(self: &Arc<Self>, loaded: LoadedAgentProfile) -> Result<()> {
        let _reload = self.profile_reload.lock().await;
        let source = self.config_manager.fingerprint()?;
        let runtime_profile = RuntimeProfile::from_loaded(source, &loaded);
        let channels_changed = self
            .profile
            .read()
            .expect("profile lock poisoned")
            .config
            .channels
            != runtime_profile.config.channels;
        let replacement_channels = if channels_changed {
            Some(Arc::new(
                ChannelManager::new(&self.profile_root, &runtime_profile.config.channels).await?,
            ))
        } else {
            None
        };
        let (default_model, default_reasoning, default_mode) = runtime_profile.defaults();

        self.service.apply_profile(loaded)?;
        self.automation
            .apply_defaults(default_model, default_reasoning, default_mode)
            .await;
        crate::logging::reload(&runtime_profile.config.logging)?;

        if let Some(channels) = replacement_channels {
            self.channel_gateway.stop_all().await;
            *self
                .channels
                .write()
                .expect("channel manager lock poisoned") = channels;
            self.channel_gateway.start_all(self.clone()).await;
        }

        *self.profile.write().expect("profile lock poisoned") = runtime_profile;
        tracing::info!(event = "profile.reloaded", "profile configuration reloaded");
        tracing::info!(
            event = "config.changed",
            source = "profile",
            "host configuration applied"
        );
        self.events
            .publish("config.changed", json!({"source": "profile"}))
            .await;
        Ok(())
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
        .map(|(name, value)| redacted_mcp_server_config(name, value))
        .collect::<Vec<_>>();
    json!({"servers": servers})
}

pub(super) fn redacted_mcp_server_config(name: &str, value: &Value) -> Value {
    let object = value.as_object();
    json!({
        "name": name,
        "enabled": object.and_then(|v| v.get("enabled")).and_then(Value::as_bool).unwrap_or(true),
        "type": object.and_then(|v| v.get("type")).and_then(Value::as_str).unwrap_or("stdio"),
        "description": object.and_then(|v| v.get("description")).and_then(Value::as_str),
        "authConfigured": object.and_then(|v| v.get("auth")).is_some(),
        "credentialsConfigured": object.is_some_and(|v| v.contains_key("env") || v.contains_key("headers")),
    })
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
mod tests;
