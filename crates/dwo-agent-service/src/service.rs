use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use dwo_context::{ContextManager, SystemPromptBuilder};
use dwo_model_client::{ConfiguredModelClient, ModelClient, ModelSelection};
use dwo_tools::{FileEditManager, PolicyConfig, SessionMode, ToolManager, ToolPolicyEngine};
use tokio::sync::Mutex;

use crate::error::AgentServiceError;
use crate::events::{ClientTranscriptEvent, SessionEventPayload};
use crate::profile::LoadedAgentProfile;
use crate::record::{
    SessionConfigUpdate, SessionId, SessionLlmSettings, SessionRecord, title_from_user_content,
};
use crate::repository::SessionRepository;
use crate::session::SessionAgent;

pub struct NewSession {
    pub id: Option<SessionId>,
    pub title: Option<String>,
    pub cwd: PathBuf,
    pub mode: SessionMode,
    pub llm: SessionLlmSettings,
}

pub struct AgentService {
    repository: Arc<dyn SessionRepository>,
    model: Arc<dyn ModelClient>,
    policy: Arc<ToolPolicyEngine>,
    file_edit: Arc<FileEditManager>,
    profile_root: Option<PathBuf>,
    loaded: Mutex<LoadedSessionRegistry>,
}

#[derive(Default)]
struct LoadedSessionRegistry {
    agents: HashMap<SessionId, Arc<SessionAgent>>,
    deleting: HashSet<SessionId>,
}

impl AgentService {
    pub fn new(
        repository: Arc<dyn SessionRepository>,
        model: Arc<dyn ModelClient>,
        policy: PolicyConfig,
    ) -> Self {
        Self::build(repository, model, policy, None)
    }

    pub fn with_profile_root(
        repository: Arc<dyn SessionRepository>,
        model: Arc<dyn ModelClient>,
        policy: PolicyConfig,
        profile_root: impl Into<PathBuf>,
    ) -> Result<Self, AgentServiceError> {
        let profile_root =
            std::fs::canonicalize(profile_root.into()).map_err(anyhow::Error::from)?;
        Ok(Self::build(repository, model, policy, Some(profile_root)))
    }

    pub fn from_profile(
        repository: Arc<dyn SessionRepository>,
        profile: LoadedAgentProfile,
        policy: PolicyConfig,
    ) -> Result<Self, AgentServiceError> {
        let model = ConfiguredModelClient::from_resolved(profile.models)
            .map_err(|error| AgentServiceError::InvalidConfig(error.to_string()))?;
        Ok(Self::build(repository, model, policy, Some(profile.root)))
    }

    fn build(
        repository: Arc<dyn SessionRepository>,
        model: Arc<dyn ModelClient>,
        policy: PolicyConfig,
        profile_root: Option<PathBuf>,
    ) -> Self {
        Self {
            repository,
            model,
            policy: Arc::new(ToolPolicyEngine::new(policy)),
            file_edit: Arc::new(FileEditManager::new()),
            profile_root,
            loaded: Mutex::new(LoadedSessionRegistry::default()),
        }
    }

    pub async fn create(
        &self,
        new_session: NewSession,
    ) -> Result<Arc<SessionAgent>, AgentServiceError> {
        self.model
            .validate_selection(&ModelSelection {
                model: new_session.llm.model.clone(),
                reasoning: new_session.llm.reasoning.clone(),
            })
            .map_err(|error| AgentServiceError::InvalidConfig(error.to_string()))?;
        let cwd = std::fs::canonicalize(&new_session.cwd).map_err(anyhow::Error::from)?;
        let explicit_title = new_session
            .title
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty());
        let automatic_title = explicit_title.is_none();
        let title = explicit_title.unwrap_or_else(|| default_session_title(&cwd));
        let prompt_builder = self.prompt_builder(cwd.clone());
        let context = ContextManager::initialize(&prompt_builder)
            .map_err(anyhow::Error::from)?
            .into_context();
        let mut record = SessionRecord::new(
            new_session.id.unwrap_or_default(),
            title,
            cwd,
            new_session.mode,
            new_session.llm,
        );
        if automatic_title {
            record.enable_auto_title();
        }
        record.context = context;
        self.repository.save(&record).await?;
        self.load_record(record).await
    }

    pub async fn load(&self, id: &SessionId) -> Result<Arc<SessionAgent>, AgentServiceError> {
        {
            let loaded = self.loaded.lock().await;
            if loaded.deleting.contains(id) {
                return Err(AgentServiceError::SessionDeleting(id.clone()));
            }
            if let Some(agent) = loaded.agents.get(id).cloned() {
                return Ok(agent);
            }
        }
        let record = self
            .repository
            .load(id)
            .await?
            .ok_or_else(|| AgentServiceError::SessionNotFound(id.clone()))?;
        self.load_record(record).await
    }

    pub async fn list(&self) -> Result<Vec<SessionRecord>, AgentServiceError> {
        let deleting = self.loaded.lock().await.deleting.clone();
        let mut records = self.repository.list().await?;
        records.retain(|record| !deleting.contains(&record.info.id));
        for record in &mut records {
            if !record.info.title.trim().is_empty() {
                continue;
            }
            let transcript = self.repository.load_transcript(&record.info.id).await?;
            if repair_empty_title(record, &transcript) {
                self.repository.save(record).await?;
            }
        }
        Ok(records)
    }

    pub async fn close(&self, id: &SessionId) -> Result<(), AgentServiceError> {
        let agent = {
            let loaded = self.loaded.lock().await;
            if loaded.deleting.contains(id) {
                return Err(AgentServiceError::SessionDeleting(id.clone()));
            }
            loaded.agents.get(id).cloned()
        };
        let Some(agent) = agent else {
            return if self.repository.load(id).await?.is_some() {
                Ok(())
            } else {
                Err(AgentServiceError::SessionNotFound(id.clone()))
            };
        };
        agent.close().await?;
        let mut loaded = self.loaded.lock().await;
        if loaded
            .agents
            .get(id)
            .is_some_and(|current| Arc::ptr_eq(current, &agent))
        {
            loaded.agents.remove(id);
        }
        Ok(())
    }

    pub async fn delete(&self, id: &SessionId) -> Result<(), AgentServiceError> {
        let agent = {
            let mut loaded = self.loaded.lock().await;
            if !loaded.deleting.insert(id.clone()) {
                return Err(AgentServiceError::SessionDeleting(id.clone()));
            }
            loaded.agents.remove(id)
        };
        let was_loaded = agent.is_some();

        if let Some(agent) = agent
            && let Err(error) = agent.close().await
        {
            self.loaded.lock().await.deleting.remove(id);
            return Err(error);
        }

        let deleted = self.repository.delete(id).await;
        self.loaded.lock().await.deleting.remove(id);
        let deleted = deleted?;
        if !was_loaded && !deleted {
            return Err(AgentServiceError::SessionNotFound(id.clone()));
        }
        Ok(())
    }

    pub async fn set_config(
        &self,
        id: &SessionId,
        update: SessionConfigUpdate,
    ) -> Result<(), AgentServiceError> {
        self.load(id).await?.set_config(update).await
    }

    pub async fn shutdown(&self) {
        let agents: Vec<_> = self
            .loaded
            .lock()
            .await
            .agents
            .drain()
            .map(|(_, agent)| agent)
            .collect();
        for agent in agents {
            let _ = agent.close().await;
        }
    }

    async fn load_record(
        &self,
        mut record: SessionRecord,
    ) -> Result<Arc<SessionAgent>, AgentServiceError> {
        let transcript = self.repository.load_transcript(&record.info.id).await?;
        let prompt_builder = self.prompt_builder(record.info.cwd.clone());
        let mut record_changed = repair_empty_title(&mut record, &transcript);
        if !record.context.system_prompt.is_initialized() {
            record.context = ContextManager::initialize(&prompt_builder)
                .map_err(anyhow::Error::from)?
                .into_context();
            record_changed = true;
        }
        let mut loaded = self.loaded.lock().await;
        if loaded.deleting.contains(&record.info.id) {
            return Err(AgentServiceError::SessionDeleting(record.info.id));
        }
        if let Some(agent) = loaded.agents.get(&record.info.id).cloned() {
            return Ok(agent);
        }
        let tools = Arc::new(ToolManager::new(
            record.info.cwd.clone(),
            self.policy.clone(),
            self.file_edit.clone(),
        )?);
        let previous_tokens = record.context.usage.current_tokens;
        let mut context = ContextManager::new(record.context.clone());
        let current_tokens = context.refresh_usage(&tools.schemas());
        record.context = context.into_context();
        if current_tokens != previous_tokens {
            record_changed = true;
        }
        if record_changed {
            self.repository.save(&record).await?;
        }
        let agent = SessionAgent::spawn(
            record.clone(),
            transcript,
            self.repository.clone(),
            self.model.clone(),
            tools,
            prompt_builder,
        );
        loaded.agents.insert(record.info.id, agent.clone());
        Ok(agent)
    }

    fn prompt_builder(&self, cwd: PathBuf) -> SystemPromptBuilder {
        SystemPromptBuilder::new(self.profile_root.clone(), cwd)
            .with_tool_prompt(dwo_tools::prompt::combined())
    }
}

fn default_session_title(cwd: &std::path::Path) -> String {
    cwd.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "session".to_string())
}

fn repair_empty_title(record: &mut SessionRecord, transcript: &[ClientTranscriptEvent]) -> bool {
    if !record.info.title.trim().is_empty() {
        return false;
    }
    let Some(title) = transcript.iter().find_map(|event| match &event.payload {
        SessionEventPayload::UserPromptSubmitted { content, .. } => {
            title_from_user_content(content)
        }
        _ => None,
    }) else {
        return false;
    };
    record.set_automatic_title(title);
    true
}
