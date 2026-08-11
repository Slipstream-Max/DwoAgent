use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use dwo_agent_service::{
    AgentService, EndpointId, FsSessionRepository, LoadedAgentProfile, NewSession, PromptAccepted,
    SessionConfig, SessionConfigUpdate, SessionEventPayload, SessionId, SessionLlmSettings,
    SessionRecord, SessionSnapshot, SessionStatusSnapshot, SessionSubscription, TurnId,
};
use dwo_context::MessageContent;
use dwo_mcp::McpRuntime;
use dwo_tools::{ConfirmationDecision, PolicyConfig, SessionMode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::automation::{
    AutomationConfig, AutomationJob, AutomationRuntime, parse_config as parse_automation_config,
};
use crate::channels::{ChannelGateway, ChannelKind, ChannelManager, ChannelPollParams};
use crate::slash_commands::{
    AvailableSkill, DirectiveKinds, directive_kinds, expand as expand_prompt_directives,
};

pub struct Host {
    service: Arc<AgentService>,
    pub channel_gateway: Arc<ChannelGateway>,
    pub mcp: Arc<McpRuntime>,
    pub automation: Arc<AutomationRuntime>,
    channels: RwLock<Arc<ChannelManager>>,
    profile_root: PathBuf,
    config_path: PathBuf,
    profile: RwLock<RuntimeProfile>,
    profile_reload: tokio::sync::Mutex<()>,
    shutdown: CancellationToken,
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
}

#[derive(Deserialize)]
struct SessionCommandParam {
    session_id: String,
    endpoint_id: String,
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
    caller_session_id: Option<String>,
}

#[derive(Deserialize)]
struct AutomationNameParam {
    job: String,
}

#[derive(Deserialize)]
struct AutomationAddParam {
    job: AutomationJob,
}

#[derive(Deserialize)]
struct AutomationToggleParam {
    job: Option<String>,
    #[serde(default)]
    all: bool,
}

#[derive(Deserialize)]
struct AutomationDeleteParam {
    job: Option<String>,
    #[serde(default)]
    all: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionModelOption {
    id: String,
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
        let source = std::fs::read_to_string(profile_root.join("profile.yaml"))?;
        let default_model = profile.models.default_model_name.clone();
        let default_mode = profile.config.policy_mode;
        let default_max_model_steps = profile.config.max_model_steps;
        let profile_config = profile.config.clone();
        let model_options = profile
            .models
            .models
            .iter()
            .map(|(id, model)| SessionModelOption {
                id: id.clone(),
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
            channel_gateway: Arc::new(ChannelGateway::new()),
            mcp,
            automation,
            channels: RwLock::new(channels),
            profile_root,
            config_path: config_path.to_path_buf(),
            profile: RwLock::new(RuntimeProfile {
                source,
                config: profile_config,
                model_options,
            }),
            profile_reload: tokio::sync::Mutex::new(()),
            shutdown,
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

    pub(crate) fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub async fn shutdown(&self) {
        tokio::join!(
            self.channel_gateway.stop_all(),
            self.mcp.shutdown(),
            self.service.shutdown()
        );
    }

    pub async fn list_sessions(
        &self,
        all: bool,
        caller: Option<&SessionId>,
    ) -> Result<Vec<SessionRecord>> {
        let mut records = self.service.list().await?;
        if !all {
            records.retain(|record| record.info.parent_session_id.as_ref() == caller);
        }
        Ok(records)
    }

    pub async fn list_session_statuses(
        &self,
        all: bool,
        caller: Option<&SessionId>,
    ) -> Result<Vec<SessionStatusSnapshot>> {
        let mut statuses = self.service.list_statuses().await?;
        if !all {
            statuses.retain(|status| status.record.info.parent_session_id.as_ref() == caller);
        }
        Ok(statuses)
    }

    pub async fn session_status(&self, id: &SessionId) -> Result<SessionStatusSnapshot> {
        Ok(self.service.status(id).await?)
    }

    pub async fn session_snapshot(&self, id: &SessionId) -> Result<SessionSnapshot> {
        Ok(self.service.load(id).await?.snapshot().await?)
    }

    pub async fn setup_session(
        &self,
        title: Option<String>,
        cwd: Option<PathBuf>,
    ) -> Result<SessionSnapshot> {
        Ok(self.create_session(title, cwd).await?.snapshot().await?)
    }

    pub async fn fork_session(&self, source_id: &SessionId) -> Result<SessionSnapshot> {
        Ok(self.service.fork(source_id, None).await?.snapshot().await?)
    }

    pub async fn subscribe_session(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
        checkpoint_cursor: Option<usize>,
    ) -> Result<SessionSubscription> {
        Ok(self
            .service
            .load(id)
            .await?
            .attach_from(endpoint, checkpoint_cursor)
            .await?)
    }

    pub async fn prompt_session(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
        content: MessageContent,
    ) -> Result<PromptAccepted> {
        let agent = self.service.load(id).await?;
        let snapshot = agent.snapshot().await?;
        let content = self
            .expand_prompt_directives(&snapshot.record.info.cwd, content)
            .await?;
        Ok(agent.prompt_content(endpoint, content).await?)
    }

    async fn expand_prompt_directives(
        &self,
        cwd: &Path,
        content: MessageContent,
    ) -> Result<MessageContent> {
        let kinds: DirectiveKinds = directive_kinds(&content);
        if kinds.is_empty() {
            return Ok(content);
        }
        let skills = if kinds.skill {
            self.service
                .skill_snapshots(cwd)?
                .into_iter()
                .map(|skill| AvailableSkill {
                    name: skill.name,
                    path: skill.path,
                })
                .collect()
        } else {
            Vec::new()
        };
        let mcp_servers = if kinds.mcp {
            self.mcp
                .catalog_snapshot()
                .await?
                .servers
                .into_iter()
                .map(|server| server.name)
                .collect()
        } else {
            Vec::new()
        };
        Ok(expand_prompt_directives(content, &skills, &mcp_servers))
    }

    async fn prompt_directive_options(&self, id: &SessionId) -> Result<Value> {
        let snapshot = self.session_snapshot(id).await?;
        let skills = self
            .service
            .skill_snapshots(&snapshot.record.info.cwd)?
            .into_iter()
            .filter(|skill| !skill.name.chars().any(char::is_whitespace))
            .map(|skill| {
                json!({
                    "name": skill.name,
                    "description": skill.description,
                })
            })
            .collect::<Vec<_>>();
        let mcp_servers = self
            .mcp
            .catalog_snapshot()
            .await?
            .servers
            .into_iter()
            .filter(|server| !server.name.chars().any(char::is_whitespace))
            .map(|server| {
                json!({
                    "name": server.name,
                    "description": server.description,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "skills": skills,
            "mcpServers": mcp_servers,
        }))
    }

    pub async fn compact_session(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
    ) -> Result<PromptAccepted> {
        Ok(self.service.load(id).await?.compact(endpoint).await?)
    }

    pub async fn resume_session_turn(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
    ) -> Result<Option<PromptAccepted>> {
        Ok(self.service.load(id).await?.resume(endpoint).await?)
    }

    pub async fn cancel_session(
        &self,
        id: &SessionId,
        expected_turn_id: Option<TurnId>,
    ) -> Result<()> {
        self.service
            .load(id)
            .await?
            .cancel(expected_turn_id)
            .await?;
        Ok(())
    }

    pub async fn close_session(&self, id: &SessionId) -> Result<()> {
        self.service.close(id).await?;
        Ok(())
    }

    pub async fn set_session_config(
        &self,
        id: &SessionId,
        update: SessionConfigUpdate,
    ) -> Result<SessionSnapshot> {
        let agent = self.service.load(id).await?;
        agent.set_config(update).await?;
        Ok(agent.snapshot().await?)
    }

    pub async fn resolve_session_permission(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
        request_id: String,
        decision: ConfirmationDecision,
    ) -> Result<()> {
        self.service
            .load(id)
            .await?
            .respond_permission(endpoint, request_id, decision)
            .await?;
        Ok(())
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
                "channels": self.channels().list().await?.len(),
                "automationJobs": self.automation.list().await.len(),
            })),
            "daemon.shutdown" => {
                self.shutdown.cancel();
                Ok(json!({"stopping": true}))
            }
            "profile.list" => {
                let (name, description, policy, default_model, models) = {
                    let profile = self.profile.read().expect("profile lock poisoned");
                    (
                        profile.config.name.clone(),
                        profile.config.description.clone(),
                        profile.config.policy_mode,
                        profile.config.model.default_model_name.clone(),
                        profile.model_options.clone(),
                    )
                };
                Ok(serde_json::to_value(ProfileSnapshot {
                    name,
                    description,
                    policy,
                    default_model,
                    models,
                    session_count: self.service.list().await?.len(),
                })?)
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
            "session.new" => {
                let params: NewSessionParam = serde_json::from_value(params)?;
                let snapshot = self.setup_session(params.title, params.cwd).await?;
                Ok(json!({
                    "session_id": snapshot.record.info.id,
                    "usage": snapshot.usage,
                }))
            }
            "session.fork" => {
                let source_id = parse_session(params)?;
                let snapshot = self.fork_session(&source_id).await?;
                let id = snapshot.record.info.id;
                Ok(json!({
                    "accepted": false,
                    "session_id": id.clone(),
                    "forked_session_id": id,
                    "usage": snapshot.usage,
                }))
            }
            "session.delete" => {
                let id = parse_session(params)?;
                self.delete_session(&id).await?;
                Ok(json!({"deleted": true}))
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
                let accepted = agent.prompt_content(endpoint, content).await?;
                if let Some(parent_id) = parent_id {
                    self.spawn_result_delivery(
                        subscription,
                        agent.id().clone(),
                        parent_id,
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
            "session.set_model" => {
                let params: ConfigParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let model = params
                    .value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("model must be a string"))?;
                let snapshot = self
                    .set_session_config(&id, SessionConfigUpdate::Model(model.to_string()))
                    .await?;
                Ok(json!({
                    "updated": true,
                    "usage": snapshot.usage,
                }))
            }
            "session.set_reasoning" => {
                let params: ConfigParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let reasoning = params.value.as_str().map(str::to_string);
                self.set_session_config(&id, SessionConfigUpdate::Reasoning(reasoning))
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

    async fn dispatch_automation(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        match method {
            "automation.list" => Ok(serde_json::to_value(self.automation.list().await)?),
            "automation.status" => {
                let params: AutomationNameParam = serde_json::from_value(params)?;
                Ok(serde_json::to_value(
                    self.automation.status(&params.job).await?,
                )?)
            }
            "automation.add" => {
                let params: AutomationAddParam = serde_json::from_value(params)?;
                let name = params.job.name.clone();
                self.mutate_automation_config(|config| {
                    anyhow::ensure!(
                        !config.jobs.iter().any(|job| job.name == name),
                        "automation job already exists: {name}"
                    );
                    config.enabled = true;
                    config.jobs.push(params.job);
                    Ok(())
                })
                .await?;
                Ok(serde_json::to_value(self.automation.status(&name).await?)?)
            }
            "automation.enable" | "automation.disable" => {
                let params: AutomationToggleParam = serde_json::from_value(params)?;
                let expected = method == "automation.enable";
                let job_name = params.job.clone();
                let all = params.all;
                self.mutate_automation_config(|config| {
                    anyhow::ensure!(all ^ job_name.is_some(), "specify a job or --all");
                    if expected {
                        config.enabled = true;
                    }
                    if all {
                        for job in &mut config.jobs {
                            job.enabled = expected;
                        }
                    } else if let Some(name) = &job_name {
                        let job = config
                            .jobs
                            .iter_mut()
                            .find(|job| &job.name == name)
                            .with_context(|| format!("automation job not found: {name}"))?;
                        job.enabled = expected;
                    }
                    Ok(())
                })
                .await?;
                Ok(
                    json!({"updated": if all { "all" } else { job_name.as_deref().unwrap() }, "enabled": expected}),
                )
            }
            "automation.delete" => {
                let params: AutomationDeleteParam = serde_json::from_value(params)?;
                let job_name = params.job.clone();
                let all = params.all;
                self.mutate_automation_config(|config| {
                    anyhow::ensure!(all ^ job_name.is_some(), "specify a job or --all");
                    if all {
                        config.jobs.clear();
                    } else if let Some(name) = &job_name {
                        let previous = config.jobs.len();
                        config.jobs.retain(|job| &job.name != name);
                        anyhow::ensure!(
                            config.jobs.len() != previous,
                            "automation job not found: {name}"
                        );
                    }
                    Ok(())
                })
                .await?;
                self.automation
                    .remove_job_state(job_name.as_deref(), all)
                    .await?;
                Ok(json!({"deleted": if all { "all" } else { job_name.as_deref().unwrap() }}))
            }
            "automation.run" => {
                let params: AutomationJobParam = serde_json::from_value(params)?;
                let caller = parse_optional_session(params.caller_session_id)?;
                if let Some(caller) = &caller {
                    self.service.load(caller).await?;
                }
                Ok(serde_json::to_value(
                    self.automation.run_now(&params.job, caller).await?,
                )?)
            }
            other => anyhow::bail!("unknown RPC method: {other}"),
        }
    }

    async fn mutate_automation_config<F>(self: &Arc<Self>, update: F) -> Result<()>
    where
        F: FnOnce(&mut AutomationConfig) -> Result<()>,
    {
        let reload = self.profile_reload.lock().await;
        let mut profile = dwo_agent_service::AgentProfileConfig::load(&self.profile_root)?;
        let mut automation = parse_automation_config(profile.automation.clone())?;
        update(&mut automation)?;
        parse_automation_config(serde_yaml::to_value(&automation)?)?;
        profile.automation = serde_yaml::to_value(automation)?;
        profile.validate()?;

        let path = self.profile_root.join("profile.yaml");
        dwo_agent_service::atomic_file::write(&path, serde_yaml::to_string(&profile)?.into_bytes())
            .await?;
        drop(reload);
        self.reload_profile_if_changed().await?;
        Ok(())
    }

    async fn dispatch_mcp(&self, method: &str, params: Value) -> Result<Value> {
        match method {
            "mcp.list" => Ok(serde_json::to_value(self.mcp.catalog_snapshot().await?)?),
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
        if method == "channel.list" {
            return Ok(serde_json::to_value(self.channels().list().await?)?);
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
            "begin" => Ok(self
                .channel_gateway
                .begin_bind(channel, self.clone())
                .await?),
            "poll" => {
                let params: ChannelPollParams = serde_json::from_value(params)?;
                Ok(serde_json::to_value(
                    self.channel_gateway
                        .poll_bind(channel, self.clone(), params)
                        .await?,
                )?)
            }
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
            ManagedChannelAction::Status => {
                let mut value = serde_json::to_value(self.channels().summary(channel).await?)?;
                if channel == ChannelKind::Websocket {
                    let runtime = self.channels().load_websocket().await?;
                    let object = value.as_object_mut().expect("channel summary is an object");
                    object.insert(
                        "running".to_string(),
                        json!(self.channel_gateway.is_running(channel).await),
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
                let target = self.channels().bound_target(channel).await?;
                self.channel_gateway
                    .send_message(channel, &params.text)
                    .await?;
                Ok(json!({"sent": true, "to": target}))
            }
            ManagedChannelAction::SendFile => {
                let params: SendFileParam = serde_json::from_value(params)?;
                let target = self.channels().bound_target(channel).await?;
                self.channel_gateway
                    .send_file(channel, &params.path)
                    .await?;
                Ok(json!({"sent": true, "to": target, "path": params.path}))
            }
            ManagedChannelAction::Remove => Ok(json!({
                "removed": self.channel_gateway.unbind(channel, self.clone()).await?
            })),
            ManagedChannelAction::Token => {
                anyhow::ensure!(
                    channel == ChannelKind::Websocket,
                    "token is only available for WebSocket"
                );
                let runtime = self.channels().load_websocket().await?;
                Ok(json!({"token": runtime.token, "port": runtime.config.port, "path": "/acp"}))
            }
            ManagedChannelAction::ResetToken => {
                anyhow::ensure!(
                    channel == ChannelKind::Websocket,
                    "reset-token is only available for WebSocket"
                );
                self.channel_gateway.stop(channel).await;
                let token = self.channels().reset_websocket_token().await?;
                let summary = self.channels().summary(channel).await?;
                if summary.enabled {
                    self.channel_gateway.start(channel, self.clone()).await?;
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
        let (default_model, default_mode, default_max_model_steps) = self.defaults();
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
                automation_job: None,
                cwd,
                mode: default_mode,
                max_model_steps: default_max_model_steps,
                llm: SessionLlmSettings::new(default_model, None),
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
        anyhow::ensure!(
            params.session_id.is_none() || params.from_session_id.is_none(),
            "--from cannot be used with --to"
        );
        let (default_model, default_mode, default_max_model_steps) = self.defaults();
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
            apply_prompt_config(&agent, params).await?;
            return Ok((agent, record.info.parent_session_id.clone()));
        }

        if let Some(source) = &params.from_session_id {
            anyhow::ensure!(
                params.cwd.is_none(),
                "--cwd cannot be used when forking a session"
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
                automation_job: None,
                cwd,
                mode,
                max_model_steps: default_max_model_steps,
                llm: SessionLlmSettings::new(model, reasoning),
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

    pub(crate) fn channels(&self) -> Arc<ChannelManager> {
        self.channels
            .read()
            .expect("channel manager lock poisoned")
            .clone()
    }

    fn defaults(&self) -> (String, SessionMode, usize) {
        let profile = self.profile.read().expect("profile lock poisoned");
        (
            profile.config.model.default_model_name.clone(),
            profile.config.policy_mode,
            profile.config.max_model_steps,
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
                            tracing::warn!(
                                event = "profile.reload_failed",
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
        let _reload = self.profile_reload.lock().await;
        let source = std::fs::read_to_string(self.profile_root.join("profile.yaml"))?;
        if self.profile.read().expect("profile lock poisoned").source == source {
            return Ok(false);
        }

        let loaded = LoadedAgentProfile::load(&self.profile_root)?;
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
                provider: model.provider.clone(),
                reasoning: model.reasoning.keys().cloned().collect(),
                default_reasoning: model.default_reasoning_mode.clone(),
            })
            .collect();
        let default_model = loaded.models.default_model_name.clone();
        let default_mode = loaded.config.policy_mode;
        let default_max_model_steps = loaded.config.max_model_steps;

        self.service.replace_models(loaded.models)?;
        self.service
            .replace_external_skill_dirs(loaded.external_skill_dirs);
        self.automation
            .apply_profile(
                automation_config,
                default_model,
                default_mode,
                default_max_model_steps,
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
        Ok(true)
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
        checkpoint_cursor: Option<usize>,
    ) -> Result<dwo_agent_service::SessionSubscription> {
        let id = SessionId::parse(session_id.to_string()).map_err(anyhow::Error::msg)?;
        let endpoint = EndpointId::parse(endpoint_id.to_string()).map_err(anyhow::Error::msg)?;
        self.subscribe_session(&id, endpoint, checkpoint_cursor)
            .await
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
            | SessionEventPayload::ToolStarted { .. }
            | SessionEventPayload::ToolUpdated { .. }
            | SessionEventPayload::ToolCompleted { .. }
            | SessionEventPayload::TerminalOpened { .. }
            | SessionEventPayload::TerminalExited { .. }
            | SessionEventPayload::FileRead { .. }
            | SessionEventPayload::FileChanged { .. }
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
                "externalSkillsDirs",
                "logging",
                "maxModelSteps",
                "model",
                "name",
                "policyMode",
            ]
        );
        assert_eq!(yaml_keys(&document["logging"]), ["level", "retentionDays"]);
        assert_eq!(
            yaml_keys(&document["channels"]["weixin"]),
            [
                "enabled",
                "markdownFilter",
                "mediaInput",
                "replayMode",
                "replayTurns"
            ]
        );
        assert_eq!(
            yaml_keys(&document["channels"]["telegram"]),
            [
                "botTokenEnv",
                "enabled",
                "mediaInput",
                "replayMode",
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
                "replayMode",
                "replayTurns"
            ]
        );
        assert_eq!(
            yaml_keys(&document["channels"]["qq"]),
            ["enabled", "mediaInput", "replayMode", "replayTurns"]
        );
        assert_eq!(
            yaml_keys(&document["channels"]["websocket"]),
            ["enabled", "port"]
        );
        assert_eq!(
            yaml_keys(&document["automation"]),
            ["enabled", "jobs", "timeoutSeconds"]
        );
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
            ["behavior", "cwd", "mode", "title"]
        );
        assert_eq!(
            yaml_keys(&document["automation"]["jobs"][1]["session"]),
            ["mode", "sessionId"]
        );
        assert_eq!(
            yaml_keys(&document["model"]),
            ["defaultModelName", "models", "providers"]
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
        assert_eq!(channel.len(), 5);
        assert_eq!(channel[0].name, "weixin");
        assert!(!channel[0].enabled);
        assert_eq!(channel[1].name, "telegram");
        assert!(!channel[1].enabled);
        assert_eq!(channel[2].name, "feishu");
        assert!(!channel[2].enabled);
        assert_eq!(channel[3].name, "qq");
        assert!(!channel[3].enabled);
        assert_eq!(channel[4].name, "websocket");
        assert!(!channel[4].enabled);

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
  defaultModelName: deepseek-v4-pro
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
            .dispatch(
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
            r#"name: reloaded
description: reloaded agent
policyMode: watch
maxModelSteps: 17
logging:
  level: debug
  retentionDays: 7
model:
  defaultModelName: backup
  providers:
    deepseek:
      type: deepseek
  models:
    - modelName: deepseek-v4-pro
      provider: deepseek
      modelId: deepseek-v4-pro
    - modelName: backup
      provider: deepseek
      modelId: deepseek-v4-pro
channels:
  websocket:
    enabled: false
    port: 19000
automation:
  enabled: false
  jobs: []
"#,
        )
        .unwrap();

        assert!(host.reload_profile_if_changed().await.unwrap());
        let snapshot = host.dispatch("profile.list", json!({})).await.unwrap();
        assert_eq!(snapshot["name"], "reloaded");
        assert_eq!(snapshot["description"], "reloaded agent");
        assert_eq!(snapshot["policy"], "watch");
        assert_eq!(snapshot["defaultModel"], "backup");
        assert_eq!(snapshot["models"].as_array().unwrap().len(), 2);
        assert_eq!(host.channels().list().await.unwrap().len(), 1);
        existing
            .set_config(SessionConfigUpdate::Model("backup".to_string()))
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
        assert_eq!(record.llm.model, "backup");
        assert_eq!(record.config().max_model_steps, 17);

        let invalid =
            std::fs::read_to_string(&config)
                .unwrap()
                .replacen("name: reloaded", "name: ''", 1);
        std::fs::write(&config, invalid).unwrap();
        assert!(host.reload_profile_if_changed().await.is_err());
        let snapshot = host.dispatch("profile.list", json!({})).await.unwrap();
        assert_eq!(snapshot["name"], "reloaded");
        assert_eq!(snapshot["defaultModel"], "backup");

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
            .dispatch("automation.add", json!({"job": job}))
            .await
            .unwrap();
        assert_eq!(added["job"]["name"], "daily-report");
        assert_eq!(host.automation.list().await.len(), 1);

        host.dispatch(
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

        host.dispatch("automation.enable", json!({"job": null, "all": true}))
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

        host.dispatch(
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
            .dispatch(
                "automation.run",
                json!({"job": "background-failure", "caller_session_id": null}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cannot find the file"));

        let value = host
            .dispatch(
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
