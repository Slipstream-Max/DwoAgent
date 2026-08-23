use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use dwo_context::{
    CompactionView, ContextManager, ContextMessage, PromptBuildError, SkillSnapshot,
    SystemPromptBuilder,
};
use dwo_model_client::{
    ConfiguredModelClient, ModelClient, ModelClientConfig, ModelClientError, ModelLimits,
    ModelReply, ModelSelection, ModelStreamEvent, SummaryReply,
};
use dwo_tools::{FileEditManager, PolicyConfig, SessionMode, ToolManager, ToolPolicyEngine};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::error::AgentServiceError;
use crate::events::{
    ClientTranscriptEvent, RuntimePhase, SessionEventPayload, SessionStatusSnapshot,
    SessionUsageSnapshot,
};
use crate::profile::LoadedAgentProfile;
use crate::record::{
    SessionConfigUpdate, SessionId, SessionLlmSettings, SessionRecord, title_from_user_content,
};
use crate::repository::SessionRepository;
use crate::session::{EndpointId, PromptAccepted, SessionAgent};
use dwo_context::MessageContent;

pub struct NewSession {
    pub id: Option<SessionId>,
    pub parent_session_id: Option<SessionId>,
    pub title: Option<String>,
    pub automation_job: Option<String>,
    pub cwd: PathBuf,
    pub mode: SessionMode,
    pub llm: SessionLlmSettings,
    pub ephemeral: bool,
}

pub struct AgentService {
    repository: Arc<dyn SessionRepository>,
    model: Arc<ReloadableModelClient>,
    policy: Arc<ToolPolicyEngine>,
    file_edit: Arc<FileEditManager>,
    profile_root: Option<PathBuf>,
    external_skill_dirs: Arc<RwLock<Vec<PathBuf>>>,
    max_model_steps: Arc<AtomicUsize>,
    loaded: Mutex<LoadedSessionRegistry>,
    operations: Mutex<HashMap<SessionId, Arc<Mutex<()>>>>,
}

struct ReloadableModelClient {
    current: RwLock<Arc<dyn ModelClient>>,
}

impl ReloadableModelClient {
    fn new(model: Arc<dyn ModelClient>) -> Self {
        Self {
            current: RwLock::new(model),
        }
    }

    fn current(&self) -> Arc<dyn ModelClient> {
        self.current
            .read()
            .expect("reloadable model client lock poisoned")
            .clone()
    }

    fn replace(&self, model: Arc<dyn ModelClient>) {
        *self
            .current
            .write()
            .expect("reloadable model client lock poisoned") = model;
    }
}

#[async_trait]
impl ModelClient for ReloadableModelClient {
    fn model_limits(&self, model: &str) -> Result<ModelLimits, ModelClientError> {
        self.current().model_limits(model)
    }

    fn provider_id(&self, model: &str) -> Result<String, ModelClientError> {
        self.current().provider_id(model)
    }

    fn context_owner_id(&self, model: &str) -> Result<String, ModelClientError> {
        self.current().context_owner_id(model)
    }

    fn supports_image_input(&self, model: &str) -> Result<bool, ModelClientError> {
        self.current().supports_image_input(model)
    }

    fn reasoning_modes(&self, model: &str) -> Result<Vec<String>, ModelClientError> {
        self.current().reasoning_modes(model)
    }

    fn validate_selection(&self, selection: &ModelSelection) -> Result<(), ModelClientError> {
        self.current().validate_selection(selection)
    }

    async fn stream_turn(
        &self,
        selection: ModelSelection,
        messages: &[ContextMessage],
        tools: &[serde_json::Value],
        events: mpsc::UnboundedSender<ModelStreamEvent>,
        cancellation: &CancellationToken,
    ) -> Result<ModelReply, ModelClientError> {
        self.current()
            .stream_turn(selection, messages, tools, events, cancellation)
            .await
    }

    async fn complete(
        &self,
        selection: ModelSelection,
        messages: Vec<ContextMessage>,
        cancellation: CancellationToken,
    ) -> Result<ModelReply, ModelClientError> {
        self.current()
            .complete(selection, messages, cancellation)
            .await
    }

    async fn summarize(
        &self,
        selection: ModelSelection,
        view: CompactionView,
        cancellation: CancellationToken,
    ) -> Result<SummaryReply, ModelClientError> {
        self.current()
            .summarize(selection, view, cancellation)
            .await
    }
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
        Self::build(
            repository,
            model,
            policy,
            None,
            crate::DEFAULT_MAX_MODEL_STEPS,
        )
    }

    pub fn with_profile_root(
        repository: Arc<dyn SessionRepository>,
        model: Arc<dyn ModelClient>,
        policy: PolicyConfig,
        profile_root: impl Into<PathBuf>,
    ) -> Result<Self, AgentServiceError> {
        let profile_root =
            std::fs::canonicalize(profile_root.into()).map_err(anyhow::Error::from)?;
        Ok(Self::build(
            repository,
            model,
            policy,
            Some(profile_root),
            crate::DEFAULT_MAX_MODEL_STEPS,
        ))
    }

    pub fn from_profile(
        repository: Arc<dyn SessionRepository>,
        profile: LoadedAgentProfile,
        policy: PolicyConfig,
    ) -> Result<Self, AgentServiceError> {
        let model = ConfiguredModelClient::from_resolved(profile.models)
            .map_err(|error| AgentServiceError::InvalidConfig(error.to_string()))?;
        let service = Self::build(
            repository,
            model,
            policy,
            Some(profile.root),
            profile.config.max_model_steps,
        );
        service.replace_external_skill_dirs(profile.external_skill_dirs);
        Ok(service)
    }

    fn build(
        repository: Arc<dyn SessionRepository>,
        model: Arc<dyn ModelClient>,
        policy: PolicyConfig,
        profile_root: Option<PathBuf>,
        max_model_steps: usize,
    ) -> Self {
        Self {
            repository,
            model: Arc::new(ReloadableModelClient::new(model)),
            policy: Arc::new(ToolPolicyEngine::new(policy)),
            file_edit: Arc::new(FileEditManager::new()),
            profile_root,
            external_skill_dirs: Arc::new(RwLock::new(Vec::new())),
            max_model_steps: Arc::new(AtomicUsize::new(max_model_steps)),
            loaded: Mutex::new(LoadedSessionRegistry::default()),
            operations: Mutex::new(HashMap::new()),
        }
    }

    fn replace_model(&self, model: Arc<dyn ModelClient>) {
        self.model.replace(model);
    }

    pub fn replace_external_skill_dirs(&self, dirs: Vec<PathBuf>) {
        *self
            .external_skill_dirs
            .write()
            .expect("external skill dirs lock poisoned") = dirs;
    }

    pub fn replace_max_model_steps(&self, max_model_steps: usize) {
        self.max_model_steps
            .store(max_model_steps, Ordering::Release);
    }

    pub fn max_model_steps(&self) -> usize {
        self.max_model_steps.load(Ordering::Acquire)
    }

    pub fn skill_snapshots(&self, cwd: &Path) -> Result<Vec<SkillSnapshot>, PromptBuildError> {
        self.prompt_builder(cwd.to_path_buf()).scan_skills()
    }

    pub fn replace_models(&self, config: ModelClientConfig) -> Result<(), AgentServiceError> {
        let model = ConfiguredModelClient::from_resolved(config)
            .map_err(|error| AgentServiceError::InvalidConfig(error.to_string()))?;
        self.replace_model(model);
        Ok(())
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
        record.set_parent_session_id(new_session.parent_session_id);
        record.set_automation_job(new_session.automation_job);
        record.info.ephemeral = new_session.ephemeral;
        if automatic_title {
            record.enable_auto_title();
        }
        record.context = context;
        let operation = self.session_operation(&record.info.id).await;
        let _operation = operation.lock().await;
        self.repository.save(&record).await?;
        self.load_record(record).await
    }

    pub async fn fork(
        &self,
        source_id: &SessionId,
        title: Option<String>,
    ) -> Result<Arc<SessionAgent>, AgentServiceError> {
        let snapshot = self.load(source_id).await?.snapshot().await?;
        if snapshot.phase != RuntimePhase::Idle {
            return Err(AgentServiceError::SessionBusy(source_id.clone()));
        }

        let source = snapshot.record;
        let title = title
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| source.info.title.clone());
        let mut record = SessionRecord::new(
            SessionId::new(),
            title,
            source.info.cwd.clone(),
            source.info.mode,
            source.llm.clone(),
        );
        record.set_parent_session_id(source.info.parent_session_id.clone());
        record.context = source.context;
        record.current_plan = source.current_plan;

        let id = record.info.id.clone();
        let operation = self.session_operation(&id).await;
        let _operation = operation.lock().await;
        let persisted = async {
            self.repository.save(&record).await?;
            for event in &snapshot.transcript {
                self.repository.append_transcript_event(&id, event).await?;
            }
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = persisted {
            let _ = self.repository.delete(&id).await;
            return Err(error.into());
        }

        match self.load_record(record).await {
            Ok(agent) => Ok(agent),
            Err(error) => {
                let _ = self.repository.delete(&id).await;
                Err(error)
            }
        }
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
        let operation = self.session_operation(id).await;
        let _operation = operation.lock().await;
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

    pub async fn clear_automation_job(&self, job: Option<&str>) -> Result<(), AgentServiceError> {
        let mut records = self.repository.list().await?;
        for record in &mut records {
            if job.is_none() || record.info.automation_job.as_deref() == job {
                record.set_automation_job(None);
                record.touch();
                self.repository.save(record).await?;
            }
        }
        Ok(())
    }

    pub async fn list_statuses(&self) -> Result<Vec<SessionStatusSnapshot>, AgentServiceError> {
        let records = self.list().await?;
        let loaded = self.loaded.lock().await.agents.clone();
        let mut statuses = Vec::with_capacity(records.len());
        for record in records {
            if let Some(agent) = loaded.get(&record.info.id) {
                let snapshot = agent.snapshot().await?;
                statuses.push(SessionStatusSnapshot {
                    last_answer: last_answer_preview(&snapshot.transcript),
                    record: snapshot.record,
                    usage: snapshot.usage,
                    phase: snapshot.phase,
                    active_turn_id: snapshot.active_turn_id,
                });
            } else {
                let transcript = self.repository.load_transcript(&record.info.id).await?;
                let used = record.context.usage.current_tokens;
                let size = self
                    .model
                    .model_limits(&record.llm.model)
                    .map(|limits| limits.context_window_tokens)
                    .unwrap_or(used);
                statuses.push(SessionStatusSnapshot {
                    last_answer: last_answer_preview(&transcript),
                    record,
                    usage: SessionUsageSnapshot { used, size },
                    phase: RuntimePhase::Idle,
                    active_turn_id: None,
                });
            }
        }
        Ok(statuses)
    }

    pub async fn status(&self, id: &SessionId) -> Result<SessionStatusSnapshot, AgentServiceError> {
        self.list_statuses()
            .await?
            .into_iter()
            .find(|status| &status.record.info.id == id)
            .ok_or_else(|| AgentServiceError::SessionNotFound(id.clone()))
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
        let operation = self.session_operation(id).await;
        let _operation = operation.lock().await;
        self.delete_locked(id).await
    }

    pub async fn delete_if_ephemeral_expired(
        &self,
        id: &SessionId,
        now_ms: u64,
    ) -> Result<Option<SessionRecord>, AgentServiceError> {
        let operation = self.session_operation(id).await;
        let _operation = operation.lock().await;
        let Some(record) = self.repository.load(id).await? else {
            return Ok(None);
        };
        if !record.info.ephemeral
            || record
                .info
                .delete_after_ms
                .is_none_or(|deadline| deadline > now_ms)
        {
            return Ok(None);
        }
        self.delete_locked(id).await?;
        Ok(Some(record))
    }

    async fn delete_locked(&self, id: &SessionId) -> Result<(), AgentServiceError> {
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

    pub async fn keep(&self, id: &SessionId) -> Result<bool, AgentServiceError> {
        let agent = self.load(id).await?;
        let operation = self.session_operation(id).await;
        let _operation = operation.lock().await;
        agent.keep().await
    }

    pub async fn recover_ephemeral_sessions(
        &self,
        now_ms: u64,
        grace_ms: u64,
    ) -> Result<Vec<(SessionId, u64)>, AgentServiceError> {
        let mut schedule = Vec::new();
        for mut record in self.repository.list().await? {
            if !record.info.ephemeral {
                continue;
            }
            let deadline = match record.info.delete_after_ms {
                Some(deadline) => deadline,
                None => {
                    let deadline = now_ms.saturating_add(grace_ms);
                    record.info.delete_after_ms = Some(deadline);
                    record.touch();
                    self.repository.save(&record).await?;
                    deadline
                }
            };
            schedule.push((record.info.id, deadline));
        }
        Ok(schedule)
    }

    pub async fn set_config(
        &self,
        id: &SessionId,
        update: SessionConfigUpdate,
    ) -> Result<(), AgentServiceError> {
        self.load(id).await?.set_config(update).await
    }

    pub async fn prompt(
        &self,
        id: &SessionId,
        origin: EndpointId,
        content: MessageContent,
    ) -> Result<PromptAccepted, AgentServiceError> {
        let agent = self.load(id).await?;
        let operation = self.session_operation(id).await;
        let _operation = operation.lock().await;
        agent.prompt_content(origin, content).await
    }

    pub async fn shutdown(&self) {
        use futures::future::join_all;

        let agents: Vec<_> = self
            .loaded
            .lock()
            .await
            .agents
            .drain()
            .map(|(_, agent)| agent)
            .collect();
        join_all(
            agents
                .into_iter()
                .map(|agent| async move { agent.close().await }),
        )
        .await;
    }

    async fn load_record(
        &self,
        mut record: SessionRecord,
    ) -> Result<Arc<SessionAgent>, AgentServiceError> {
        {
            let loaded = self.loaded.lock().await;
            if loaded.deleting.contains(&record.info.id) {
                return Err(AgentServiceError::SessionDeleting(record.info.id.clone()));
            }
            if let Some(agent) = loaded.agents.get(&record.info.id).cloned() {
                return Ok(agent);
            }
        }
        let transcript = self.repository.load_transcript(&record.info.id).await?;
        let prompt_builder = self.prompt_builder(record.info.cwd.clone());
        let mut record_changed = repair_empty_title(&mut record, &transcript);
        if !record.context.system_prompt.is_initialized() {
            record.context = ContextManager::initialize(&prompt_builder)
                .map_err(anyhow::Error::from)?
                .into_context();
            record_changed = true;
        }
        let tools = Arc::new(ToolManager::new_with_environment(
            record.info.cwd.clone(),
            self.policy.clone(),
            self.file_edit.clone(),
            [("DWO_SESSION_ID".to_string(), record.info.id.to_string())],
        )?);
        let previous_tokens = record.context.usage.current_tokens;
        let mut context = ContextManager::new(record.context.clone());
        record_changed |= context.replace_plan_watcher(
            record
                .current_plan
                .as_ref()
                .map(crate::ExecutionPlan::watcher_message),
        );
        let current_tokens = context.refresh_usage(tools.schemas());
        record.context = context.into_context();
        if current_tokens != previous_tokens {
            record_changed = true;
        }
        if record_changed {
            self.repository.save(&record).await?;
        }
        let mut loaded = self.loaded.lock().await;
        if loaded.deleting.contains(&record.info.id) {
            return Err(AgentServiceError::SessionDeleting(record.info.id.clone()));
        }
        if let Some(agent) = loaded.agents.get(&record.info.id).cloned() {
            return Ok(agent);
        }
        let agent = SessionAgent::spawn(
            record.clone(),
            transcript,
            self.repository.clone(),
            self.model.clone(),
            tools,
            prompt_builder,
            self.max_model_steps.clone(),
        );
        loaded.agents.insert(record.info.id, agent.clone());
        Ok(agent)
    }

    fn prompt_builder(&self, cwd: PathBuf) -> SystemPromptBuilder {
        SystemPromptBuilder::new(self.profile_root.clone(), cwd)
            .with_external_skill_dirs(self.external_skill_dirs.clone())
            .with_tool_prompt(dwo_tools::prompt::tools())
            .with_subsession_prompt(dwo_tools::prompt::SUBSESSIONS)
            .with_automation_prompt(dwo_tools::prompt::AUTOMATION)
            .with_channel_prompt(dwo_tools::prompt::CHANNELS)
    }

    async fn session_operation(&self, id: &SessionId) -> Arc<Mutex<()>> {
        self.operations
            .lock()
            .await
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
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

fn last_answer_preview(transcript: &[ClientTranscriptEvent]) -> Option<String> {
    transcript.iter().rev().find_map(|event| {
        let SessionEventPayload::AssistantCompleted { content, .. } = &event.payload else {
            return None;
        };
        let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.is_empty() {
            return None;
        }
        let truncated = normalized.chars().count() > 100;
        let mut preview = normalized
            .chars()
            .take(if truncated { 97 } else { 100 })
            .collect::<String>();
        if truncated {
            preview.push_str("...");
        }
        Some(preview)
    })
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use crate::TurnId;

    #[test]
    fn last_answer_preview_is_bounded_to_one_hundred_characters() {
        let transcript = vec![ClientTranscriptEvent::new(
            SessionEventPayload::AssistantCompleted {
                message_id: crate::MessageId::new(),
                thought_message_id: crate::MessageId::new(),
                turn_id: TurnId::new(),
                content: format!("answer\n{}", "x".repeat(120)),
                reasoning: None,
                tool_calls: Vec::new(),
            },
        )];
        let preview = last_answer_preview(&transcript).unwrap();
        assert_eq!(preview.chars().count(), 100);
        assert!(preview.starts_with("answer "));
        assert!(preview.ends_with("..."));
    }
}
