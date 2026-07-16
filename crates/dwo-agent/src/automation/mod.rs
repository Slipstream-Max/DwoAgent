use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use dwo_agent_service::{
    AgentService, EndpointId, NewSession, SessionEventPayload, SessionId, SessionLlmSettings,
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

#[derive(Deserialize)]
struct ProfileAutomation {
    #[serde(default)]
    automation: AutomationConfig,
}

pub struct AutomationRuntime {
    service: Arc<AgentService>,
    profile_root: PathBuf,
    profile_path: PathBuf,
    default_model: String,
    default_mode: SessionMode,
    shutdown: CancellationToken,
    state: Mutex<RuntimeState>,
}

impl AutomationRuntime {
    pub fn new(
        service: Arc<AgentService>,
        profile_root: PathBuf,
        config: AutomationConfig,
        default_model: String,
        default_mode: SessionMode,
        shutdown: CancellationToken,
    ) -> Result<Arc<Self>> {
        validate_config(&config)?;
        let source = std::fs::read_to_string(profile_root.join("profile.yaml"))?;
        let next_runs = build_next_runs(&config)?;
        Ok(Arc::new(Self {
            service,
            profile_path: profile_root.join("profile.yaml"),
            profile_root,
            default_model,
            default_mode,
            shutdown,
            state: Mutex::new(RuntimeState {
                source,
                config,
                next_runs,
                active: BTreeMap::new(),
            }),
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

    pub async fn run_now(self: &Arc<Self>, name: &str) -> Result<AutomationRunRecord> {
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
        self.run_job(job, false).await
    }

    async fn scheduler_loop(self: Arc<Self>) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(error) = self.reload_if_changed().await {
                        eprintln!("reload automation configuration: {error:#}");
                    }
                    for job in self.take_due_jobs().await {
                        let runtime = self.clone();
                        tokio::spawn(async move {
                            if let Err(error) = runtime.run_job(job.clone(), true).await {
                                eprintln!("automation job {} failed: {error:#}", job.name);
                            }
                        });
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
                Err(error) => eprintln!("schedule automation job {}: {error:#}", job.name),
            }
        }
        jobs
    }

    async fn run_job(
        self: &Arc<Self>,
        job: AutomationJob,
        scheduled: bool,
    ) -> Result<AutomationRunRecord> {
        let run_id = format!("run-{}", Uuid::new_v4().simple());
        let mut record = AutomationRunRecord {
            run_id: run_id.clone(),
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

        let result = self.execute_job(&job, &mut record).await;
        if let Err(error) = result {
            record.status = AutomationRunStatus::Failed;
            record.error = Some(format!("{error:#}"));
        }
        record.finished_at = Some(Utc::now().to_rfc3339());
        self.clear_active(&run_id).await;
        Ok(record)
    }

    async fn execute_job(
        &self,
        job: &AutomationJob,
        record: &mut AutomationRunRecord,
    ) -> Result<()> {
        let agent = match &job.session {
            AutomationSession::New { cwd, title } => {
                let cwd = if cwd.is_absolute() {
                    cwd.clone()
                } else {
                    self.profile_root.join(cwd)
                };
                self.service
                    .create(NewSession {
                        title: title
                            .clone()
                            .unwrap_or_else(|| format!("automation/{}", job.name)),
                        cwd,
                        mode: self.default_mode,
                        llm: SessionLlmSettings {
                            model: self.default_model.clone(),
                            reasoning: None,
                        },
                    })
                    .await?
            }
            AutomationSession::Fixed { session_id } => {
                let id = SessionId::parse(session_id.clone()).map_err(anyhow::Error::msg)?;
                self.service.load(&id).await?
            }
        };
        record.session_id = Some(agent.id().to_string());
        self.set_active(record.clone()).await;

        let endpoint = EndpointId::new();
        let mut subscription = agent.attach(endpoint.clone()).await?;
        let turn_id = agent.prompt(endpoint.clone(), job.prompt.clone()).await?;
        record.turn_id = Some(turn_id.to_string());
        self.set_active(record.clone()).await;

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
                        SessionEventPayload::AssistantCompleted { turn_id: event_turn, content }
                            if event_turn == turn_id => record.response = content,
                        SessionEventPayload::PermissionRequested { turn_id: event_turn, permission }
                            if event_turn == turn_id => {
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
                            if event_turn == turn_id => {
                                record.status = AutomationRunStatus::Completed;
                                return Ok(());
                            }
                        SessionEventPayload::TurnCancelled { turn_id: event_turn }
                            if event_turn == turn_id => {
                                record.status = AutomationRunStatus::Cancelled;
                                return Ok(());
                            }
                        SessionEventPayload::TurnFailed { turn_id: event_turn, error }
                            if event_turn == turn_id => bail!(error),
                        _ => {}
                    }
                }
            }
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

    #[test]
    fn parses_new_and_fixed_jobs() {
        let config: AutomationConfig = serde_yaml::from_str(
            r#"
enabled: true
jobs:
  - name: fresh
    schedule: { cron: "0 9 * * *", timezone: Asia/Shanghai }
    session: { mode: new, cwd: projects/demo }
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
            AutomationSession::New { .. }
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
}
