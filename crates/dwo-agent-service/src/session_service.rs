//! Session service lifecycle, persistence, and loaded-session orchestration.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use dwo_context::{
    CompactionView, ContextManager, ContextMessage, ExternalRuleFile, SystemPromptBuilder,
};
use dwo_model_client::{
    ConfiguredModelClient, ModelClient, ModelClientError, ModelLimits, ModelReply, ModelSelection,
    ModelStreamEvent, SummaryReply,
};
use dwo_tools::{
    ConfirmationDecision, FileEditManager, PolicyConfig, SessionMode, ToolManager, ToolPolicyEngine,
};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::error::SessionServiceError;
use crate::events::{
    ClientTranscriptEvent, RuntimePhase, SessionEventPayload, SessionNotification,
    SessionStatusSnapshot, TerminalTurnStatus,
};
use crate::profile::LoadedAgentProfile;
use crate::repository::SessionRepository;
use crate::session::{EndpointId, PromptAccepted, SessionHandle};
use crate::session_record::{
    SessionConfigUpdate, SessionId, SessionLlmSettings, SessionRecord, SessionUpdate,
    title_from_user_content,
};
use dwo_context::MessageContent;

pub struct NewSession {
    pub from: Option<SessionId>,
    pub id: Option<SessionId>,
    pub parent_session_id: Option<SessionId>,
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
    pub worktree_id: Option<String>,
    pub external_rule_files: Vec<ExternalRuleFile>,
    pub mode: Option<SessionMode>,
    pub llm: Option<SessionLlmSettings>,
    pub ephemeral: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionListQuery {
    pub cursor: Option<usize>,
    pub limit: Option<usize>,
    pub parent_session_id: Option<SessionId>,
    pub roots_only: bool,
    pub cwd: Option<PathBuf>,
}

impl SessionListQuery {
    pub fn new(cursor: Option<usize>, limit: Option<usize>) -> Self {
        Self {
            cursor,
            limit,
            parent_session_id: None,
            roots_only: false,
            cwd: None,
        }
    }

    fn page_size(&self) -> usize {
        self.limit.unwrap_or(100).clamp(1, 500)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListItem {
    pub session_id: SessionId,
    pub cwd: PathBuf,
    pub title: String,
    pub updated_at: u64,
    pub status: RuntimePhase,
    pub model: String,
    pub reasoning: Option<String>,
    pub policy: dwo_tools::SessionMode,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListPage {
    pub sessions: Vec<SessionListItem>,
    pub next_cursor: Option<usize>,
}

impl Default for SessionListPage {
    fn default() -> Self {
        Self {
            sessions: Vec::new(),
            next_cursor: None,
        }
    }
}

pub struct SessionService {
    repository: Arc<dyn SessionRepository>,
    model: Arc<ModelRuntime>,
    policy: Arc<ToolPolicyEngine>,
    file_edit: Arc<FileEditManager>,
    profile_root: PathBuf,
    external_skill_dirs: Arc<RwLock<Vec<PathBuf>>>,
    external_rule_files: Arc<RwLock<Vec<ExternalRuleFile>>>,
    session_rule_files: RwLock<HashMap<SessionId, Arc<RwLock<Vec<ExternalRuleFile>>>>>,
    max_model_steps: Arc<AtomicUsize>,
    loaded: Mutex<LoadedSessionRegistry>,
    session_locks: Mutex<HashMap<SessionId, Arc<Mutex<()>>>>,
}

struct ModelRuntime {
    current: RwLock<Arc<dyn ModelClient>>,
}

impl ModelRuntime {
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
impl ModelClient for ModelRuntime {
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
    handles: HashMap<SessionId, Arc<SessionHandle>>,
    deleting: HashSet<SessionId>,
}

impl SessionService {
    pub fn from_profile(
        repository: Arc<dyn SessionRepository>,
        profile: LoadedAgentProfile,
        policy: PolicyConfig,
    ) -> Result<Self, SessionServiceError> {
        let model = ConfiguredModelClient::from_resolved(profile.models.clone())
            .map_err(|error| SessionServiceError::InvalidConfig(error.to_string()))?;
        Ok(Self {
            repository,
            model: Arc::new(ModelRuntime::new(model)),
            policy: Arc::new(ToolPolicyEngine::new(policy)),
            file_edit: Arc::new(FileEditManager::new()),
            profile_root: profile.root,
            external_skill_dirs: Arc::new(RwLock::new(profile.external_skill_dirs)),
            external_rule_files: Arc::new(RwLock::new(profile.external_rule_files)),
            session_rule_files: RwLock::new(HashMap::new()),
            max_model_steps: Arc::new(AtomicUsize::new(profile.config.max_model_steps)),
            loaded: Mutex::new(LoadedSessionRegistry::default()),
            session_locks: Mutex::new(HashMap::new()),
        })
    }

    pub fn apply_profile(&self, profile: LoadedAgentProfile) -> Result<(), SessionServiceError> {
        let model = ConfiguredModelClient::from_resolved(profile.models)
            .map_err(|error| SessionServiceError::InvalidConfig(error.to_string()))?;
        self.model.replace(model);
        *self
            .external_skill_dirs
            .write()
            .expect("external skill dirs lock poisoned") = profile.external_skill_dirs;
        *self
            .external_rule_files
            .write()
            .expect("external rule files lock poisoned") = profile.external_rule_files;
        self.max_model_steps
            .store(profile.config.max_model_steps, Ordering::Release);
        Ok(())
    }

    pub async fn create(
        &self,
        new_session: NewSession,
    ) -> Result<Arc<SessionHandle>, SessionServiceError> {
        let id = new_session.id.clone().unwrap_or_default();
        self.set_external_rule_files(&id, new_session.external_rule_files.clone());
        let (record, transcript, rollback_on_load_error) = if let Some(source_id) =
            &new_session.from
        {
            let snapshot = self.load(source_id).await?.snapshot().await?;
            if snapshot.phase != RuntimePhase::Idle {
                return Err(SessionServiceError::SessionBusy(source_id.clone()));
            }
            let source = snapshot.record;
            let title = new_session
                .title
                .map(|title| title.trim().to_string())
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| source.info.title.clone());
            let mut record = SessionRecord::new(
                id.clone(),
                title,
                source.info.cwd.clone(),
                new_session.mode.unwrap_or(source.info.mode),
                new_session.llm.unwrap_or_else(|| source.llm.clone()),
            );
            record.set_parent_session_id(
                new_session
                    .parent_session_id
                    .or_else(|| source.info.parent_session_id.clone()),
            );
            record.info.ephemeral = new_session.ephemeral;
            record.info.worktree_id = new_session.worktree_id.or(source.info.worktree_id.clone());
            record.context = source.context;
            record.current_plan = source.current_plan;
            (record, snapshot.transcript, true)
        } else {
            let cwd = new_session.cwd.ok_or_else(|| {
                SessionServiceError::InvalidConfig("new session requires cwd".to_string())
            })?;
            let mode = new_session.mode.ok_or_else(|| {
                SessionServiceError::InvalidConfig("new session requires mode".to_string())
            })?;
            let llm = new_session.llm.ok_or_else(|| {
                SessionServiceError::InvalidConfig("new session requires llm".to_string())
            })?;
            self.model
                .validate_selection(&ModelSelection {
                    model: llm.model.clone(),
                    reasoning: llm.reasoning.clone(),
                })
                .map_err(|error| SessionServiceError::InvalidConfig(error.to_string()))?;
            let cwd = std::fs::canonicalize(cwd).map_err(anyhow::Error::from)?;
            let explicit_title = new_session
                .title
                .map(|title| title.trim().to_string())
                .filter(|title| !title.is_empty());
            let automatic_title = explicit_title.is_none();
            let title = explicit_title.unwrap_or_else(|| default_session_title(&cwd));
            let prompt_builder = self.prompt_builder(&id, cwd.clone());
            let context = ContextManager::initialize(&prompt_builder)
                .map_err(anyhow::Error::from)?
                .into_context();
            let mut record = SessionRecord::new(id.clone(), title, cwd, mode, llm);
            record.set_parent_session_id(new_session.parent_session_id);
            record.info.worktree_id = new_session.worktree_id;
            record.info.ephemeral = new_session.ephemeral;
            if automatic_title {
                record.enable_auto_title();
            }
            record.context = context;
            (record, Vec::new(), false)
        };

        let id = record.info.id.clone();
        let session_lock = {
            self.session_locks
                .lock()
                .await
                .entry(id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        {
            let _session = session_lock.lock().await;
            let persisted = async {
                self.repository.save(&record).await?;
                for event in &transcript {
                    self.repository.append_transcript_event(&id, event).await?;
                }
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = persisted {
                if rollback_on_load_error {
                    let _ = self.repository.delete(&id).await;
                }
                return Err(error.into());
            }
        }

        match self.load(&id).await {
            Ok(handle) => Ok(handle),
            Err(error) if rollback_on_load_error => {
                let _ = self.repository.delete(&id).await;
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    pub async fn load(&self, id: &SessionId) -> Result<Arc<SessionHandle>, SessionServiceError> {
        {
            let loaded = self.loaded.lock().await;
            if loaded.deleting.contains(id) {
                return Err(SessionServiceError::SessionDeleting(id.clone()));
            }
            if let Some(handle) = loaded.handles.get(id).cloned()
                && !handle.is_terminated()
            {
                return Ok(handle);
            }
        }
        let session_lock = {
            self.session_locks
                .lock()
                .await
                .entry(id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _session = session_lock.lock().await;
        {
            let loaded = self.loaded.lock().await;
            if loaded.deleting.contains(id) {
                return Err(SessionServiceError::SessionDeleting(id.clone()));
            }
            if let Some(handle) = loaded.handles.get(id).cloned()
                && !handle.is_terminated()
            {
                return Ok(handle);
            }
        }
        let mut record = self
            .repository
            .load(id)
            .await?
            .ok_or_else(|| SessionServiceError::SessionNotFound(id.clone()))?;
        let transcript = self.repository.load_transcript(id).await?;
        let prompt_builder = self.prompt_builder(id, record.info.cwd.clone());
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
        let current_tokens = context.refresh_usage(tools.schemas());
        record.context = context.into_context();
        if current_tokens != previous_tokens {
            record_changed = true;
        }
        if record_changed {
            self.repository.save(&record).await?;
        }
        let mut loaded = self.loaded.lock().await;
        if loaded.deleting.contains(id) {
            return Err(SessionServiceError::SessionDeleting(id.clone()));
        }
        if let Some(handle) = loaded.handles.get(id).cloned() {
            if !handle.is_terminated() {
                return Ok(handle);
            }
            loaded.handles.remove(id);
        }
        let handle = crate::session::SessionActor::spawn(
            record,
            transcript,
            self.repository.clone(),
            self.model.clone(),
            tools,
            prompt_builder,
            self.max_model_steps.clone(),
        );
        loaded.handles.insert(id.clone(), handle.clone());
        Ok(handle)
    }

    pub async fn list(
        &self,
        query: SessionListQuery,
    ) -> Result<SessionListPage, SessionServiceError> {
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
        if let Some(parent) = &query.parent_session_id {
            records.retain(|record| record.info.parent_session_id.as_ref() == Some(parent));
        } else if query.roots_only {
            records.retain(|record| record.info.parent_session_id.is_none());
        }
        if let Some(cwd) = &query.cwd {
            records.retain(|record| &record.info.cwd == cwd);
        }
        let total = records.len();
        let offset = query.cursor.unwrap_or(0).min(records.len());
        let limit = query.page_size();
        let end = offset.saturating_add(limit).min(records.len());
        let mut sessions = Vec::with_capacity(end.saturating_sub(offset));
        let loaded = self.loaded.lock().await.handles.clone();
        for record in records.into_iter().skip(offset).take(limit) {
            let status = match loaded.get(&record.info.id) {
                Some(handle) if !handle.is_terminated() => handle.snapshot().await?.phase,
                _ => RuntimePhase::Idle,
            };
            sessions.push(SessionListItem {
                session_id: record.info.id,
                cwd: record.info.cwd,
                title: record.info.title,
                updated_at: record.info.updated_at_ms,
                status,
                model: record.llm.model,
                reasoning: record.llm.reasoning,
                policy: record.info.mode,
            });
        }
        Ok(SessionListPage {
            sessions,
            next_cursor: (end < total).then_some(end),
        })
    }

    pub async fn status(
        &self,
        id: &SessionId,
    ) -> Result<SessionStatusSnapshot, SessionServiceError> {
        let snapshot = self.load(id).await?.snapshot().await?;
        let (last_turn_status, last_turn_finished_at_ms) = last_terminal_turn(&snapshot.transcript);
        Ok(SessionStatusSnapshot {
            last_answer: last_answer_preview(&snapshot.transcript),
            last_turn_status,
            last_turn_finished_at_ms,
            record: snapshot.record,
            usage: snapshot.usage,
            phase: snapshot.phase,
            active_turn_id: snapshot.active_turn_id,
        })
    }

    pub async fn unload(&self, id: &SessionId) -> Result<(), SessionServiceError> {
        let handle = {
            let loaded = self.loaded.lock().await;
            if loaded.deleting.contains(id) {
                return Err(SessionServiceError::SessionDeleting(id.clone()));
            }
            loaded.handles.get(id).cloned()
        };
        let Some(handle) = handle else {
            return if self.repository.load(id).await?.is_some() {
                Ok(())
            } else {
                Err(SessionServiceError::SessionNotFound(id.clone()))
            };
        };
        if !handle.is_terminated() {
            handle.unload().await?;
        }
        let mut loaded = self.loaded.lock().await;
        if loaded
            .handles
            .get(id)
            .is_some_and(|current| Arc::ptr_eq(current, &handle))
        {
            loaded.handles.remove(id);
        }
        Ok(())
    }

    pub async fn delete(&self, id: &SessionId) -> Result<(), SessionServiceError> {
        let session_lock = {
            self.session_locks
                .lock()
                .await
                .entry(id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _session = session_lock.lock().await;
        let handle = {
            let mut loaded = self.loaded.lock().await;
            if !loaded.deleting.insert(id.clone()) {
                return Err(SessionServiceError::SessionDeleting(id.clone()));
            }
            loaded.handles.remove(id)
        };
        let was_loaded = handle.is_some();

        if let Some(handle) = handle
            && !handle.is_terminated()
            && let Err(error) = handle.unload().await
        {
            self.loaded.lock().await.deleting.remove(id);
            return Err(error);
        }

        let deleted = self.repository.delete(id).await;
        self.loaded.lock().await.deleting.remove(id);
        let deleted = deleted?;
        if !was_loaded && !deleted {
            return Err(SessionServiceError::SessionNotFound(id.clone()));
        }
        self.session_rule_files
            .write()
            .expect("session rule files registry lock poisoned")
            .remove(id);
        Ok(())
    }

    pub async fn keep(&self, id: &SessionId) -> Result<bool, SessionServiceError> {
        self.load(id).await?.keep().await
    }

    pub async fn set_config(
        &self,
        id: &SessionId,
        update: SessionConfigUpdate,
    ) -> Result<(), SessionServiceError> {
        self.load(id).await?.set_config(update).await
    }

    pub async fn set(
        &self,
        id: &SessionId,
        update: SessionUpdate,
    ) -> Result<(), SessionServiceError> {
        self.load(id).await?.set(update).await
    }

    pub async fn set_workspace(
        &self,
        id: &SessionId,
        worktree_id: Option<String>,
        cwd: PathBuf,
        external_rule_files: Vec<ExternalRuleFile>,
    ) -> Result<(), SessionServiceError> {
        let previous_rule_files = self
            .session_rule_files
            .read()
            .expect("session rule files registry lock poisoned")
            .get(id)
            .and_then(|files| files.read().ok().map(|files| files.clone()))
            .unwrap_or_default();
        self.set_external_rule_files(id, external_rule_files);
        let cwd = std::fs::canonicalize(cwd).map_err(anyhow::Error::from)?;
        let prompt_builder = self.prompt_builder(id, cwd.clone());
        let tools = Arc::new(ToolManager::new_with_environment(
            cwd.clone(),
            self.policy.clone(),
            self.file_edit.clone(),
            [("DWO_SESSION_ID".to_string(), id.to_string())],
        )?);
        let result = self
            .load(id)
            .await?
            .set_workspace(worktree_id, cwd, tools, prompt_builder)
            .await;
        if result.is_err() {
            self.set_external_rule_files(id, previous_rule_files);
        }
        result
    }

    pub fn set_external_rule_files(&self, id: &SessionId, files: Vec<ExternalRuleFile>) {
        let shared = self
            .session_rule_files
            .write()
            .expect("session rule files registry lock poisoned")
            .entry(id.clone())
            .or_insert_with(|| Arc::new(RwLock::new(Vec::new())))
            .clone();
        *shared
            .write()
            .expect("session external rule files lock poisoned") = files;
    }

    pub async fn subscribe(
        &self,
        id: &SessionId,
        cursor: Option<usize>,
    ) -> Result<crate::SessionSubscription, SessionServiceError> {
        self.load(id).await?.subscribe(cursor).await
    }

    pub async fn snapshot(
        &self,
        id: &SessionId,
    ) -> Result<crate::SessionSnapshot, SessionServiceError> {
        self.load(id).await?.snapshot().await
    }

    pub async fn prompt(
        &self,
        id: &SessionId,
        origin: EndpointId,
        content: MessageContent,
    ) -> Result<PromptAccepted, SessionServiceError> {
        self.load(id).await?.prompt(origin, content).await
    }

    pub async fn prompt_internal(
        &self,
        id: &SessionId,
        content: MessageContent,
    ) -> Result<crate::TurnId, SessionServiceError> {
        self.load(id).await?.prompt_internal(content).await
    }

    pub async fn compact(
        &self,
        id: &SessionId,
        origin: EndpointId,
    ) -> Result<crate::CompactionAccepted, SessionServiceError> {
        self.load(id).await?.compact(origin).await
    }

    pub async fn cancel(
        &self,
        id: &SessionId,
        expected_turn_id: Option<crate::TurnId>,
    ) -> Result<(), SessionServiceError> {
        let handle = self.load(id).await?;
        handle.cancel(expected_turn_id).await
    }

    pub async fn respond_permission(
        &self,
        id: &SessionId,
        origin: EndpointId,
        request_id: String,
        decision: ConfirmationDecision,
    ) -> Result<(), SessionServiceError> {
        self.load(id)
            .await?
            .respond_permission(origin, request_id, decision)
            .await
    }

    pub async fn publish_notification(
        &self,
        id: &SessionId,
        notification: SessionNotification,
    ) -> Result<crate::MessageId, SessionServiceError> {
        self.load(id)
            .await?
            .publish_notification(notification)
            .await
    }

    pub async fn shutdown(&self) {
        use futures::future::join_all;

        let handles: Vec<_> = self
            .loaded
            .lock()
            .await
            .handles
            .drain()
            .map(|(_, handle)| handle)
            .collect();
        join_all(
            handles
                .into_iter()
                .map(|handle| async move { handle.unload().await }),
        )
        .await;
    }

    fn prompt_builder(&self, id: &SessionId, cwd: PathBuf) -> SystemPromptBuilder {
        let session_rule_files = self
            .session_rule_files
            .write()
            .expect("session rule files registry lock poisoned")
            .entry(id.clone())
            .or_insert_with(|| Arc::new(RwLock::new(Vec::new())))
            .clone();
        SystemPromptBuilder::new(Some(self.profile_root.clone()), cwd)
            .with_external_skill_dirs(self.external_skill_dirs.clone())
            .with_external_rule_files(self.external_rule_files.clone(), session_rule_files)
            .with_tool_prompt(dwo_tools::prompt::tools())
            .with_subsession_prompt(dwo_tools::prompt::SUBSESSIONS)
            .with_automation_prompt(dwo_tools::prompt::AUTOMATION)
            .with_channel_prompt(dwo_tools::prompt::CHANNELS)
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

fn last_terminal_turn(
    transcript: &[ClientTranscriptEvent],
) -> (Option<TerminalTurnStatus>, Option<u64>) {
    transcript
        .iter()
        .rev()
        .find_map(|event| {
            let status = match &event.payload {
                SessionEventPayload::TurnCompleted { .. } => TerminalTurnStatus::Completed,
                SessionEventPayload::TurnFailed { .. } => TerminalTurnStatus::Failed,
                SessionEventPayload::TurnCancelled { .. } => TerminalTurnStatus::Cancelled,
                _ => return None,
            };
            Some((Some(status), Some(event.recorded_at_ms)))
        })
        .unwrap_or((None, None))
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

    #[test]
    fn last_terminal_turn_reports_status_and_recorded_time() {
        let turn_id = TurnId::new();
        let transcript = vec![
            ClientTranscriptEvent {
                recorded_at_ms: 10,
                payload: SessionEventPayload::TurnFailed {
                    turn_id: turn_id.clone(),
                    error: "failed".to_string(),
                },
            },
            ClientTranscriptEvent {
                recorded_at_ms: 20,
                payload: SessionEventPayload::TurnCompleted { turn_id },
            },
        ];
        assert_eq!(
            last_terminal_turn(&transcript),
            (Some(TerminalTurnStatus::Completed), Some(20))
        );
    }
}
