use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use dwo_agent_service::{
    EndpointId, ExternalRuleFile, NewSession, SessionEventPayload, SessionId, SessionListQuery,
    SessionLlmSettings, SessionService, SessionServiceError, SessionSubscription, TurnId,
};
use dwo_project::ProjectService;
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
    pub topic_id: Option<String>,
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
    pub project_id: String,
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
    pub project_id: String,
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
    projects: BTreeMap<String, ProjectAutomationState>,
    active: BTreeMap<String, AutomationRunRecord>,
}

struct ProjectAutomationState {
    config: AutomationConfig,
    next_runs: BTreeMap<String, DateTime<Utc>>,
    history: AutomationHistory,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AutomationHistory {
    #[serde(default)]
    runs: Vec<AutomationRunRecord>,
    #[serde(default)]
    once_sessions: BTreeMap<String, String>,
}

struct AutomationExecution {
    record: AutomationRunRecord,
    session_id: SessionId,
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
    service: Arc<SessionService>,
    projects: Arc<ProjectService>,
    defaults: Mutex<AutomationDefaults>,
    shutdown: CancellationToken,
    state: Mutex<RuntimeState>,
    session_queues: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
}

impl AutomationRuntime {
    pub fn new(
        service: Arc<SessionService>,
        projects: Arc<ProjectService>,
        default_model: String,
        default_reasoning: Option<String>,
        default_mode: SessionMode,
        shutdown: CancellationToken,
    ) -> Result<Arc<Self>> {
        let mut project_states = BTreeMap::new();
        for project in projects.list() {
            let config = read_project_config(&projects.automation_config_path(&project.id)?)?;
            validate_project_config(&project, &config)?;
            let next_runs = build_next_runs(&config)?;
            let mut history = read_history(&projects.automation_history_path(&project.id)?)?;
            mark_interrupted_runs(&mut history);
            project_states.insert(
                project.id,
                ProjectAutomationState {
                    config,
                    next_runs,
                    history,
                },
            );
        }
        Ok(Arc::new(Self {
            service,
            projects,
            defaults: Mutex::new(AutomationDefaults {
                model: default_model,
                reasoning: default_reasoning,
                mode: default_mode,
            }),
            shutdown,
            state: Mutex::new(RuntimeState {
                projects: project_states,
                active: BTreeMap::new(),
            }),
            session_queues: Mutex::new(BTreeMap::new()),
        }))
    }

    pub fn start(self: &Arc<Self>) {
        let runtime = self.clone();
        tokio::spawn(async move { runtime.scheduler_loop().await });
    }

    pub async fn list(&self, project_id: Option<&str>) -> Vec<AutomationJobStatus> {
        let (projects, active) = {
            let state = self.state.lock().await;
            let projects = state
                .projects
                .iter()
                .filter(|(id, _)| project_id.is_none_or(|wanted| wanted == id.as_str()))
                .map(|(id, project)| {
                    (
                        id.clone(),
                        project.config.clone(),
                        project.next_runs.clone(),
                        project.history.clone(),
                    )
                })
                .collect::<Vec<_>>();
            (projects, state.active.values().cloned().collect::<Vec<_>>())
        };
        let default_model = self.defaults.lock().await.model.clone();
        let mut sessions = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .service
                .list(SessionListQuery::new(cursor, Some(500)))
                .await
                .unwrap_or_default();
            sessions.extend(page.sessions);
            let Some(next) = page.next_cursor else {
                break;
            };
            cursor = Some(next);
        }
        let mut statuses = Vec::new();
        for (project_id, config, next_runs, history) in projects {
            for job in config.jobs {
                let bound_session_id = match &job.session {
                    AutomationSession::New {
                        behavior: AutomationNewBehavior::Once,
                        ..
                    } => history.once_sessions.get(&job.name).cloned(),
                    AutomationSession::Fixed { session_id } => Some(session_id.clone()),
                    AutomationSession::New { .. } => None,
                };
                let effective_model = bound_session_id
                    .as_ref()
                    .and_then(|session_id| {
                        sessions
                            .iter()
                            .find(|item| item.session_id.as_str() == session_id)
                    })
                    .map(|item| item.model.clone())
                    .or_else(|| job.model.clone())
                    .unwrap_or_else(|| default_model.clone());
                statuses.push(AutomationJobStatus {
                    project_id: project_id.clone(),
                    scheduler_enabled: config.enabled,
                    next_run_at: next_runs.get(&job.name).map(DateTime::to_rfc3339),
                    active_runs: active
                        .iter()
                        .filter(|record| record.project_id == project_id && record.job == job.name)
                        .cloned()
                        .collect(),
                    recent_runs: history
                        .runs
                        .iter()
                        .rev()
                        .filter(|record| record.job == job.name)
                        .take(10)
                        .cloned()
                        .collect(),
                    bound_session_id,
                    effective_model,
                    job,
                });
            }
        }
        statuses
    }

    pub async fn status(&self, project_id: &str, name: &str) -> Result<AutomationJobStatus> {
        self.list(Some(project_id))
            .await
            .into_iter()
            .find(|status| status.job.name == name)
            .with_context(|| format!("automation job not found: {name}"))
    }

    pub async fn history(
        &self,
        project_id: &str,
        name: Option<&str>,
        limit: usize,
    ) -> Vec<AutomationRunRecord> {
        let limit = limit.clamp(1, 100);
        let state = self.state.lock().await;
        state
            .projects
            .get(project_id)
            .into_iter()
            .flat_map(|project| project.history.runs.iter())
            .rev()
            .filter(|run| name.is_none_or(|name| run.job == name))
            .take(limit)
            .cloned()
            .collect()
    }

    pub async fn remove_job_state(
        &self,
        project_id: &str,
        name: Option<&str>,
        all: bool,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        let project = state
            .projects
            .get_mut(project_id)
            .with_context(|| format!("project not found: {project_id}"))?;
        if all {
            project.history.runs.clear();
            project.history.once_sessions.clear();
        } else if let Some(name) = name {
            project.history.runs.retain(|run| run.job != name);
            project.history.once_sessions.remove(name);
        }
        write_history(
            &self.projects.automation_history_path(project_id)?,
            &project.history,
        )
        .await?;
        Ok(())
    }

    pub async fn run_now(
        self: &Arc<Self>,
        project_id: &str,
        name: &str,
        caller: Option<SessionId>,
    ) -> Result<AutomationRunRecord> {
        let job = {
            let state = self.state.lock().await;
            state
                .projects
                .get(project_id)
                .with_context(|| format!("project not found: {project_id}"))?
                .config
                .jobs
                .iter()
                .find(|job| job.name == name)
                .cloned()
                .with_context(|| format!("automation job not found: {name}"))?
        };
        self.start_job(project_id.to_string(), job, false, caller)
            .await
    }

    async fn scheduler_loop(self: Arc<Self>) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = interval.tick() => {
                    for (project_id, job) in self.take_due_jobs().await {
                        if let Err(error) = self.start_job(project_id, job, true, None).await {
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

    pub async fn apply_defaults(
        &self,
        default_model: String,
        default_reasoning: Option<String>,
        default_mode: SessionMode,
    ) {
        *self.defaults.lock().await = AutomationDefaults {
            model: default_model,
            reasoning: default_reasoning,
            mode: default_mode,
        };
    }

    pub async fn update_project_config<F>(&self, project_id: &str, update: F) -> Result<()>
    where
        F: FnOnce(&mut AutomationConfig) -> Result<()>,
    {
        let project = self.projects.get(project_id)?;
        let mut state = self.state.lock().await;
        let mut config = state
            .projects
            .get(project_id)
            .map(|project| project.config.clone())
            .unwrap_or_default();
        update(&mut config)?;
        validate_project_config(&project, &config)?;
        let next_runs = build_next_runs(&config)?;
        write_config(&self.projects.automation_config_path(project_id)?, &config).await?;
        let project_state = state
            .projects
            .entry(project_id.to_string())
            .or_insert_with(|| ProjectAutomationState {
                config: AutomationConfig::default(),
                next_runs: BTreeMap::new(),
                history: AutomationHistory::default(),
            });
        project_state.config = config;
        project_state.next_runs = next_runs;
        Ok(())
    }

    pub async fn move_topic_jobs(
        &self,
        project_id: &str,
        from_topic_id: &str,
        to_topic_id: &str,
    ) -> Result<()> {
        self.update_project_config(project_id, |config| {
            for job in &mut config.jobs {
                if job.topic_id.as_deref() == Some(from_topic_id) {
                    job.topic_id = Some(to_topic_id.to_string());
                }
            }
            Ok(())
        })
        .await
    }

    async fn take_due_jobs(&self) -> Vec<(String, AutomationJob)> {
        let now = Utc::now();
        let mut state = self.state.lock().await;
        let mut due = Vec::new();
        for (project_id, project) in &mut state.projects {
            if !project.config.enabled {
                continue;
            }
            let jobs = project
                .config
                .jobs
                .iter()
                .filter(|job| {
                    job.enabled
                        && project
                            .next_runs
                            .get(&job.name)
                            .is_some_and(|next| next <= &now)
                })
                .cloned()
                .collect::<Vec<_>>();
            for job in jobs {
                match next_run(&job.schedule) {
                    Ok(next) => {
                        project.next_runs.insert(job.name.clone(), next);
                    }
                    Err(error) => tracing::warn!(
                        event = "automation.schedule_failed",
                        project_id = %project_id,
                        job = %job.name,
                        error = %format!("{error:#}"),
                        "schedule automation job failed"
                    ),
                }
                due.push((project_id.clone(), job));
            }
        }
        due
    }

    async fn start_job(
        self: &Arc<Self>,
        project_id: String,
        job: AutomationJob,
        scheduled: bool,
        caller: Option<SessionId>,
    ) -> Result<AutomationRunRecord> {
        let run_id = format!("run-{}", Uuid::new_v4().simple());
        let record = AutomationRunRecord {
            run_id,
            project_id: project_id.clone(),
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
        let session_id = match self.resolve_session(&project_id, &job).await {
            Ok(session_id) => session_id,
            Err(error) => {
                started.status = AutomationRunStatus::Failed;
                started.finished_at = Some(Utc::now().to_rfc3339());
                started.error = Some(format!("{error:#}"));
                self.clear_active(&started.run_id).await;
                self.append_history(started).await?;
                return Err(error);
            }
        };
        started.session_id = Some(session_id.to_string());
        self.set_active(started.clone()).await;
        if let Some(model) = &job.model {
            self.service
                .set_config(
                    &session_id,
                    dwo_agent_service::SessionConfigUpdate::Model(model.clone()),
                )
                .await?;
        }
        if let Some(reasoning) = &job.reasoning {
            self.service
                .set_config(
                    &session_id,
                    dwo_agent_service::SessionConfigUpdate::Reasoning(Some(reasoning.clone())),
                )
                .await?;
        }
        if let Some(policy) = job.policy {
            self.service
                .set_config(
                    &session_id,
                    dwo_agent_service::SessionConfigUpdate::Mode(policy),
                )
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
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let returned = started.clone();
        let runtime = self.clone();
        tokio::spawn(async move {
            runtime
                .run_queued_job(started, job.prompt, session_id, session_queue, caller)
                .await
        });
        Ok(returned)
    }

    async fn resolve_session(&self, project_id: &str, job: &AutomationJob) -> Result<SessionId> {
        let session_id = match &job.session {
            AutomationSession::New { behavior, title } => match behavior {
                AutomationNewBehavior::EveryTime => {
                    self.create_session(project_id, job, title).await?
                }
                AutomationNewBehavior::Once => self.once_session(project_id, job, title).await?,
            },
            AutomationSession::Fixed { session_id } => {
                let id = SessionId::parse(session_id.clone()).map_err(anyhow::Error::msg)?;
                self.service.load(&id).await?;
                id
            }
        };
        self.bind_session_to_job_topic(project_id, job, &session_id)
            .await?;
        Ok(session_id)
    }

    async fn start_execution(
        &self,
        session_id: SessionId,
        prompt: String,
        record: &mut AutomationRunRecord,
    ) -> Result<AutomationExecution> {
        let endpoint = EndpointId::new();
        let mut subscription = self.service.subscribe(&session_id, None).await?;
        let turn_id = loop {
            match self
                .service
                .prompt(
                    &session_id,
                    endpoint.clone(),
                    dwo_context::MessageContent::text(prompt.clone()),
                )
                .await
            {
                Ok(accepted) => break accepted.turn_id,
                Err(SessionServiceError::SessionBusy(_)) => {}
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
            session_id,
            endpoint,
            subscription,
            turn_id,
        })
    }

    async fn create_session(
        &self,
        project_id: &str,
        job: &AutomationJob,
        title: &Option<String>,
    ) -> Result<SessionId> {
        let project = self.projects.get(project_id)?;
        let topic_id = job
            .topic_id
            .as_deref()
            .unwrap_or(&project.board.uncategorized_topic_id);
        let external_rule_files = vec![ExternalRuleFile::new(
            self.projects.agents_path(project_id, topic_id)?,
            project.pwd.clone(),
        )];
        let defaults = self.defaults.lock().await.clone();
        let model = job.model.clone().unwrap_or_else(|| defaults.model.clone());
        let reasoning = job.reasoning.clone().or_else(|| {
            job.model
                .is_none()
                .then(|| defaults.reasoning.clone())
                .flatten()
        });
        let id = SessionId::new();
        self.service
            .create(NewSession {
                from: None,
                id: Some(id.clone()),
                parent_session_id: None,
                title: Some(
                    title
                        .clone()
                        .unwrap_or_else(|| format!("automation/{}", job.name)),
                ),
                cwd: Some(project.pwd),
                worktree_id: None,
                external_rule_files,
                mode: Some(job.policy.unwrap_or(defaults.mode)),
                llm: Some(SessionLlmSettings::new(model, reasoning)),
                ephemeral: false,
            })
            .await?;
        Ok(id)
    }

    async fn bind_session_to_job_topic(
        &self,
        project_id: &str,
        job: &AutomationJob,
        session_id: &SessionId,
    ) -> Result<()> {
        let project = self.projects.get(project_id)?;
        let topic_id = job
            .topic_id
            .as_deref()
            .unwrap_or(&project.board.uncategorized_topic_id);
        self.projects.agents_path(project_id, topic_id)?;
        let snapshot = self.service.snapshot(session_id).await?;
        anyhow::ensure!(
            snapshot.record.info.cwd == project.pwd,
            "automation session cwd does not match the topic project pwd"
        );
        let source = ExternalRuleFile::new(
            self.projects.agents_path(project_id, topic_id)?,
            project.pwd.clone(),
        );
        self.service
            .set_external_rule_files(session_id, vec![source]);
        self.projects
            .assign_session(project_id, topic_id, session_id.to_string())?;
        Ok(())
    }

    async fn once_session(
        &self,
        project_id: &str,
        job: &AutomationJob,
        title: &Option<String>,
    ) -> Result<SessionId> {
        let bound = self
            .state
            .lock()
            .await
            .projects
            .get(project_id)
            .and_then(|project| project.history.once_sessions.get(&job.name))
            .cloned();
        if let Some(bound) = bound {
            let id = SessionId::parse(bound).map_err(anyhow::Error::msg)?;
            match self.service.load(&id).await {
                Ok(_) => return Ok(id),
                Err(SessionServiceError::SessionNotFound(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let session_id = self.create_session(project_id, job, title).await?;
        let history = {
            let mut state = self.state.lock().await;
            let project = state
                .projects
                .get_mut(project_id)
                .with_context(|| format!("project not found: {project_id}"))?;
            project
                .history
                .once_sessions
                .insert(job.name.clone(), session_id.to_string());
            project.history.clone()
        };
        write_history(
            &self.projects.automation_history_path(project_id)?,
            &history,
        )
        .await?;
        Ok(session_id)
    }

    async fn run_queued_job(
        self: &Arc<Self>,
        mut record: AutomationRunRecord,
        prompt: String,
        session_id: SessionId,
        session_queue: Arc<Mutex<()>>,
        caller: Option<SessionId>,
    ) {
        let queue = tokio::select! {
            _ = self.shutdown.cancelled() => None,
            guard = session_queue.lock_owned() => Some(guard),
        };
        let result = if queue.is_some() {
            match self.start_execution(session_id, prompt, &mut record).await {
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
            session_id,
            endpoint,
            subscription,
            turn_id,
        } = execution;
        let timeout_seconds = self
            .state
            .lock()
            .await
            .projects
            .get(&record.project_id)
            .map(|project| project.config.timeout_seconds)
            .unwrap_or_else(default_timeout_seconds);
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(timeout_seconds));
        tokio::pin!(timeout);
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    let _ = self.service.cancel(session_id, Some(turn_id.clone())).await;
                    record.status = AutomationRunStatus::Cancelled;
                    return Ok(());
                }
                _ = &mut timeout => {
                    let _ = self.service.cancel(session_id, Some(turn_id.clone())).await;
                    record.status = AutomationRunStatus::Failed;
                    record.error = Some(format!(
                        "automation exceeded its {} second timeout",
                        timeout_seconds
                    ));
                    record.finish_reason = Some("timeout".to_string());
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
                                self.service.respond_permission(
                                    session_id,
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
        if let Err(error) = self
            .service
            .prompt_internal(&caller, dwo_context::MessageContent::text(notification))
            .await
        {
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
        let project_id = record.project_id.clone();
        let mut state = self.state.lock().await;
        let project = state
            .projects
            .get_mut(&project_id)
            .with_context(|| format!("project not found: {project_id}"))?;
        project.history.runs.push(record);
        if project.history.runs.len() > 100 {
            let remove = project.history.runs.len() - 100;
            project.history.runs.drain(..remove);
        }
        write_history(
            &self.projects.automation_history_path(&project_id)?,
            &project.history,
        )
        .await
    }
}

fn automation_result_notification(record: &AutomationRunRecord) -> String {
    format!(
        "<automation_result>\n{}\n</automation_result>",
        serde_json::json!({
            "run_id": record.run_id,
            "project_id": record.project_id,
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

fn read_history(path: &Path) -> Result<AutomationHistory> {
    if !path.is_file() {
        return Ok(AutomationHistory::default());
    }
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("read automation history from {}", path.display()))?;
    serde_yaml::from_str(&source)
        .with_context(|| format!("parse automation history from {}", path.display()))
}

fn read_project_config(path: &Path) -> Result<AutomationConfig> {
    if !path.is_file() {
        return Ok(AutomationConfig::default());
    }
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("read automation config from {}", path.display()))?;
    serde_yaml::from_str(&source)
        .with_context(|| format!("parse automation config from {}", path.display()))
}

fn mark_interrupted_runs(history: &mut AutomationHistory) {
    for run in &mut history.runs {
        if matches!(
            run.status,
            AutomationRunStatus::Queued | AutomationRunStatus::Running
        ) {
            run.status = AutomationRunStatus::Failed;
            run.finished_at = Some(Utc::now().to_rfc3339());
            run.error = Some("daemon restarted before the automation run completed".to_string());
            run.finish_reason = Some("host_restarted".to_string());
        }
    }
}

async fn write_config(path: &Path, config: &AutomationConfig) -> Result<()> {
    let source = serde_yaml::to_string(config)?;
    dwo_agent_service::atomic_file::write(path, source.into_bytes()).await
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

fn validate_project_config(
    project: &dwo_project::Project,
    config: &AutomationConfig,
) -> Result<()> {
    validate_config(config)?;
    for job in &config.jobs {
        if let Some(topic_id) = &job.topic_id {
            anyhow::ensure!(
                project
                    .board
                    .topics
                    .iter()
                    .any(|topic| &topic.id == topic_id),
                "automation job {} refers to an unknown topic: {topic_id}",
                job.name
            );
        }
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
    #[test]
    fn parses_new_and_fixed_jobs() {
        let config: AutomationConfig = serde_yaml::from_str(
            r#"
enabled: true
jobs:
  - name: fresh
    schedule: { cron: "0 9 * * *", timezone: Asia/Shanghai }
    session: { mode: new, behavior: once }
    topicId: topic-demo
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
        let error = serde_yaml::from_str::<AutomationSession>("mode: new\n").unwrap_err();
        assert!(error.to_string().contains("behavior"));
    }

    #[test]
    fn automation_result_contains_terminal_state_and_ids() {
        let notification = automation_result_notification(&AutomationRunRecord {
            run_id: "run-test".to_string(),
            project_id: "project-test".to_string(),
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
        assert_eq!(value["project_id"], "project-test");
        assert_eq!(value["session_id"], "session-test");
        assert_eq!(value["turn_id"], "turn-test");
        assert_eq!(value["status"], "failed");
        assert_eq!(value["content"], "partial response");
        assert_eq!(value["error"], "model failed");
    }
}
