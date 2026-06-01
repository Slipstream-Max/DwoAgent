//! Simple Agent runtime with session/state management.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::factory::{CreateSessionArgs, RuntimeFactoryBuilder, SessionAgentFactory};
use super::session::{
    SESSION_CLIENT_TRANSCRIPT_FILE, SESSION_META_FILE, SESSION_MODEL_CONTEXT_FILE,
};
use super::session_agent::SessionAgent;
use crate::config::loader::{
    read_agent_meta, read_json_model, read_model_registry, resolve_agent_structure_dir,
    resolve_session_store_dir,
};
use crate::config::models::{
    AgentMeta, AgentState, AgentTools, ModelProfile, PolicyMode, ReasoningMode, SessionMetaPayload,
    SessionModelContextPayload, SessionTranscriptEvent, StopReason,
};
use crate::tools::{
    subagent_tool_runtime::{PermissionRequester, UpdateEmitter},
    tool_schemas,
};
use crate::utils::policy::parse_policy_mode;

/// Own session lifecycle + run loop state transitions.
pub struct AgentService {
    agent_structure_dir: PathBuf,
    agent_meta: AgentMeta,
    default_model_id: String,
    model_profiles: HashMap<String, ModelProfile>,
    session_store_dir: PathBuf,
    agents: Mutex<HashMap<String, Arc<SessionAgent>>>,
}

impl AgentService {
    pub fn new(agent_folder: &Path) -> Result<Self> {
        let agent_structure_dir = resolve_agent_structure_dir(agent_folder)?;
        let agent_meta = read_agent_meta(&agent_structure_dir.join("agent.yaml"))?;
        let (default_model_id, model_profiles) =
            read_model_registry(&agent_structure_dir.join("model.yaml"))?;

        let session_store_dir =
            resolve_session_store_dir(&agent_meta.session_store_dir, &agent_structure_dir)?;
        std::fs::create_dir_all(&session_store_dir)
            .with_context(|| format!("create session store {}", session_store_dir.display()))?;

        Ok(Self {
            agent_structure_dir,
            agent_meta,
            default_model_id,
            model_profiles,
            session_store_dir,
            agents: Mutex::new(HashMap::new()),
        })
    }

    pub fn meta(&self) -> &AgentMeta {
        &self.agent_meta
    }

    pub fn agent_structure_dir(&self) -> &Path {
        &self.agent_structure_dir
    }

    pub fn default_model_id(&self) -> &str {
        &self.default_model_id
    }

    pub fn model_profiles(&self) -> HashMap<String, ModelProfile> {
        self.model_profiles.clone()
    }

    pub async fn new_session(&self, cwd: &str) -> Result<Arc<SessionAgent>> {
        let agent = self
            .create_agent(CreateAgentArgs {
                cwd: cwd.to_string(),
                model_id: self.default_model_id.clone(),
                mode_id: self.agent_meta.policy_mode,
                max_running_turn: self.agent_meta.max_running_turn,
                runtime_tools: self.agent_meta.runtime_tools,
                tool_schemas: tool_schemas(&self.agent_meta.runtime_tools),
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

    pub async fn load_or_create_channel_session(
        &self,
        cwd: &str,
        session_dir: PathBuf,
        channel_tools: Vec<Value>,
        override_model: Option<&str>,
        override_reasoning_mode: Option<ReasoningMode>,
    ) -> Result<Arc<SessionAgent>> {
        let meta_path = session_dir.join(SESSION_META_FILE);
        if meta_path.is_file() {
            let meta: SessionMetaPayload = read_json_model(&meta_path)?;
            if let Some(agent) = self.agents.lock().await.get(&meta.session_id).cloned() {
                return Ok(agent);
            }
        }

        if let Some(agent) = self.load_persisted_session_from_dir(&session_dir).await? {
            {
                let mut agents = self.agents.lock().await;
                agents.insert(agent.session_id().to_string(), agent.clone());
            }
            agent.mark_loaded(true).await?;
            return Ok(agent);
        }

        let model_id = match override_model {
            Some(model) => {
                let trimmed = model.trim();
                if trimmed.is_empty() {
                    bail!("channel override_model must not be empty");
                }
                trimmed.to_string()
            }
            None => self.default_model_id.clone(),
        };
        if let Some(reasoning_mode) = override_reasoning_mode {
            self.validate_reasoning_mode(&model_id, reasoning_mode)?;
        }

        let mut tool_schemas_for_session = tool_schemas(&self.agent_meta.runtime_tools);
        tool_schemas_for_session.extend(channel_tools);

        let agent = self
            .create_agent(CreateAgentArgs {
                cwd: cwd.to_string(),
                model_id,
                mode_id: self.agent_meta.policy_mode,
                max_running_turn: self.agent_meta.max_running_turn,
                runtime_tools: self.agent_meta.runtime_tools,
                tool_schemas: tool_schemas_for_session,
                reasoning_mode: override_reasoning_mode,
                session_dir: Some(session_dir),
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
            let session = agent.session_snapshot().await;
            merged.insert(
                session.session_id.clone(),
                SessionMetaPayload {
                    session_id: session.session_id,
                    cwd: session.cwd,
                    title: session.title,
                    model_id: session.model_id,
                    mode_id: session.mode_id,
                    state: session.state,
                    stop_reason: session.stop_reason,
                    updated_at: session.updated_at,
                    max_running_turn: session.max_running_turn,
                    runtime_tools: session.runtime_tools,
                    tool_schemas: session.tool_schemas,
                    pending_model_id: session.pending_model_id,
                    reasoning_mode: session.reasoning_mode,
                    pending_reasoning_mode: session.pending_reasoning_mode,
                },
            );
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
        let agent = {
            let agents = self.agents.lock().await;
            agents
                .get(session_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Unknown session: {session_id}"))?
        };
        agent
            .run_prompt(user_input, user_blocks, emit_update, request_permission)
            .await
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
            transcript_events,
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
        let session_dir = session_dir.unwrap_or_else(|| self.session_store_dir.join(&session_uuid));
        let profile = self
            .model_profiles
            .get(&model_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Unknown model_id: {model_id}"))?;
        let factory = self.create_runtime_factory(&resolved_cwd, &profile, runtime_tools)?;

        let runtime_factory_builder: RuntimeFactoryBuilder = {
            let service = self.snapshot_shape(runtime_tools);
            Arc::new(move |cwd: &str, profile: &ModelProfile| {
                service.create_runtime_factory(cwd, profile)
            })
        };

        let create_args = CreateSessionArgs {
            session_id: session_uuid,
            cwd: resolved_cwd,
            session_dir,
            model_id,
            model_profiles: self.model_profiles.clone(),
            runtime_factory_builder,
            max_running_turn,
            runtime_tools,
            mode_id,
            state,
            stop_reason,
            title,
            context_messages,
            transcript_events,
            pending_model_id,
            reasoning_mode,
            pending_reasoning_mode,
            tool_schemas,
        };
        factory.create_session_agent(create_args).await
    }

    fn create_runtime_factory(
        &self,
        cwd: &str,
        profile: &ModelProfile,
        runtime_tools: AgentTools,
    ) -> Result<SessionAgentFactory> {
        let mcp_config_path = self.agent_structure_dir.join("resources").join("mcp.json");
        Ok(SessionAgentFactory::new(
            &self.agent_structure_dir,
            &self.agent_meta.agent_id,
            &mcp_config_path,
            cwd.to_string(),
            profile.context_window,
            profile.compact_threshold,
            profile.config.clone(),
            profile.capabilities,
            profile.default_reasoning_mode.as_str(),
            runtime_tools,
        ))
    }

    fn snapshot_shape(&self, runtime_tools: AgentTools) -> AgentServiceShape {
        AgentServiceShape {
            agent_structure_dir: self.agent_structure_dir.clone(),
            agent_id: self.agent_meta.agent_id.clone(),
            runtime_tools,
        }
    }

    async fn load_persisted_session(&self, session_id: &str) -> Result<Option<Arc<SessionAgent>>> {
        let session_dir = self.session_store_dir.join(session_id);
        self.load_persisted_session_from_dir(&session_dir).await
    }

    async fn load_persisted_session_from_dir(
        &self,
        session_dir: &Path,
    ) -> Result<Option<Arc<SessionAgent>>> {
        let meta_path = session_dir.join(SESSION_META_FILE);
        let context_path = session_dir.join(SESSION_MODEL_CONTEXT_FILE);
        let transcript_path = session_dir.join(SESSION_CLIENT_TRANSCRIPT_FILE);
        if !meta_path.exists() || !context_path.exists() || !transcript_path.exists() {
            return Ok(None);
        }

        let meta: SessionMetaPayload = read_json_model(&meta_path)?;
        let context_payload: SessionModelContextPayload = read_json_model(&context_path)?;
        let transcript_events = read_transcript_jsonl(&transcript_path)?;

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
            transcript_events,
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
        let Ok(entries) = std::fs::read_dir(&self.session_store_dir) else {
            return Vec::new();
        };
        let mut out: Vec<SessionMetaPayload> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let meta_path = path.join(SESSION_META_FILE);
            if !meta_path.is_file() {
                continue;
            }
            let Ok(meta) = read_json_model::<SessionMetaPayload>(&meta_path) else {
                continue;
            };
            out.push(meta);
        }
        out
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

#[derive(Clone)]
struct AgentServiceShape {
    agent_structure_dir: PathBuf,
    agent_id: String,
    runtime_tools: AgentTools,
}

impl AgentServiceShape {
    fn create_runtime_factory(
        &self,
        cwd: &str,
        profile: &ModelProfile,
    ) -> Result<SessionAgentFactory> {
        let mcp_config_path = self.agent_structure_dir.join("resources").join("mcp.json");
        Ok(SessionAgentFactory::new(
            &self.agent_structure_dir,
            &self.agent_id,
            &mcp_config_path,
            cwd.to_string(),
            profile.context_window,
            profile.compact_threshold,
            profile.config.clone(),
            profile.capabilities,
            profile.default_reasoning_mode.as_str(),
            self.runtime_tools,
        ))
    }
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
    transcript_events: Vec<Value>,
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
            transcript_events: Vec::new(),
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

fn read_transcript_jsonl(path: &Path) -> Result<Vec<Value>> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut events: Vec<Value> = Vec::new();
    for line in raw.lines() {
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let loaded: Value = serde_json::from_str(text)
            .with_context(|| format!("parse JSONL line in {}", path.display()))?;
        let event = deserialize_transcript_event(loaded)?;
        events.push(event);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::ToolSwitch;
    use tempfile::tempdir;

    fn write_agent_folder(root: &Path, max_running_turn: u32, tools: &str) {
        let resources_dir = root.join("resources");
        let agents_dir = resources_dir.join("agents");
        let skills_dir = resources_dir.join("skills");
        std::fs::create_dir_all(&agents_dir).unwrap();
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
session_store_dir: .sessions
tools:
{tools}
"
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("model.yaml"),
            "\
default_model_id: mock-model
models:
  - model_name: mock-model
    provider: deepseek
    model_id: deepseek-v4-pro
    api_key: test-key
    default_reasoning_mode: auto
",
        )
        .unwrap();
        std::fs::write(
            agents_dir.join("test-agent.agent.md"),
            "You are a test agent.\n",
        )
        .unwrap();
        std::fs::write(resources_dir.join("mcp.json"), r#"{"mcpServers": {}}"#).unwrap();
    }

    #[tokio::test]
    async fn persisted_session_keeps_tools_and_max_turn_snapshot() {
        let tmp = tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        write_agent_folder(
            &agent_dir,
            7,
            "  mcp: disable\n  file_edit: disable\n  terminal: enable\n  subagent: disable",
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
            "  mcp: enable\n  file_edit: enable\n  terminal: enable\n  subagent: enable",
        );
        let service = AgentService::new(&agent_dir).unwrap();
        let loaded = service.load_session(&session_id).await.unwrap().unwrap();
        let snapshot = loaded.session_snapshot().await;

        assert_eq!(snapshot.max_running_turn, Some(7));
        assert_eq!(snapshot.runtime_tools.mcp, ToolSwitch::Disable);
        assert_eq!(snapshot.runtime_tools.file_edit, ToolSwitch::Disable);
        assert_eq!(snapshot.runtime_tools.terminal, ToolSwitch::Enable);
        assert_eq!(snapshot.runtime_tools.subagent, ToolSwitch::Disable);
    }
}

fn deserialize_transcript_event(value: Value) -> Result<Value> {
    let event: SessionTranscriptEvent =
        serde_json::from_value(value).context("validate SessionTranscriptEvent")?;
    serde_json::to_value(&event).context("serialize SessionTranscriptEvent")
}
