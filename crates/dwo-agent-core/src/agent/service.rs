//! Simple Agent runtime with session/state management.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::Local;
use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::factory::{CreateSessionArgs, RuntimeFactorySpec};
use super::session::{SESSION_META_FILE, SESSION_MODEL_CONTEXT_FILE};
use super::session_agent::SessionAgent;
use crate::config::loader::{
    agent_yaml_path, read_agent_meta, read_json_model, read_model_registry, read_tool_policy,
    resolve_agent_structure_dir, resolve_channel_state_dir, resolve_session_store_dir,
};
use crate::config::models::{
    AgentMeta, AgentState, AgentTools, ContextUsageSnapshot, ModelProfile, PolicyMode,
    ReasoningMode, SessionMetaPayload, SessionModelContextPayload, StopReason,
};
use crate::config::policy::ToolPolicyConfig;
use crate::ingress::channel_control::{PendingConfirmationRegistry, SessionLeaseRegistry};
use crate::tools::{PermissionRequester, UpdateEmitter, tool_schemas};
use crate::utils::policy::parse_policy_mode;

/// Own session lifecycle + run loop state transitions.
pub struct AgentService {
    agent_structure_dir: PathBuf,
    agent_meta: AgentMeta,
    default_model_id: String,
    model_profiles: HashMap<String, ModelProfile>,
    tool_policy: Arc<ToolPolicyConfig>,
    session_store_dir: PathBuf,
    channel_state_dir: PathBuf,
    session_leases: Arc<SessionLeaseRegistry>,
    pending_confirmations: Arc<PendingConfirmationRegistry>,
    agents: Mutex<HashMap<String, Arc<SessionAgent>>>,
}

#[derive(Debug, Clone)]
pub struct AgentProfileSnapshot {
    pub agent_id: String,
    pub name: String,
    pub description: String,
    pub agent_structure_dir: PathBuf,
    pub default_model_id: String,
}

#[derive(Debug, Clone)]
pub struct SessionContextSnapshot {
    pub session_id: String,
    pub messages: Vec<Value>,
}

impl AgentService {
    pub fn new(agent_folder: &Path) -> Result<Self> {
        let agent_structure_dir = resolve_agent_structure_dir(agent_folder)?;
        let agent_yaml = agent_yaml_path(&agent_structure_dir);
        let agent_meta = read_agent_meta(&agent_yaml)?;
        let tool_policy = Arc::new(read_tool_policy(&agent_structure_dir)?);
        let (default_model_id, model_profiles) = read_model_registry(&agent_yaml)?;

        let session_store_dir =
            resolve_session_store_dir(&agent_meta.session_store_dir, &agent_structure_dir)?;
        std::fs::create_dir_all(&session_store_dir)
            .with_context(|| format!("create session store {}", session_store_dir.display()))?;
        let channel_state_dir = resolve_channel_state_dir(&agent_structure_dir)?;
        std::fs::create_dir_all(&channel_state_dir).with_context(|| {
            format!("create channel state store {}", channel_state_dir.display())
        })?;
        let session_leases = Arc::new(SessionLeaseRegistry::new());
        let pending_confirmations =
            Arc::new(PendingConfirmationRegistry::new(&agent_structure_dir));

        Ok(Self {
            agent_structure_dir,
            agent_meta,
            default_model_id,
            model_profiles,
            tool_policy,
            session_store_dir,
            channel_state_dir,
            session_leases,
            pending_confirmations,
            agents: Mutex::new(HashMap::new()),
        })
    }

    pub fn meta(&self) -> &AgentMeta {
        &self.agent_meta
    }

    pub fn agent_structure_dir(&self) -> &Path {
        &self.agent_structure_dir
    }

    pub fn channel_state_dir(&self) -> &Path {
        &self.channel_state_dir
    }

    pub fn session_leases(&self) -> Arc<SessionLeaseRegistry> {
        self.session_leases.clone()
    }

    pub fn pending_confirmations(&self) -> Arc<PendingConfirmationRegistry> {
        self.pending_confirmations.clone()
    }

    pub fn default_model_id(&self) -> &str {
        &self.default_model_id
    }

    pub fn model_profiles(&self) -> HashMap<String, ModelProfile> {
        self.model_profiles.clone()
    }

    pub fn profile_snapshot(&self) -> AgentProfileSnapshot {
        AgentProfileSnapshot {
            agent_id: self.agent_meta.agent_id.clone(),
            name: self.agent_meta.name.clone(),
            description: self.agent_meta.description.clone(),
            agent_structure_dir: self.agent_structure_dir.clone(),
            default_model_id: self.default_model_id.clone(),
        }
    }

    pub async fn new_session(&self, cwd: &str) -> Result<Arc<SessionAgent>> {
        self.new_session_with_options(cwd, None, None).await
    }

    pub async fn new_session_with_options(
        &self,
        cwd: &str,
        override_model: Option<&str>,
        override_reasoning_mode: Option<ReasoningMode>,
    ) -> Result<Arc<SessionAgent>> {
        let model_id = match override_model {
            Some(model) => {
                let trimmed = model.trim();
                if trimmed.is_empty() {
                    bail!("override_model must not be empty");
                }
                trimmed.to_string()
            }
            None => self.default_model_id.clone(),
        };
        if let Some(reasoning_mode) = override_reasoning_mode {
            self.validate_reasoning_mode(&model_id, reasoning_mode)?;
        }
        let agent = self
            .create_agent(CreateAgentArgs {
                cwd: cwd.to_string(),
                model_id,
                mode_id: self.agent_meta.policy_mode,
                max_running_turn: self.agent_meta.max_running_turn,
                runtime_tools: self.agent_meta.tools,
                tool_schemas: tool_schemas(&self.agent_meta.tools),
                reasoning_mode: override_reasoning_mode,
                ..CreateAgentArgs::default()
            })
            .await?;
        {
            let mut agents = self.agents.lock().await;
            agents.insert(agent.session_id().to_string(), agent.clone());
        }
        agent.persist_async().await?;
        Ok(agent)
    }

    pub async fn load_session(&self, session_id: &str) -> Result<Option<Arc<SessionAgent>>> {
        let cached = {
            let agents = self.agents.lock().await;
            agents.get(session_id).cloned()
        };
        let mut loaded_from_disk = false;
        let agent = match cached {
            Some(a) => a,
            None => match self.load_persisted_session(session_id).await? {
                Some(agent) => {
                    let mut agents = self.agents.lock().await;
                    agents.insert(agent.session_id().to_string(), agent.clone());
                    loaded_from_disk = true;
                    agent
                }
                None => return Ok(None),
            },
        };

        agent.mark_loaded(loaded_from_disk).await?;
        Ok(Some(agent))
    }

    pub async fn set_session_mode(
        &self,
        session_id: &str,
        mode_id: &str,
    ) -> Result<Option<Arc<SessionAgent>>> {
        let agents = self.agents.lock().await;
        let Some(agent) = agents.get(session_id).cloned() else {
            return Ok(None);
        };
        drop(agents);
        agent.set_mode(mode_id).await?;
        Ok(Some(agent))
    }

    pub async fn set_session_model(
        &self,
        session_id: &str,
        model_id: &str,
    ) -> Result<&'static str> {
        let agent = {
            let agents = self.agents.lock().await;
            agents
                .get(session_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Unknown session_id: {session_id}"))?
        };
        agent.set_model(model_id).await
    }

    pub async fn set_session_reasoning_mode(
        &self,
        session_id: &str,
        reasoning_mode: &str,
    ) -> Result<&'static str> {
        let agent = {
            let agents = self.agents.lock().await;
            agents
                .get(session_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Unknown session_id: {session_id}"))?
        };
        agent.set_reasoning_mode(reasoning_mode).await
    }

    pub async fn set_session_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<Option<SessionMetaPayload>> {
        match config_id {
            "policy_mode" => {
                let Some(agent) = self.set_session_mode(session_id, value).await? else {
                    return Ok(None);
                };
                Ok(Some(agent.session_meta_snapshot().await))
            }
            "model" => {
                self.set_session_model(session_id, value).await?;
                let agent = self
                    .get_session(session_id)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("session not found"))?;
                Ok(Some(agent.session_meta_snapshot().await))
            }
            "reasoning_mode" => {
                self.set_session_reasoning_mode(session_id, value).await?;
                let agent = self
                    .get_session(session_id)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("session not found"))?;
                Ok(Some(agent.session_meta_snapshot().await))
            }
            _ => bail!("Unsupported config option: {config_id}"),
        }
    }

    pub async fn get_session(&self, session_id: &str) -> Option<Arc<SessionAgent>> {
        let agents = self.agents.lock().await;
        agents.get(session_id).cloned()
    }

    pub async fn list_sessions(&self, cwd: Option<&str>) -> Vec<SessionMetaPayload> {
        let resolved_cwd = cwd.map(|p| {
            std::fs::canonicalize(p)
                .map(|r| r.to_string_lossy().into_owned())
                .unwrap_or_else(|_| p.to_string())
        });
        let mut merged: HashMap<String, SessionMetaPayload> = HashMap::new();

        for meta in self.list_persisted_session_meta() {
            if let Some(cwd_filter) = &resolved_cwd
                && &meta.cwd != cwd_filter
            {
                continue;
            }
            merged.insert(meta.session_id.clone(), meta);
        }

        let live_agents: Vec<Arc<SessionAgent>> = {
            let agents = self.agents.lock().await;
            agents.values().cloned().collect()
        };
        for agent in live_agents {
            if let Some(cwd_filter) = &resolved_cwd
                && agent.cwd() != cwd_filter
            {
                continue;
            }
            let meta = agent.session_meta_snapshot().await;
            merged.insert(meta.session_id.clone(), meta);
        }

        let mut result: Vec<SessionMetaPayload> = merged.into_values().collect();
        result.sort_by(|a, b| {
            b.updated_at
                .as_deref()
                .unwrap_or("")
                .cmp(a.updated_at.as_deref().unwrap_or(""))
        });
        result
    }

    pub async fn cancel(&self, session_id: &str) {
        let agent = {
            let agents = self.agents.lock().await;
            agents.get(session_id).cloned()
        };
        if let Some(agent) = agent {
            agent.cancel().await;
        }
    }

    pub async fn cancel_tool_call(&self, session_id: &str, tool_call_id: &str) -> bool {
        let agent = {
            let agents = self.agents.lock().await;
            agents.get(session_id).cloned()
        };
        match agent {
            None => false,
            Some(agent) => agent.cancel_tool_call(tool_call_id).await,
        }
    }

    pub async fn run_prompt(
        &self,
        session_id: &str,
        user_input: Value,
        user_blocks: Vec<Value>,
        emit_update: UpdateEmitter,
        request_permission: PermissionRequester,
    ) -> Result<String> {
        self.run_prompt_with_extra_tools(
            session_id,
            user_input,
            user_blocks,
            emit_update,
            request_permission,
            Vec::new(),
        )
        .await
    }

    pub async fn run_prompt_with_extra_tools(
        &self,
        session_id: &str,
        user_input: Value,
        user_blocks: Vec<Value>,
        emit_update: UpdateEmitter,
        request_permission: PermissionRequester,
        extra_tool_schemas: Vec<Value>,
    ) -> Result<String> {
        let agent = {
            let agents = self.agents.lock().await;
            agents
                .get(session_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Unknown session: {session_id}"))?
        };
        agent
            .run_prompt(
                user_input,
                user_blocks,
                emit_update,
                request_permission,
                extra_tool_schemas,
            )
            .await
    }

    pub async fn session_context_snapshot(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionContextSnapshot>> {
        let Some(session) = self.load_session(session_id).await? else {
            return Ok(None);
        };
        Ok(Some(SessionContextSnapshot {
            session_id: session_id.to_string(),
            messages: session.context_manager_snapshot().await,
        }))
    }

    // ── Internals ──────────────────────────────────────────────────────────

    async fn create_agent(&self, args: CreateAgentArgs) -> Result<Arc<SessionAgent>> {
        let CreateAgentArgs {
            cwd,
            model_id,
            mode_id,
            session_id,
            title,
            context_messages,
            context_usage,
            max_running_turn,
            runtime_tools,
            state,
            stop_reason,
            pending_model_id,
            reasoning_mode,
            pending_reasoning_mode,
            session_dir,
            tool_schemas,
        } = args;

        let resolved_cwd = std::fs::canonicalize(&cwd)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(cwd);
        let session_uuid = session_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let session_dir = session_dir.unwrap_or_else(|| self.new_session_dir(&session_uuid));
        let profile = self
            .model_profiles
            .get(&model_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Unknown model_id: {model_id}"))?;
        let runtime_factory_spec = self.runtime_factory_spec(runtime_tools);
        let factory = runtime_factory_spec.create_factory(&resolved_cwd, &profile);

        let create_args = CreateSessionArgs {
            session_id: session_uuid,
            cwd: resolved_cwd,
            session_dir,
            model_id,
            model_profiles: self.model_profiles.clone(),
            runtime_factory_spec,
            max_running_turn,
            runtime_tools,
            mode_id,
            state,
            stop_reason,
            title,
            context_messages,
            context_usage,
            pending_model_id,
            reasoning_mode,
            pending_reasoning_mode,
            tool_schemas,
        };
        factory.create_session_agent(create_args).await
    }

    fn runtime_factory_spec(&self, runtime_tools: AgentTools) -> RuntimeFactorySpec {
        RuntimeFactorySpec::new(
            self.agent_structure_dir.clone(),
            self.agent_meta.agent_id.clone(),
            runtime_tools,
            self.tool_policy.clone(),
            resolve_config_paths(
                &self.agent_meta.external_skills_dirs,
                &self.agent_structure_dir,
            ),
            resolve_config_paths(
                &self.agent_meta.external_rule_files,
                &self.agent_structure_dir,
            ),
        )
    }

    async fn load_persisted_session(&self, session_id: &str) -> Result<Option<Arc<SessionAgent>>> {
        let Some(session_dir) = self.find_persisted_session_dir(session_id) else {
            return Ok(None);
        };
        self.load_persisted_session_from_dir(&session_dir).await
    }

    async fn load_persisted_session_from_dir(
        &self,
        session_dir: &Path,
    ) -> Result<Option<Arc<SessionAgent>>> {
        let meta_path = session_dir.join(SESSION_META_FILE);
        let context_path = session_dir.join(SESSION_MODEL_CONTEXT_FILE);
        if !meta_path.exists() || !context_path.exists() {
            return Ok(None);
        }

        let meta: SessionMetaPayload = read_json_model(&meta_path)?;
        let context_payload: SessionModelContextPayload = read_json_model(&context_path)?;

        if !self.model_profiles.contains_key(&meta.model_id) {
            bail!("Unknown model_id in persisted session: {}", meta.model_id);
        }
        parse_policy_mode(meta.mode_id.as_str())?;
        if let Some(pending) = &meta.pending_model_id
            && !self.model_profiles.contains_key(pending)
        {
            bail!("Unknown pending_model_id in persisted session: {pending}");
        }
        self.validate_reasoning_mode(&meta.model_id, meta.reasoning_mode)?;
        if let Some(pending_reasoning) = meta.pending_reasoning_mode {
            let model_id = meta.pending_model_id.as_deref().unwrap_or(&meta.model_id);
            self.validate_reasoning_mode(model_id, pending_reasoning)?;
        }

        let args = CreateAgentArgs {
            cwd: meta.cwd.clone(),
            model_id: meta.model_id.clone(),
            mode_id: meta.mode_id,
            session_id: Some(meta.session_id.clone()),
            title: meta.title,
            context_messages: Some(context_payload.messages),
            context_usage: context_payload.usage,
            max_running_turn: meta.max_running_turn,
            runtime_tools: meta.runtime_tools,
            tool_schemas: meta.tool_schemas,
            state: meta.state,
            stop_reason: meta.stop_reason,
            pending_model_id: meta.pending_model_id,
            reasoning_mode: Some(meta.reasoning_mode),
            pending_reasoning_mode: meta.pending_reasoning_mode,
            session_dir: Some(session_dir.to_path_buf()),
        };
        let agent = self.create_agent(args).await?;
        Ok(Some(agent))
    }

    fn list_persisted_session_meta(&self) -> Vec<SessionMetaPayload> {
        let mut out: Vec<SessionMetaPayload> = Vec::new();
        collect_persisted_session_meta(&self.session_store_dir, &mut out);
        out
    }

    fn new_session_dir(&self, session_id: &str) -> PathBuf {
        let date = Local::now();
        self.session_store_dir
            .join(date.format("%Y").to_string())
            .join(date.format("%m").to_string())
            .join(date.format("%d").to_string())
            .join(session_id)
    }

    fn find_persisted_session_dir(&self, session_id: &str) -> Option<PathBuf> {
        for year_dir in date_child_dirs(&self.session_store_dir, 4) {
            for month_dir in date_child_dirs(&year_dir, 2) {
                for day_dir in date_child_dirs(&month_dir, 2) {
                    let session_dir = day_dir.join(session_id);
                    if persisted_session_dir_matches(&session_dir, session_id) {
                        return Some(session_dir);
                    }
                }
            }
        }
        None
    }

    fn validate_reasoning_mode(&self, model_id: &str, mode: ReasoningMode) -> Result<()> {
        let profile = self
            .model_profiles
            .get(model_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown model_id: {model_id}"))?;
        if !profile.reasoning_modes.contains(&mode) {
            bail!(
                "Unknown reasoning_mode `{}` for model `{}`",
                mode.as_str(),
                model_id
            );
        }
        Ok(())
    }
}

fn collect_persisted_session_meta(session_store_dir: &Path, out: &mut Vec<SessionMetaPayload>) {
    for year_dir in date_child_dirs(session_store_dir, 4) {
        for month_dir in date_child_dirs(&year_dir, 2) {
            for day_dir in date_child_dirs(&month_dir, 2) {
                let Ok(entries) = std::fs::read_dir(&day_dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let session_dir = entry.path();
                    if !session_dir.is_dir() {
                        continue;
                    }
                    let meta_path = session_dir.join(SESSION_META_FILE);
                    if let Ok(meta) = read_json_model::<SessionMetaPayload>(&meta_path) {
                        out.push(meta);
                    }
                }
            }
        }
    }
}

fn date_child_dirs(parent: &Path, name_len: usize) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.len() == name_len && name.chars().all(|ch| ch.is_ascii_digit())
                    })
        })
        .collect()
}

fn persisted_session_dir_matches(dir: &Path, session_id: &str) -> bool {
    let meta_path = dir.join(SESSION_META_FILE);
    let context_path = dir.join(SESSION_MODEL_CONTEXT_FILE);
    if !meta_path.is_file() || !context_path.is_file() {
        return false;
    }
    read_json_model::<SessionMetaPayload>(&meta_path)
        .is_ok_and(|meta| meta.session_id == session_id)
}

fn resolve_config_paths(paths: &[String], base_dir: &Path) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|path| {
            let candidate = PathBuf::from(path);
            let joined = if candidate.is_absolute() {
                candidate
            } else {
                base_dir.join(candidate)
            };
            std::fs::canonicalize(&joined).unwrap_or(joined)
        })
        .collect()
}

#[derive(Clone)]
struct CreateAgentArgs {
    cwd: String,
    model_id: String,
    mode_id: PolicyMode,
    session_id: Option<String>,
    session_dir: Option<PathBuf>,
    title: Option<String>,
    context_messages: Option<Vec<Value>>,
    context_usage: Option<ContextUsageSnapshot>,
    max_running_turn: Option<u32>,
    runtime_tools: AgentTools,
    tool_schemas: Vec<Value>,
    state: AgentState,
    stop_reason: Option<StopReason>,
    pending_model_id: Option<String>,
    reasoning_mode: Option<ReasoningMode>,
    pending_reasoning_mode: Option<ReasoningMode>,
}

impl Default for CreateAgentArgs {
    fn default() -> Self {
        Self {
            cwd: String::new(),
            model_id: String::new(),
            mode_id: PolicyMode::Confirm,
            session_id: None,
            session_dir: None,
            title: None,
            context_messages: None,
            context_usage: None,
            max_running_turn: None,
            runtime_tools: AgentTools::default(),
            tool_schemas: Vec::new(),
            state: AgentState::Idle,
            stop_reason: None,
            pending_model_id: None,
            reasoning_mode: None,
            pending_reasoning_mode: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::ToolSwitch;
    use tempfile::tempdir;

    fn write_agent_folder(root: &Path, max_running_turn: u32, tools: &str) {
        write_agent_folder_with_dirs(root, max_running_turn, tools, ".sessions")
    }

    fn write_agent_folder_with_dirs(
        root: &Path,
        max_running_turn: u32,
        tools: &str,
        session_store_dir: &str,
    ) {
        let resources_dir = root.join("resources");
        let prompt_dir = resources_dir.join("prompt");
        let skills_dir = resources_dir.join("skills");
        std::fs::create_dir_all(&prompt_dir).unwrap();
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(
            root.join("agent.yaml"),
            format!(
                "\
agent_id: test-agent
name: Test Agent
description: Test
max_running_turn: {max_running_turn}
policy_mode: confirm
session_store_dir: {session_store_dir}
tools:
{tools}
model:
  default_model_id: mock-model
  models:
    - model_name: mock-model
      provider: deepseek
      model_id: deepseek-v4-pro
      api_key: test-key
      default_reasoning_mode: auto
"
            ),
        )
        .unwrap();
        std::fs::write(prompt_dir.join("system.md"), "You are a test agent.\n").unwrap();
    }

    #[tokio::test]
    async fn persisted_session_keeps_tools_and_max_turn_snapshot() {
        let tmp = tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        write_agent_folder(
            &agent_dir,
            7,
            "  file_edit: disable\n  terminal: enable\n  subagent: disable",
        );
        let cwd = tmp.path().to_string_lossy().to_string();

        let service = AgentService::new(&agent_dir).unwrap();
        let session = service.new_session(&cwd).await.unwrap();
        let session_id = session.session_id().to_string();
        drop(session);
        drop(service);

        write_agent_folder(
            &agent_dir,
            3,
            "  file_edit: enable\n  terminal: enable\n  subagent: enable",
        );
        let service = AgentService::new(&agent_dir).unwrap();
        let loaded = service.load_session(&session_id).await.unwrap().unwrap();
        let snapshot = loaded.session_snapshot().await;

        assert_eq!(snapshot.max_running_turn, Some(7));
        assert_eq!(snapshot.runtime_tools.file_edit, ToolSwitch::Disable);
        assert_eq!(snapshot.runtime_tools.terminal, ToolSwitch::Enable);
        assert_eq!(snapshot.runtime_tools.subagent, ToolSwitch::Disable);
    }

    #[tokio::test]
    async fn service_uses_configured_session_dirs() {
        let tmp = tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        write_agent_folder_with_dirs(
            &agent_dir,
            7,
            "  file_edit: enable\n  terminal: enable\n  subagent: enable",
            "agent-sessions",
        );

        let service = AgentService::new(&agent_dir).unwrap();
        assert!(
            service
                .channel_state_dir()
                .ends_with("runtime/channel_state")
        );
        assert!(service.channel_state_dir().is_dir());

        let cwd = tmp.path().to_string_lossy().to_string();
        let session = service.new_session(&cwd).await.unwrap();
        let session_root = std::fs::canonicalize(agent_dir.join("agent-sessions")).unwrap();
        let relative = session.session_dir().strip_prefix(session_root).unwrap();
        let parts = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0].len(), 4);
        assert_eq!(parts[1].len(), 2);
        assert_eq!(parts[2].len(), 2);
        assert!(
            parts[..3]
                .iter()
                .all(|part| part.chars().all(|ch| ch.is_ascii_digit()))
        );
        assert_eq!(parts[3], session.session_id());
    }
}
