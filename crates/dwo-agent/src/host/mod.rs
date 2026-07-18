use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use dwo_agent_service::{
    AgentService, EndpointId, FsSessionRepository, LoadedAgentProfile, NewSession, SessionConfig,
    SessionConfigUpdate, SessionId, SessionLlmSettings,
};
use dwo_mcp::{McpRuntime, ShowResult};
use dwo_tools::{ConfirmationDecision, PolicyConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::automation::{AutomationRuntime, parse_config as parse_automation_config};
use crate::channels::{ChannelManager, GatewayHub, WeixinLoginProgress};

pub struct Host {
    pub service: Arc<AgentService>,
    pub channels: Arc<ChannelManager>,
    pub gateway: Arc<GatewayHub>,
    pub mcp: Arc<McpRuntime>,
    pub automation: Arc<AutomationRuntime>,
    profile_root: PathBuf,
    default_model: String,
    default_mode: dwo_tools::SessionMode,
    model_options: Vec<SessionModelOption>,
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
struct PromptParam {
    session_id: String,
    endpoint_id: String,
    message: String,
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
struct McpSelectorParam {
    selector: String,
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

impl Host {
    pub async fn load(config_path: &Path) -> Result<Arc<Self>> {
        let profile_root = profile_root(config_path)?;
        let mcp = Arc::new(McpRuntime::new(&profile_root));
        mcp.refresh().await?;
        let profile = LoadedAgentProfile::load(&profile_root)?;
        let default_model = profile.models.default_model_id.clone();
        let default_mode = profile.config.policy_mode;
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
            shutdown.clone(),
        )?;
        let host = Arc::new(Self {
            service,
            channels,
            gateway: Arc::new(GatewayHub::new()),
            mcp,
            automation,
            profile_root,
            default_model,
            default_mode,
            model_options,
            shutdown,
        });
        host.gateway.start_all(host.clone()).await;
        host.start_mcp_watcher();
        host.automation.start();
        Ok(host)
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    pub async fn dispatch(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
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
            "session.list" => Ok(serde_json::to_value(self.service.list().await?)?),
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
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let endpoint = EndpointId::parse(params.endpoint_id).map_err(anyhow::Error::msg)?;
                let agent = self.service.load(&id).await?;
                let turn_id = agent.prompt(endpoint, params.message).await?;
                Ok(json!({"turn_id": turn_id}))
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
            "automation.list" | "automation.status" => {
                Ok(serde_json::to_value(self.automation.list().await)?)
            }
            "automation.run" => {
                let params: AutomationJobParam = serde_json::from_value(params)?;
                Ok(serde_json::to_value(
                    self.automation.run_now(&params.job).await?,
                )?)
            }
            "channel.list" => Ok(serde_json::to_value(self.channels.list().await?)?),
            "channel.weixin.status" => {
                let channel = self
                    .channels
                    .list()
                    .await?
                    .into_iter()
                    .find(|channel| channel.name == "weixin")
                    .context("channels.weixin is not configured")?;
                Ok(serde_json::to_value(channel)?)
            }
            "channel.weixin.send_message" => {
                let params: SendMessageParam = serde_json::from_value(params)?;
                let target = self.channels.bound_weixin_user().await?;
                self.gateway
                    .send_weixin_message(&target, &params.text)
                    .await?;
                Ok(json!({"sent": true, "to": target}))
            }
            "channel.weixin.send_file" => {
                let params: SendFileParam = serde_json::from_value(params)?;
                let target = self.channels.bound_weixin_user().await?;
                self.gateway.send_weixin_file(&target, &params.path).await?;
                Ok(json!({"sent": true, "to": target, "path": params.path}))
            }
            "channel.weixin.remove" => {
                self.gateway.stop().await;
                Ok(json!({"removed": self.channels.remove_weixin().await?}))
            }
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
                    self.gateway.stop().await;
                    if channel.enabled {
                        self.gateway.start_weixin(self.clone()).await?;
                    }
                }
                Ok(serde_json::to_value(progress)?)
            }
            "mcp.list" => Ok(serde_json::to_value(self.mcp.catalog().await?)?),
            "mcp.search" => {
                let params: McpSearchParam = serde_json::from_value(params)?;
                let catalog = self.mcp.catalog().await?;
                Ok(serde_json::to_value(catalog.search(&params.query))?)
            }
            "mcp.show" => {
                let params: McpSelectorParam = serde_json::from_value(params)?;
                let catalog = self.mcp.catalog().await?;
                match catalog.show(&params.selector)? {
                    ShowResult::Server(server) => Ok(json!({
                        "kind": "server",
                        "server": server,
                    })),
                    ShowResult::Tool { server, tool } => Ok(json!({
                        "kind": "tool",
                        "server": server,
                        "tool": tool,
                    })),
                }
            }
            "mcp.call" => {
                let params: McpCallParam = serde_json::from_value(params)?;
                Ok(serde_json::to_value(
                    self.mcp.call(&params.selector, params.arguments).await?,
                )?)
            }
            "mcp.auth.status" => {
                let params: McpAuthParam = serde_json::from_value(params)?;
                Ok(serde_json::to_value(
                    self.mcp.auth_status(&params.server).await?,
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
                title,
                cwd,
                mode: self.default_mode,
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
                        if let Err(error) = runtime.refresh_if_changed().await {
                            eprintln!("reload MCP configuration: {error:#}");
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

fn parse_session(params: Value) -> Result<SessionId> {
    let params: SessionIdParam = serde_json::from_value(params)?;
    SessionId::parse(params.session_id).map_err(anyhow::Error::msg)
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
