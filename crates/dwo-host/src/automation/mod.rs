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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub jobs: Vec<AutomationJob>,
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_seconds: default_timeout_seconds(),
            jobs: Vec::new(),
        }
    }
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
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub policy: Option<SessionMode>,
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
    Queued,
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
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationJobStatus {
    pub job: AutomationJob,
    pub scheduler_enabled: bool,
    pub next_run_at: Option<String>,
    pub active_runs: Vec<AutomationRunRecord>,
    pub recent_runs: Vec<AutomationRunRecord>,
    pub effective_model: String,
    pub bound_session_id: Option<String>,
}

#[derive(Default)]
struct RuntimeState {
    config: AutomationConfig,
    next_runs: BTreeMap<String, DateTime<Utc>>,
    active: BTreeMap<String, AutomationRunRecord>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AutomationHistory {
    #[serde(default)]
    runs: Vec<AutomationRunRecord>,
}

struct AutomationExecution {
    record: AutomationRunRecord,
    agent: Arc<SessionAgent>,
    endpoint: EndpointId,
    subscription: SessionSubscription,
    turn_id: TurnId,
}

#[derive(Clone)]
struct AutomationDefaults {
    model: String,
    reasoning: Option<String>,
    mode: SessionMode,
}

pub struct AutomationRuntime {
    service: Arc<AgentService>,
    profile_root: PathBuf,
    history_path: PathBuf,
    defaults: Mutex<AutomationDefaults>,
    shutdown: CancellationToken,
    state: Mutex<RuntimeState>,
    history: Mutex<AutomationHistory>,
    session_queues: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
}

impl AutomationRuntime {
    pub fn new(
        service: Arc<AgentService>,
        profile_root: PathBuf,
        config: AutomationConfig,
        default_model: String,
        default_reasoning: Option<String>,
        default_mode: SessionMode,
        shutdown: CancellationToken,
    ) -> Result<Arc<Self>> {
        validate_config(&config)?;
        let next_runs = build_next_runs(&config)?;
        let history_path = profile_root.join("runtime/automation-runs.yaml");
        let mut history = read_history(&history_path)?;
        for run in &mut history.runs {
            if matches!(
                run.status,
                AutomationRunStatus::Queued | AutomationRunStatus::Running
            ) {
                run.status = AutomationRunStatus::Failed;
                run.finished_at = Some(Utc::now().to_rfc3339());
                run.error =
                    Some("daemon restarted before the automation run completed".to_string());
                run.finish_reason = Some("host_restarted".to_string());
            }
        }
        Ok(Arc::new(Self {
            service,
            history_path,
            profile_root,
            defaults: Mutex::new(AutomationDefaults {
                model: default_model,
                reasoning: default_reasoning,
                mode: default_mode,
            }),
            shutdown,
            state: Mutex::new(RuntimeState {
                config,
                next_runs,
                active: BTreeMap::new(),
            }),
            history: Mutex::new(history),
            session_queues: Mutex::new(BTreeMap::new()),
        }))
    }

    pub fn start(self: &Arc<Self>) {
        let runtime = self.clone();
        tokio::spawn(async move { runtime.scheduler_loop().await });
    }

    pub async fn list(&self) -> Vec<AutomationJobStatus> {
        let (scheduler_enabled, jobs, next_runs, active) = {
            let state = self.state.lock().await;
            (
                state.config.enabled,
                state.config.jobs.clone(),
                state.next_runs.clone(),
                state.active.values().cloned().collect::<Vec<_>>(),
            )
        };
        let history = self.history.lock().await.runs.clone();
        let default_model = self.defaults.lock().await.model.clone();
        let sessions = self
            .service
            .list()
            .await
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        jobs.iter()
            .cloned()
            .map(|job| {
                let bound_session_id = match &job.session {
                    AutomationSession::New {
                        behavior: AutomationNewBehavior::Once,
                        ..
                    } => sessions
                        .iter()
                        .find(|record| record.info.automation_job.as_deref() == Some(&job.name))
                        .map(|record| record.info.id.to_string()),
                    AutomationSession::Fixed { session_id } => Some(session_id.clone()),
                    AutomationSession::New { .. } => None,
                };
                let effective_model = bound_session_id
                    .as_ref()
                    .and_then(|session_id| {
                        sessions
                            .iter()
                            .find(|record| record.info.id.to_string() == *session_id)
                    })
                    .map(|record| record.llm.model.clone())
                    .unwrap_or_else(|| default_model.clone());
                AutomationJobStatus {
                    scheduler_enabled,
                    next_run_at: next_runs.get(&job.name).map(DateTime::to_rfc3339),
                    active_runs: active
                        .iter()
                        .filter(|record| record.job == job.name)
                        .cloned()
                        .collect(),
                    recent_runs: history
                        .iter()
                        .rev()
                        .filter(|record| record.job == job.name)
                        .take(10)
                        .cloned()
                        .collect(),
                    bound_session_id,
                    effective_model,
                    job,
                }
            })
            .collect()
    }

    pub async fn status(&self, name: &str) -> Result<AutomationJobStatus> {
        self.list()
            .await
            .into_iter()
            .find(|status| status.job.name == name)
            .with_context(|| format!("automation job not found: {name}"))
    }

    pub async fn history(&self, name: Option<&str>, limit: usize) -> Vec<AutomationRunRecord> {
        let limit = limit.clamp(1, 100);
        let history = self.history.lock().await;
        history
            .runs
            .iter()
            .rev()
            .filter(|run| name.is_none_or(|name| run.job == name))
            .take(limit)
            .cloned()
            .collect()
    }

    pub async fn remove_job_state(&self, name: Option<&str>, all: bool) -> Result<()> {
        if all {
            self.service.clear_automation_job(None).await?;
        } else if let Some(name) = name {
            self.service.clear_automation_job(Some(name)).await?;
        }
        {
            let mut history = self.history.lock().await;
            if all {
                history.runs.clear();
            } else if let Some(name) = name {
                history.runs.retain(|run| run.job != name);
            }
            write_history(&self.history_path, &history).await?;
        }
        Ok(())
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
        self.start_job(job, false, caller).await
    }

    async fn scheduler_loop(self: Arc<Self>) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = interval.tick() => {
                    for job in self.take_due_jobs().await {
                        if let Err(error) = self.start_job(job, true, None).await {
                            tracing::error!(
                                event = "automation.job_start_failed",
                                error = %format!("{error:#}"),
                                "start scheduled automation job failed"
                            );
                        }
                    }
                }
            }
        }
    }

    pub async fn apply_profile(
        &self,
        config: AutomationConfig,
        default_model: String,
        default_reasoning: Option<String>,
        default_mode: SessionMode,
    ) -> Result<()> {
        validate_config(&config)?;
        let next_runs = build_next_runs(&config)?;
        let mut state = self.state.lock().await;
        state.config = config;
        state.next_runs = next_runs;
        drop(state);
        *self.defaults.lock().await = AutomationDefaults {
            model: default_model,
            reasoning: default_reasoning,
            mode: default_mode,
        };
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

    async fn start_job(
        self: &Arc<Self>,
        job: AutomationJob,
        scheduled: bool,
        caller: Option<SessionId>,
    ) -> Result<AutomationRunRecord> {
        let run_id = format!("run-{}", Uuid::new_v4().simple());
        let record = AutomationRunRecord {
            run_id,
            job: job.name.clone(),
            session_id: None,
            turn_id: None,
            status: AutomationRunStatus::Queued,
            scheduled,
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
            response: String::new(),
            error: None,
            finish_reason: None,
        };
        self.set_active(record.clone()).await;
        let mut started = record;
        let agent = match self.resolve_session(&job).await {
            Ok(agent) => agent,
            Err(error) => {
                started.status = AutomationRunStatus::Failed;
                started.finished_at = Some(Utc::now().to_rfc3339());
                started.error = Some(format!("{error:#}"));
                self.clear_active(&started.run_id).await;
                self.append_history(started).await?;
                return Err(error);
            }
        };
        started.session_id = Some(agent.id().to_string());
        self.set_active(started.clone()).await;
        if let Some(model) = &job.model {
            agent
                .set_config(dwo_agent_service::SessionConfigUpdate::Model(model.clone()))
                .await?;
        }
        if let Some(reasoning) = &job.reasoning {
            agent
                .set_config(dwo_agent_service::SessionConfigUpdate::Reasoning(Some(
                    reasoning.clone(),
                )))
                .await?;
        }
        if let Some(policy) = job.policy {
            agent
                .set_config(dwo_agent_service::SessionConfigUpdate::Mode(policy))
                .await?;
        }
        let session_queue = if matches!(
            &job.session,
            AutomationSession::New {
                behavior: AutomationNewBehavior::EveryTime,
                ..
            }
        ) {
            Arc::new(Mutex::new(()))
        } else {
            let mut queues = self.session_queues.lock().await;
            queues
                .entry(agent.id().to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let returned = started.clone();
        let runtime = self.clone();
        tokio::spawn(async move {
            runtime
                .run_queued_job(started, job.prompt, agent, session_queue, caller)
                .await
        });
        Ok(returned)
    }

    async fn resolve_session(&self, job: &AutomationJob) -> Result<Arc<SessionAgent>> {
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
        Ok(agent)
    }

    async fn start_execution(
        &self,
        agent: Arc<SessionAgent>,
        prompt: String,
        record: &mut AutomationRunRecord,
    ) -> Result<AutomationExecution> {
        let endpoint = EndpointId::new();
        let mut subscription = agent.attach(endpoint.clone()).await?;
        let turn_id = loop {
            match agent.prompt_idle(endpoint.clone(), prompt.clone()).await {
                Ok(accepted) => break accepted.turn_id,
                Err(AgentServiceError::SessionBusy(_)) => {}
                Err(error) => return Err(error.into()),
            }
            tokio::select! {
                _ = self.shutdown.cancelled() => bail!("automation runtime is shutting down"),
                event = subscription.events.recv() => {
                    let Some(event) = event else {
                        bail!("session event stream closed while automation waited for idle");
                    };
                    if !matches!(
                        event.payload,
                        SessionEventPayload::TurnCompleted { .. }
                            | SessionEventPayload::TurnCancelled { .. }
                            | SessionEventPayload::TurnFailed { .. }
                    ) {
                        continue;
                    }
                }
            }
        };
        record.status = AutomationRunStatus::Running;
        record.turn_id = Some(turn_id.to_string());
        self.set_active(record.clone()).await;
        Ok(AutomationExecution {
            record: record.clone(),
            agent,
            endpoint,
            subscription,
            turn_id,
        })
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
        let defaults = self.defaults.lock().await.clone();
        let model = job.model.clone().unwrap_or_else(|| defaults.model.clone());
        let reasoning = job.reasoning.clone().or_else(|| {
            job.model
                .is_none()
                .then(|| defaults.reasoning.clone())
                .flatten()
        });
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
                mode: job.policy.unwrap_or(defaults.mode),
                llm: SessionLlmSettings::new(model, reasoning),
                automation_job: matches!(
                    &job.session,
                    AutomationSession::New {
                        behavior: AutomationNewBehavior::Once,
                        ..
                    }
                )
                .then(|| job.name.clone()),
                ephemeral: false,
            })
            .await?)
    }

    async fn once_session(
        &self,
        job: &AutomationJob,
        cwd: &Path,
        title: &Option<String>,
    ) -> Result<Arc<SessionAgent>> {
        if let Some(record) = self
            .service
            .list()
            .await?
            .into_iter()
            .find(|record| record.info.automation_job.as_deref() == Some(&job.name))
        {
            return Ok(self.service.load(&record.info.id).await?);
        }
        self.create_session(job, cwd, title).await
    }

    async fn run_queued_job(
        self: &Arc<Self>,
        mut record: AutomationRunRecord,
        prompt: String,
        agent: Arc<SessionAgent>,
        session_queue: Arc<Mutex<()>>,
        caller: Option<SessionId>,
    ) {
        let queue = tokio::select! {
            _ = self.shutdown.cancelled() => None,
            guard = session_queue.lock_owned() => Some(guard),
        };
        let result = if queue.is_some() {
            match self.start_execution(agent, prompt, &mut record).await {
                Ok(mut execution) => {
                    let result = self.monitor_execution(&mut execution).await;
                    record = execution.record;
                    result
                }
                Err(error) => Err(error),
            }
        } else {
            record.status = AutomationRunStatus::Cancelled;
            Ok(())
        };
        if let Err(error) = result {
            if self.shutdown.is_cancelled() {
                record.status = AutomationRunStatus::Cancelled;
            } else {
                record.status = AutomationRunStatus::Failed;
                record.error = Some(format!("{error:#}"));
            }
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
        if let Err(error) = self.append_history(record.clone()).await {
            tracing::error!(
                event = "automation.history_write_failed",
                run_id = %record.run_id,
                error = %format!("{error:#}"),
                "persist automation run history failed"
            );
        }
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
        let timeout_seconds = self.state.lock().await.config.timeout_seconds;
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(timeout_seconds));
        tokio::pin!(timeout);
        let mut timeout_sent = false;
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    let _ = agent.cancel(Some(turn_id.clone())).await;
                    record.status = AutomationRunStatus::Cancelled;
                    return Ok(());
                }
                _ = &mut timeout, if !timeout_sent => {
                    timeout_sent = true;
                    match agent.append_internal(
                        turn_id.clone(),
                        automation_timeout_notification(timeout_seconds),
                    ).await {
                        Ok(()) | Err(AgentServiceError::TurnNotActive(_)) => {}
                        Err(error) => return Err(error.into()),
                    }
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

    async fn append_history(&self, mut record: AutomationRunRecord) -> Result<()> {
        record.response = answer_preview(&record.response);
        if record.finish_reason.is_none() {
            record.finish_reason = Some(
                match record.status {
                    AutomationRunStatus::Queued => "queued",
                    AutomationRunStatus::Running => "running",
                    AutomationRunStatus::Completed => "completed",
                    AutomationRunStatus::Failed => "failed",
                    AutomationRunStatus::Cancelled => "cancelled",
                }
                .to_string(),
            );
        }
        let mut history = self.history.lock().await;
        history.runs.push(record);
        if history.runs.len() > 100 {
            let remove = history.runs.len() - 100;
            history.runs.drain(..remove);
        }
        write_history(&self.history_path, &history).await
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
            "finish_reason": record.finish_reason,
        })
    )
}

fn automation_timeout_notification(timeout_seconds: u64) -> String {
    format!(
        "<automation_timeout>\nThe automation time limit of {timeout_seconds} seconds has been reached. Stop using tools and provide the final answer now using the information already available.\n</automation_timeout>"
    )
}

fn read_history(path: &Path) -> Result<AutomationHistory> {
    if !path.is_file() {
        return Ok(AutomationHistory::default());
    }
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("read automation history from {}", path.display()))?;
    serde_yaml::from_str(&source)
        .with_context(|| format!("parse automation history from {}", path.display()))
}

async fn write_history(path: &Path, history: &AutomationHistory) -> Result<()> {
    let source = serde_yaml::to_string(history)?;
    dwo_agent_service::atomic_file::write(path, source.into_bytes()).await
}

fn answer_preview(content: &str) -> String {
    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated = normalized.chars().count() > 100;
    let mut preview = normalized
        .chars()
        .take(if truncated { 97 } else { 100 })
        .collect::<String>();
    if truncated {
        preview.push_str("...");
    }
    preview
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
    anyhow::ensure!(
        (1..=86_400).contains(&config.timeout_seconds),
        "automation.timeoutSeconds must be between 1 and 86400"
    );
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

fn default_timeout_seconds() -> u64 {
    900
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

    #[test]
    fn answer_preview_is_normalized_and_never_exceeds_one_hundred_characters() {
        let source = format!("  first\n\n{}  ", "x".repeat(120));
        let preview = answer_preview(&source);
        assert_eq!(preview.chars().count(), 100);
        assert!(preview.starts_with("first "));
        assert!(preview.ends_with("..."));
    }
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
        assert_eq!(config.timeout_seconds, 900);
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
    fn rejects_invalid_timeout() {
        let config: AutomationConfig =
            serde_yaml::from_str("enabled: false\ntimeoutSeconds: 0\njobs: []\n").unwrap();
        let error = validate_config(&config).unwrap_err();
        assert!(error.to_string().contains("timeoutSeconds"));
    }

    #[test]
    fn disabled_config_has_no_next_runs() {
        let config = AutomationConfig {
            enabled: false,
            timeout_seconds: default_timeout_seconds(),
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
            model: None,
            reasoning: None,
            policy: None,
        };
        let build_runtime = || {
            AutomationRuntime::new(
                service.clone(),
                root.path().to_path_buf(),
                AutomationConfig {
                    enabled: false,
                    timeout_seconds: default_timeout_seconds(),
                    jobs: vec![job.clone()],
                },
                "test-model".to_string(),
                None,
                SessionMode::Watch,
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
            finish_reason: Some("failed".to_string()),
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
