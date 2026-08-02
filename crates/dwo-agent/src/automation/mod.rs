use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use dwo_agent_service::{
    AgentService, AgentServiceError, EndpointId, NewSession, SessionAgent, SessionEventPayload,
    SessionId, SessionLlmSettings, SessionSubscription, TurnId,
};
use dwo_tools::{ConfirmationDecision, SessionMode};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub jobs: Vec<AutomationJob>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationJob {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub schedule: AutomationSchedule,
    pub session: AutomationSession,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationSchedule {
    pub cron: String,
    #[serde(default = "default_timezone")]
    pub timezone: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum AutomationSession {
    New {
        behavior: AutomationNewBehavior,
        #[serde(default = "default_cwd")]
        cwd: PathBuf,
        #[serde(default)]
        title: Option<String>,
    },
    Fixed {
        session_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationNewBehavior {
    EveryTime,
    Once,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationRunStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationRunRecord {
    pub run_id: String,
    pub job: String,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub status: AutomationRunStatus,
    pub scheduled: bool,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub response: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationJobStatus {
    pub job: AutomationJob,
    pub next_run_at: Option<String>,
    pub active_runs: Vec<AutomationRunRecord>,
}

#[derive(Default)]
struct RuntimeState {
    source: String,
    config: AutomationConfig,
    next_runs: BTreeMap<String, DateTime<Utc>>,
    active: BTreeMap<String, AutomationRunRecord>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AutomationBindings {
    #[serde(default)]
    sessions: BTreeMap<String, String>,
}

struct AutomationExecution {
    record: AutomationRunRecord,
    agent: Arc<SessionAgent>,
    endpoint: EndpointId,
    subscription: SessionSubscription,
    turn_id: TurnId,
}

#[derive(Deserialize)]
struct ProfileAutomation {
    #[serde(default)]
    automation: AutomationConfig,
}

pub struct AutomationRuntime {
    service: Arc<AgentService>,
    profile_root: PathBuf,
    profile_path: PathBuf,
    bindings_path: PathBuf,
    default_model: String,
    default_mode: SessionMode,
    default_max_model_steps: usize,
    shutdown: CancellationToken,
    state: Mutex<RuntimeState>,
    bindings: Mutex<AutomationBindings>,
}

impl AutomationRuntime {
    pub fn new(
        service: Arc<AgentService>,
        profile_root: PathBuf,
        config: AutomationConfig,
        default_model: String,
        default_mode: SessionMode,
        default_max_model_steps: usize,
        shutdown: CancellationToken,
    ) -> Result<Arc<Self>> {
        validate_config(&config)?;
        let source = std::fs::read_to_string(profile_root.join("profile.yaml"))?;
        let next_runs = build_next_runs(&config)?;
        let bindings_path = profile_root.join("runtime/automation.yaml");
        let bindings = read_bindings(&bindings_path)?;
        Ok(Arc::new(Self {
            service,
            profile_path: profile_root.join("profile.yaml"),
            bindings_path,
            profile_root,
            default_model,
            default_mode,
            default_max_model_steps,
            shutdown,
            state: Mutex::new(RuntimeState {
                source,
                config,
                next_runs,
                active: BTreeMap::new(),
            }),
            bindings: Mutex::new(bindings),
        }))
    }

    pub fn start(self: &Arc<Self>) {
        let runtime = self.clone();
        tokio::spawn(async move { runtime.scheduler_loop().await });
    }

    pub async fn list(&self) -> Vec<AutomationJobStatus> {
        let state = self.state.lock().await;
        state
            .config
            .jobs
            .iter()
            .cloned()
            .map(|job| AutomationJobStatus {
                next_run_at: state.next_runs.get(&job.name).map(DateTime::to_rfc3339),
                active_runs: state
                    .active
                    .values()
                    .filter(|record| record.job == job.name)
                    .cloned()
                    .collect(),
                job,
            })
            .collect()
    }

    pub async fn run_now(
        self: &Arc<Self>,
        name: &str,
        caller: Option<SessionId>,
    ) -> Result<AutomationRunRecord> {
        let job = {
            let state = self.state.lock().await;
            state
                .config
                .jobs
                .iter()
                .find(|job| job.name == name)
                .cloned()
                .with_context(|| format!("automation job not found: {name}"))?
        };
        Ok(self.queue_job(job, false, caller).await)
    }

    async fn scheduler_loop(self: Arc<Self>) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(error) = self.reload_if_changed().await {
                        tracing::warn!(
                            event = "automation.reload_failed",
                            error = %format!("{error:#}"),
                            "reload automation configuration failed"
                        );
                    }
                    for job in self.take_due_jobs().await {
                        self.queue_job(job, true, None).await;
                    }
                }
            }
        }
    }

    async fn reload_if_changed(&self) -> Result<()> {
        let source = std::fs::read_to_string(&self.profile_path)?;
        if self.state.lock().await.source == source {
            return Ok(());
        }
        let profile: ProfileAutomation = serde_yaml::from_str(&source)?;
        validate_config(&profile.automation)?;
        let next_runs = build_next_runs(&profile.automation)?;
        let mut state = self.state.lock().await;
        state.source = source;
        state.config = profile.automation;
        state.next_runs = next_runs;
        Ok(())
    }

    async fn take_due_jobs(&self) -> Vec<AutomationJob> {
        let now = Utc::now();
        let mut state = self.state.lock().await;
        if !state.config.enabled {
            return Vec::new();
        }
        let jobs = state
            .config
            .jobs
            .iter()
            .filter(|job| {
                job.enabled
                    && state
                        .next_runs
                        .get(&job.name)
                        .is_some_and(|next| next <= &now)
            })
            .cloned()
            .collect::<Vec<_>>();
        for job in &jobs {
            match next_run(&job.schedule) {
                Ok(next) => {
                    state.next_runs.insert(job.name.clone(), next);
                }
                Err(error) => tracing::warn!(
                    event = "automation.schedule_failed",
                    job = %job.name,
                    error = %format!("{error:#}"),
                    "schedule automation job failed"
                ),
            }
        }
        jobs
    }

    async fn queue_job(
        self: &Arc<Self>,
        job: AutomationJob,
        scheduled: bool,
        caller: Option<SessionId>,
    ) -> AutomationRunRecord {
        let run_id = format!("run-{}", Uuid::new_v4().simple());
        let record = AutomationRunRecord {
            run_id,
            job: job.name.clone(),
            session_id: None,
            turn_id: None,
            status: AutomationRunStatus::Running,
            scheduled,
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
            response: String::new(),
            error: None,
        };
        self.set_active(record.clone()).await;
        let runtime = self.clone();
        let queued = record.clone();
        tokio::spawn(async move { runtime.run_job(queued, job, caller).await });
        record
    }

    async fn start_execution(
        &self,
        job: &AutomationJob,
        record: &mut AutomationRunRecord,
    ) -> Result<(Arc<SessionAgent>, EndpointId, SessionSubscription, TurnId)> {
        let agent = match &job.session {
            AutomationSession::New {
                behavior,
                cwd,
                title,
            } => match behavior {
                AutomationNewBehavior::EveryTime => self.create_session(job, cwd, title).await?,
                AutomationNewBehavior::Once => self.once_session(job, cwd, title).await?,
            },
            AutomationSession::Fixed { session_id } => {
                let id = SessionId::parse(session_id.clone()).map_err(anyhow::Error::msg)?;
                self.service.load(&id).await?
            }
        };
        record.session_id = Some(agent.id().to_string());
        self.set_active(record.clone()).await;

        let endpoint = EndpointId::new();
        let subscription = agent.attach(endpoint.clone()).await?;
        let turn_id = agent.prompt(endpoint.clone(), job.prompt.clone()).await?;
        record.turn_id = Some(turn_id.to_string());
        self.set_active(record.clone()).await;
        Ok((agent, endpoint, subscription, turn_id))
    }

    async fn create_session(
        &self,
        job: &AutomationJob,
        cwd: &Path,
        title: &Option<String>,
    ) -> Result<Arc<SessionAgent>> {
        let cwd = if cwd.is_absolute() {
            cwd.to_path_buf()
        } else {
            self.profile_root.join(cwd)
        };
        Ok(self
            .service
            .create(NewSession {
                id: None,
                parent_session_id: None,
                title: Some(
                    title
                        .clone()
                        .unwrap_or_else(|| format!("automation/{}", job.name)),
                ),
                cwd,
                mode: self.default_mode,
                max_model_steps: self.default_max_model_steps,
                llm: SessionLlmSettings {
                    model: self.default_model.clone(),
                    reasoning: None,
                },
            })
            .await?)
    }

    async fn once_session(
        &self,
        job: &AutomationJob,
        cwd: &Path,
        title: &Option<String>,
    ) -> Result<Arc<SessionAgent>> {
        let mut bindings = self.bindings.lock().await;
        if let Some(session_id) = bindings.sessions.get(&job.name) {
            let id = SessionId::parse(session_id.clone()).map_err(anyhow::Error::msg)?;
            match self.service.load(&id).await {
                Ok(agent) => return Ok(agent),
                Err(AgentServiceError::SessionNotFound(_)) => {
                    bindings.sessions.remove(&job.name);
                }
                Err(error) => return Err(error.into()),
            }
        }

        let agent = self.create_session(job, cwd, title).await?;
        bindings
            .sessions
            .insert(job.name.clone(), agent.id().to_string());
        if let Err(error) = write_bindings(&self.bindings_path, &bindings).await {
            bindings.sessions.remove(&job.name);
            let _ = self.service.delete(agent.id()).await;
            return Err(error);
        }
        Ok(agent)
    }

    async fn run_job(
        self: &Arc<Self>,
        mut record: AutomationRunRecord,
        job: AutomationJob,
        caller: Option<SessionId>,
    ) {
        let result = match self.start_execution(&job, &mut record).await {
            Ok((agent, endpoint, subscription, turn_id)) => {
                let mut execution = AutomationExecution {
                    record,
                    agent,
                    endpoint,
                    subscription,
                    turn_id,
                };
                let result = self.monitor_execution(&mut execution).await;
                record = execution.record;
                result
            }
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            record.status = AutomationRunStatus::Failed;
            record.error = Some(format!("{error:#}"));
            tracing::error!(
                event = "automation.job_failed",
                job = %record.job,
                run_id = %record.run_id,
                error = %format!("{error:#}"),
                "automation job failed"
            );
        }
        record.finished_at = Some(Utc::now().to_rfc3339());
        self.clear_active(&record.run_id).await;
        if let Some(caller) = caller {
            self.deliver_result(caller, &record).await;
        }
    }

    async fn monitor_execution(&self, execution: &mut AutomationExecution) -> Result<()> {
        let AutomationExecution {
            record,
            agent,
            endpoint,
            subscription,
            turn_id,
        } = execution;
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    let _ = agent.cancel(Some(turn_id.clone())).await;
                    record.status = AutomationRunStatus::Cancelled;
                    return Ok(());
                }
                event = subscription.events.recv() => {
                    let Some(event) = event else {
                        bail!("session event stream closed before automation turn finished");
                    };
                    match event.payload {
                        SessionEventPayload::AssistantCompleted { turn_id: event_turn, content, .. }
                            if event_turn == *turn_id => record.response = content,
                        SessionEventPayload::PermissionRequested { turn_id: event_turn, permission }
                            if event_turn == *turn_id => {
                                agent.respond_permission(
                                    endpoint.clone(),
                                    permission.request_id,
                                    ConfirmationDecision {
                                        allowed: false,
                                        reason: Some("unattended automation cannot confirm tools".to_string()),
                                    },
                                ).await?;
                            }
                        SessionEventPayload::TurnCompleted { turn_id: event_turn }
                            if event_turn == *turn_id => {
                                record.status = AutomationRunStatus::Completed;
                                return Ok(());
                            }
                        SessionEventPayload::TurnCancelled { turn_id: event_turn }
                            if event_turn == *turn_id => {
                                record.status = AutomationRunStatus::Cancelled;
                                return Ok(());
                            }
                        SessionEventPayload::TurnFailed { turn_id: event_turn, error }
                            if event_turn == *turn_id => bail!(error),
                        _ => {}
                    }
                }
            }
        }
    }

    async fn deliver_result(&self, caller: SessionId, record: &AutomationRunRecord) {
        let notification = automation_result_notification(record);
        let agent = match self.service.load(&caller).await {
            Ok(agent) => agent,
            Err(error) => {
                tracing::error!(
                    event = "automation.caller_load_failed",
                    caller_session_id = %caller,
                    run_id = %record.run_id,
                    error = %format!("{error:#}"),
                    "load automation caller failed"
                );
                return;
            }
        };
        if let Err(error) = agent.notify_internal(notification).await {
            tracing::error!(
                event = "automation.result_delivery_failed",
                caller_session_id = %caller,
                run_id = %record.run_id,
                error = %format!("{error:#}"),
                "deliver automation result failed"
            );
        }
    }

    async fn set_active(&self, record: AutomationRunRecord) {
        self.state
            .lock()
            .await
            .active
            .insert(record.run_id.clone(), record);
    }

    async fn clear_active(&self, run_id: &str) {
        self.state.lock().await.active.remove(run_id);
    }
}

fn automation_result_notification(record: &AutomationRunRecord) -> String {
    format!(
        "<automation_result>\n{}\n</automation_result>",
        serde_json::json!({
            "run_id": record.run_id,
            "job": record.job,
            "session_id": record.session_id,
            "turn_id": record.turn_id,
            "status": record.status,
            "content": record.response,
            "error": record.error,
        })
    )
}

fn read_bindings(path: &Path) -> Result<AutomationBindings> {
    if !path.is_file() {
        return Ok(AutomationBindings::default());
    }
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("read automation bindings from {}", path.display()))?;
    serde_yaml::from_str(&source)
        .with_context(|| format!("parse automation bindings from {}", path.display()))
}

async fn write_bindings(path: &Path, bindings: &AutomationBindings) -> Result<()> {
    let source = serde_yaml::to_string(bindings)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temporary = path.with_extension(format!("yaml.{}.tmp", Uuid::new_v4()));
    tokio::fs::write(&temporary, source).await?;
    if tokio::fs::try_exists(path).await? {
        tokio::fs::remove_file(path).await?;
    }
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

pub fn parse_config(value: serde_yaml::Value) -> Result<AutomationConfig> {
    if value.is_null() {
        return Ok(AutomationConfig::default());
    }
    let config: AutomationConfig = serde_yaml::from_value(value)?;
    validate_config(&config)?;
    Ok(config)
}

fn validate_config(config: &AutomationConfig) -> Result<()> {
    let mut names = std::collections::BTreeSet::new();
    for job in &config.jobs {
        validate_name(&job.name)?;
        if !names.insert(&job.name) {
            bail!("duplicate automation job name: {}", job.name);
        }
        if job.prompt.trim().is_empty() {
            bail!("automation job {} prompt must not be empty", job.name);
        }
        if let AutomationSession::Fixed { session_id } = &job.session {
            SessionId::parse(session_id.clone()).map_err(anyhow::Error::msg)?;
        }
        next_run(&job.schedule)?;
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("automation job name must contain only ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

fn build_next_runs(config: &AutomationConfig) -> Result<BTreeMap<String, DateTime<Utc>>> {
    if !config.enabled {
        return Ok(BTreeMap::new());
    }
    config
        .jobs
        .iter()
        .filter(|job| job.enabled)
        .map(|job| Ok((job.name.clone(), next_run(&job.schedule)?)))
        .collect()
}

fn next_run(config: &AutomationSchedule) -> Result<DateTime<Utc>> {
    let fields = config.cron.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 5 {
        bail!("automation cron must use five fields: minute hour day month weekday");
    }
    let expression = format!("0 {} *", config.cron);
    let schedule = Schedule::from_str(&expression).context("parse automation cron")?;
    if config.timezone == "local" {
        return schedule
            .after(&Local::now())
            .next()
            .map(|next| next.with_timezone(&Utc))
            .context("automation schedule has no future occurrence");
    }
    let timezone = Tz::from_str(&config.timezone)
        .with_context(|| format!("unknown automation timezone: {}", config.timezone))?;
    schedule
        .after(&Utc::now().with_timezone(&timezone))
        .next()
        .map(|next| next.with_timezone(&Utc))
        .context("automation schedule has no future occurrence")
}

fn default_true() -> bool {
    true
}

fn default_timezone() -> String {
    "local".to_string()
}

fn default_cwd() -> PathBuf {
    PathBuf::from(".")
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dwo_agent_service::{
        CompactionView, ContextMessage, MemorySessionRepository, ModelClient, ModelClientError,
        ModelLimits, ModelReply, ModelSelection, ModelStreamEvent, SummaryReply,
    };
    use serde_json::Value;
    use tokio::sync::mpsc;

    struct UnusedModel;

    #[async_trait]
    impl ModelClient for UnusedModel {
        fn model_limits(&self, _model: &str) -> Result<ModelLimits, ModelClientError> {
            Ok(ModelLimits {
                context_window_tokens: 16_000,
                max_output_tokens: 1_000,
                max_input_tokens: 15_000,
                compact_trigger_tokens: 12_000,
            })
        }

        async fn stream_turn(
            &self,
            _selection: ModelSelection,
            _messages: &[ContextMessage],
            _tools: &[Value],
            _events: mpsc::UnboundedSender<ModelStreamEvent>,
            _cancellation: &CancellationToken,
        ) -> Result<ModelReply, ModelClientError> {
            unreachable!("once-session resolution does not call the model")
        }

        async fn complete(
            &self,
            _selection: ModelSelection,
            _messages: Vec<ContextMessage>,
            _cancellation: CancellationToken,
        ) -> Result<ModelReply, ModelClientError> {
            unreachable!("once-session resolution does not call the model")
        }

        async fn summarize(
            &self,
            _selection: ModelSelection,
            _view: CompactionView,
            _cancellation: CancellationToken,
        ) -> Result<SummaryReply, ModelClientError> {
            unreachable!("once-session resolution does not call the model")
        }
    }

    #[test]
    fn parses_new_and_fixed_jobs() {
        let config: AutomationConfig = serde_yaml::from_str(
            r#"
enabled: true
jobs:
  - name: fresh
    schedule: { cron: "0 9 * * *", timezone: Asia/Shanghai }
    session: { mode: new, behavior: once, cwd: projects/demo }
    prompt: report status
  - name: fixed
    schedule: { cron: "*/5 * * * *" }
    session: { mode: fixed, sessionId: session-existing }
    prompt: continue work
"#,
        )
        .unwrap();
        validate_config(&config).unwrap();
        assert!(matches!(
            config.jobs[0].session,
            AutomationSession::New {
                behavior: AutomationNewBehavior::Once,
                ..
            }
        ));
        assert!(matches!(
            config.jobs[1].session,
            AutomationSession::Fixed { .. }
        ));
    }

    #[test]
    fn rejects_nonstandard_cron_field_count() {
        let error = next_run(&AutomationSchedule {
            cron: "0 0 9 * * *".to_string(),
            timezone: "local".to_string(),
        })
        .unwrap_err();
        assert!(error.to_string().contains("five fields"));
    }

    #[test]
    fn disabled_config_has_no_next_runs() {
        let config = AutomationConfig {
            enabled: false,
            jobs: Vec::new(),
        };
        assert!(build_next_runs(&config).unwrap().is_empty());
    }

    #[test]
    fn new_session_requires_an_explicit_behavior() {
        let error = serde_yaml::from_str::<AutomationSession>("mode: new\ncwd: .\n").unwrap_err();
        assert!(error.to_string().contains("behavior"));
    }

    #[tokio::test]
    async fn automation_bindings_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime/automation.yaml");
        let mut bindings = AutomationBindings::default();
        bindings
            .sessions
            .insert("daily-report".to_string(), "session-test".to_string());
        write_bindings(&path, &bindings).await.unwrap();
        let loaded = read_bindings(&path).unwrap();
        assert_eq!(loaded.sessions, bindings.sessions);
    }

    #[tokio::test]
    async fn once_behavior_reuses_the_persisted_session_after_runtime_restart() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("profile.yaml"),
            "automation: { enabled: false, jobs: [] }\n",
        )
        .unwrap();
        let service = Arc::new(AgentService::new(
            Arc::new(MemorySessionRepository::default()),
            Arc::new(UnusedModel),
            dwo_tools::PolicyConfig::default(),
        ));
        let job = AutomationJob {
            name: "sticky".to_string(),
            enabled: true,
            schedule: AutomationSchedule {
                cron: "0 9 * * *".to_string(),
                timezone: "Asia/Shanghai".to_string(),
            },
            session: AutomationSession::New {
                behavior: AutomationNewBehavior::Once,
                cwd: PathBuf::from("."),
                title: None,
            },
            prompt: "continue".to_string(),
        };
        let build_runtime = || {
            AutomationRuntime::new(
                service.clone(),
                root.path().to_path_buf(),
                AutomationConfig {
                    enabled: false,
                    jobs: vec![job.clone()],
                },
                "test-model".to_string(),
                SessionMode::Watch,
                10,
                CancellationToken::new(),
            )
            .unwrap()
        };

        let first_runtime = build_runtime();
        let first = first_runtime
            .once_session(&job, Path::new("."), &None)
            .await
            .unwrap();
        drop(first_runtime);
        let second = build_runtime()
            .once_session(&job, Path::new("."), &None)
            .await
            .unwrap();

        assert_eq!(first.id(), second.id());
        assert_eq!(service.list().await.unwrap().len(), 1);
    }

    #[test]
    fn automation_result_contains_terminal_state_and_ids() {
        let notification = automation_result_notification(&AutomationRunRecord {
            run_id: "run-test".to_string(),
            job: "daily-report".to_string(),
            session_id: Some("session-test".to_string()),
            turn_id: Some("turn-test".to_string()),
            status: AutomationRunStatus::Failed,
            scheduled: false,
            started_at: "2026-08-01T00:00:00Z".to_string(),
            finished_at: Some("2026-08-01T00:00:01Z".to_string()),
            response: "partial response".to_string(),
            error: Some("model failed".to_string()),
        });
        let json = notification
            .strip_prefix("<automation_result>\n")
            .and_then(|value| value.strip_suffix("\n</automation_result>"))
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["run_id"], "run-test");
        assert_eq!(value["session_id"], "session-test");
        assert_eq!(value["turn_id"], "turn-test");
        assert_eq!(value["status"], "failed");
        assert_eq!(value["content"], "partial response");
        assert_eq!(value["error"], "model failed");
    }
}
