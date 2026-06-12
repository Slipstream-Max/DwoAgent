//! Automation scheduler for triggering ordinary agent sessions.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::Timelike;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::agent::constants::PERMISSION_REJECT_ONCE;
use crate::agent::service::AgentService;
use crate::agent::session_agent::SessionAgent;
use crate::config::loader::utc_iso;
use crate::context::content_block;
use crate::ingress::bridge::SessionLeaseRegistry;
use crate::ingress::response::{ChannelResponseDetail, ChannelUpdateCollector};
use crate::tools::subagent_tool_runtime::PermissionRequester;
use crate::utils::files::read_utf8_text;

pub const AUTOMATION_CONFIG_FILE: &str = "automation.yaml";
const AUTOMATION_SESSION_SUBDIR: &str = "automation";
const AUTOMATION_STATE_DIR: &str = "automation_state";
const AUTOMATION_STATE_FILE: &str = "state.yaml";
const RUN_FILE: &str = "run.yaml";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutomationConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub jobs: Vec<AutomationJobConfig>,
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            jobs: Vec::new(),
        }
    }
}

impl AutomationConfig {
    pub fn has_enabled_jobs(&self) -> bool {
        self.enabled && self.jobs.iter().any(|job| job.enabled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutomationJobConfig {
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_workspace_dir")]
    pub workspace_dir: String,
    #[serde(default)]
    pub session: AutomationSessionConfig,
    pub schedule: AutomationSchedule,
    pub prompt: String,
    #[serde(default)]
    pub response_detail: ChannelResponseDetail,
    #[serde(default)]
    pub notify: Vec<AutomationNotifyConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum AutomationSessionConfig {
    New,
    Fixed { session_id: String },
    Sticky,
}

impl Default for AutomationSessionConfig {
    fn default() -> Self {
        Self::New
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AutomationSchedule {
    Interval { every_seconds: u64 },
    Daily { at: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutomationNotifyConfig {
    pub channel: AutomationNotifyChannel,
    #[serde(default)]
    pub recipient: Option<AutomationNotifyRecipient>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AutomationNotifyChannel {
    Weixin,
    Feishu,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AutomationNotifyRecipient {
    #[serde(rename = "type")]
    pub recipient_type: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AutomationStickyState {
    job_id: String,
    session_id: String,
    updated_at: String,
}

pub fn load_automation_config(agent_structure_dir: &Path) -> Result<AutomationConfig> {
    let path = agent_structure_dir.join(AUTOMATION_CONFIG_FILE);
    if !path.is_file() {
        return Ok(AutomationConfig::default());
    }

    let text = read_utf8_text(&path)?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(AutomationConfig::default());
    }

    let loaded: Value = serde_yaml::from_str(trimmed)
        .with_context(|| format!("parse YAML in {}", path.display()))?;

    match loaded {
        Value::Null => Ok(AutomationConfig::default()),
        Value::Object(map) => {
            let config: AutomationConfig = serde_json::from_value(Value::Object(map))
                .with_context(|| format!("Invalid YAML config in {}", path.display()))?;
            Ok(config)
        }
        _ => bail!(
            "Invalid YAML config in {}: expected a mapping",
            path.display()
        ),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutomationRunRecord {
    pub job_id: String,
    pub run_id: String,
    pub session_id: String,
    pub status: AutomationRunStatus,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_text: Option<String>,
    #[serde(default)]
    pub response_text: String,
    #[serde(default)]
    pub notify: Vec<AutomationNotifyConfig>,
    #[serde(default)]
    pub notifications: Vec<AutomationNotificationRecord>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AutomationRunStatus {
    Running,
    Completed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutomationNotificationRecord {
    pub channel: AutomationNotifyChannel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    pub status: AutomationNotificationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AutomationNotificationStatus {
    Sent,
    Failed,
    Skipped,
}

#[async_trait]
pub trait AutomationNotificationSink: Send + Sync {
    async fn send(&self, notify: &AutomationNotifyConfig, text: &str) -> Result<String>;
}

#[derive(Clone, Default)]
pub struct AutomationNotificationSinks {
    pub weixin: Option<Arc<dyn AutomationNotificationSink>>,
    pub feishu: Option<Arc<dyn AutomationNotificationSink>>,
}

enum SessionSelection {
    Ready {
        session: Arc<SessionAgent>,
        lease_holder: String,
    },
    Skipped {
        session_id: String,
        session_dir: PathBuf,
        reason: String,
    },
}

pub struct AutomationRuntime {
    agent: Arc<AgentService>,
    leases: Arc<SessionLeaseRegistry>,
    agent_structure_dir: PathBuf,
    jobs: Vec<AutomationJobConfig>,
    sinks: AutomationNotificationSinks,
}

impl AutomationRuntime {
    pub fn new(
        agent: Arc<AgentService>,
        leases: Arc<SessionLeaseRegistry>,
        agent_structure_dir: &Path,
        jobs: Vec<AutomationJobConfig>,
        sinks: AutomationNotificationSinks,
    ) -> Self {
        Self {
            agent,
            leases,
            agent_structure_dir: agent_structure_dir.to_path_buf(),
            jobs,
            sinks,
        }
    }

    pub async fn run(self) -> Result<()> {
        let enabled_jobs = self
            .jobs
            .into_iter()
            .filter(|job| job.enabled)
            .collect::<Vec<_>>();
        if enabled_jobs.is_empty() {
            futures::future::pending::<()>().await;
            return Ok(());
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<()>>(enabled_jobs.len());
        for job in enabled_jobs {
            let runtime = Self {
                agent: self.agent.clone(),
                leases: self.leases.clone(),
                agent_structure_dir: self.agent_structure_dir.clone(),
                jobs: Vec::new(),
                sinks: self.sinks.clone(),
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let result = runtime.run_job_loop(job).await;
                let _ = tx.send(result).await;
            });
        }
        drop(tx);

        match rx.recv().await {
            Some(result) => result,
            None => Ok(()),
        }
    }

    async fn run_job_loop(self, job: AutomationJobConfig) -> Result<()> {
        loop {
            let delay = next_schedule_delay(&job.schedule)?;
            tokio::time::sleep(delay).await;
            if let Err(err) = self.run_job_once(&job).await {
                tracing::warn!(
                    target: "automation",
                    job_id = %job.id,
                    error = %format!("{err:#}"),
                    "automation job run failed"
                );
            }
        }
    }

    async fn run_job_once(&self, job: &AutomationJobConfig) -> Result<AutomationRunRecord> {
        validate_job(job)?;
        let workspace_dir = resolve_config_path(&self.agent_structure_dir, &job.workspace_dir);
        let run_id = new_run_id();
        let started_at = utc_iso();
        let blocks = vec![content_block::text(&job.prompt)?];
        let user_input = Value::String(job.prompt.clone());

        let selection = self.select_session(job, &workspace_dir).await?;
        let (session, lease_holder) = match selection {
            SessionSelection::Ready {
                session,
                lease_holder,
            } => (session, lease_holder),
            SessionSelection::Skipped {
                session_id,
                session_dir,
                reason,
            } => {
                let mut record = AutomationRunRecord {
                    job_id: job.id.clone(),
                    run_id,
                    session_id,
                    status: AutomationRunStatus::Skipped,
                    started_at,
                    finished_at: Some(utc_iso()),
                    stop_reason: None,
                    error: Some(reason),
                    detail_text: None,
                    response_text: String::new(),
                    notify: job.notify.clone(),
                    notifications: Vec::new(),
                };
                record.notifications = self.send_notifications(job, &record).await;
                let run_path =
                    automation_run_dir(&session_dir, &job.id, &record.run_id).join(RUN_FILE);
                write_run_record(&run_path, &record)?;
                return Ok(record);
            }
        };

        let run_dir = automation_run_dir(session.session_dir(), &job.id, &run_id);
        let run_path = run_dir.join(RUN_FILE);
        let mut record = AutomationRunRecord {
            job_id: job.id.clone(),
            run_id,
            session_id: session.session_id().to_string(),
            status: AutomationRunStatus::Running,
            started_at,
            finished_at: None,
            stop_reason: None,
            error: None,
            detail_text: None,
            response_text: String::new(),
            notify: job.notify.clone(),
            notifications: Vec::new(),
        };
        let session_id = record.session_id.clone();
        if let Err(err) = write_run_record(&run_path, &record) {
            self.leases
                .release_if_holder(&session_id, &lease_holder)
                .await;
            return Err(err);
        }

        let collector = ChannelUpdateCollector::new(job.response_detail);
        let run_result = session
            .clone()
            .run_prompt(
                user_input,
                blocks,
                collector.emitter(),
                rejecting_permission_requester(),
            )
            .await;
        let collected = collector.finish().await;
        record.detail_text = collected.detail_text;
        record.response_text = collected.response_text;
        record.finished_at = Some(utc_iso());

        match run_result {
            Ok(stop_reason) => {
                record.status = AutomationRunStatus::Completed;
                record.stop_reason = Some(stop_reason);
            }
            Err(err) => {
                record.status = AutomationRunStatus::Failed;
                record.error = Some(format!("{err:#}"));
            }
        }
        record.notifications = self.send_notifications(job, &record).await;
        let write_result = write_run_record(&run_path, &record);
        self.leases
            .release_if_holder(&session_id, &lease_holder)
            .await;
        write_result?;
        Ok(record)
    }

    async fn select_session(
        &self,
        job: &AutomationJobConfig,
        workspace_dir: &Path,
    ) -> Result<SessionSelection> {
        match &job.session {
            AutomationSessionConfig::New => {
                let session = self
                    .agent
                    .new_session(&workspace_dir.to_string_lossy())
                    .await?;
                let holder = automation_holder(&job.id);
                self.leases.acquire(session.session_id(), &holder).await?;
                Ok(SessionSelection::Ready {
                    session,
                    lease_holder: holder,
                })
            }
            AutomationSessionConfig::Fixed { session_id } => {
                let session = self.load_target_session(session_id).await?;
                self.prepare_existing_session(job, session).await
            }
            AutomationSessionConfig::Sticky => {
                let session = self.load_sticky_session(job, workspace_dir).await?;
                self.prepare_existing_session(job, session).await
            }
        }
    }

    async fn prepare_existing_session(
        &self,
        job: &AutomationJobConfig,
        session: Arc<SessionAgent>,
    ) -> Result<SessionSelection> {
        if let Some(holder) = self.leases.holder(session.session_id()).await {
            return Ok(SessionSelection::Skipped {
                session_id: session.session_id().to_string(),
                session_dir: session.session_dir().to_path_buf(),
                reason: format!("session {} is occupied by {holder}", session.session_id()),
            });
        }
        if session.is_active().await {
            return Ok(SessionSelection::Skipped {
                session_id: session.session_id().to_string(),
                session_dir: session.session_dir().to_path_buf(),
                reason: format!("session {} is active", session.session_id()),
            });
        }

        let holder = automation_holder(&job.id);
        if let Err(err) = self.leases.acquire(session.session_id(), &holder).await {
            return Ok(SessionSelection::Skipped {
                session_id: session.session_id().to_string(),
                session_dir: session.session_dir().to_path_buf(),
                reason: format!("{err:#}"),
            });
        }
        Ok(SessionSelection::Ready {
            session,
            lease_holder: holder,
        })
    }

    async fn load_target_session(&self, session_id: &str) -> Result<Arc<SessionAgent>> {
        let trimmed = session_id.trim();
        if trimmed.is_empty() {
            bail!("automation fixed session_id must not be empty");
        }
        self.agent
            .load_session(trimmed)
            .await?
            .ok_or_else(|| anyhow::anyhow!("automation session not found: {trimmed}"))
    }

    async fn load_sticky_session(
        &self,
        job: &AutomationJobConfig,
        workspace_dir: &Path,
    ) -> Result<Arc<SessionAgent>> {
        let state_path = sticky_state_path(&self.agent_structure_dir, &job.id);
        if let Some(state) = read_sticky_state(&state_path)? {
            if let Some(session) = self.agent.load_session(&state.session_id).await? {
                return Ok(session);
            }
        }

        let session = self
            .agent
            .new_session(&workspace_dir.to_string_lossy())
            .await?;
        write_sticky_state(&state_path, job, session.session_id())?;
        Ok(session)
    }

    async fn send_notifications(
        &self,
        job: &AutomationJobConfig,
        record: &AutomationRunRecord,
    ) -> Vec<AutomationNotificationRecord> {
        let mut out = Vec::new();
        if job.notify.is_empty() {
            return out;
        }
        let text = render_notification_text(job, record);
        for notify in &job.notify {
            let recipient = notify
                .recipient
                .as_ref()
                .map(|recipient| format!("{}:{}", recipient.recipient_type, recipient.id));
            let Some(sink) = self.sink_for(notify.channel.clone()) else {
                out.push(AutomationNotificationRecord {
                    channel: notify.channel.clone(),
                    recipient,
                    status: AutomationNotificationStatus::Skipped,
                    message_id: None,
                    error: Some("channel is not running or has no notification sink".to_string()),
                });
                continue;
            };
            match sink.send(notify, &text).await {
                Ok(message_id) => out.push(AutomationNotificationRecord {
                    channel: notify.channel.clone(),
                    recipient,
                    status: AutomationNotificationStatus::Sent,
                    message_id: Some(message_id),
                    error: None,
                }),
                Err(err) => out.push(AutomationNotificationRecord {
                    channel: notify.channel.clone(),
                    recipient,
                    status: AutomationNotificationStatus::Failed,
                    message_id: None,
                    error: Some(format!("{err:#}")),
                }),
            }
        }
        out
    }

    fn sink_for(
        &self,
        channel: AutomationNotifyChannel,
    ) -> Option<Arc<dyn AutomationNotificationSink>> {
        match channel {
            AutomationNotifyChannel::Weixin => self.sinks.weixin.clone(),
            AutomationNotifyChannel::Feishu => self.sinks.feishu.clone(),
        }
    }
}

fn automation_run_dir(session_dir: &Path, job_id: &str, run_id: &str) -> PathBuf {
    session_dir
        .join(AUTOMATION_SESSION_SUBDIR)
        .join(sanitize_filename(job_id))
        .join("runs")
        .join(run_id)
}

fn render_notification_text(job: &AutomationJobConfig, record: &AutomationRunRecord) -> String {
    let status = match record.status {
        AutomationRunStatus::Running => "running",
        AutomationRunStatus::Completed => "completed",
        AutomationRunStatus::Failed => "failed",
        AutomationRunStatus::Skipped => "skipped",
    };
    let mut text = format!(
        "automation `{}` {status}\nsession: {}\n/switch {}",
        job.id, record.session_id, record.session_id
    );
    if let Some(error) = record.error.as_deref() {
        text.push_str("\n\n[error]\n");
        text.push_str(error);
    }
    if let Some(detail) = record
        .detail_text
        .as_deref()
        .filter(|text| !text.is_empty())
    {
        text.push_str("\n\n");
        text.push_str(detail);
    }
    if !record.response_text.is_empty() {
        text.push_str("\n\n");
        text.push_str(&record.response_text);
    }
    text
}

fn validate_job(job: &AutomationJobConfig) -> Result<()> {
    if job.id.trim().is_empty() {
        bail!("automation job id must not be empty");
    }
    if job.prompt.trim().is_empty() {
        bail!("automation job `{}` prompt must not be empty", job.id);
    }
    if let AutomationSessionConfig::Fixed { session_id } = &job.session
        && session_id.trim().is_empty()
    {
        bail!(
            "automation job `{}` fixed session_id must not be empty",
            job.id
        );
    }
    Ok(())
}

fn rejecting_permission_requester() -> PermissionRequester {
    Arc::new(move |_target: String, _payload: Map<String, Value>| {
        Box::pin(async move { Ok(PERMISSION_REJECT_ONCE.to_string()) })
    })
}

fn write_run_record(path: &Path, record: &AutomationRunRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let text = serde_yaml::to_string(record)?;
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn new_run_id() -> String {
    format!(
        "{}-{}",
        sanitize_filename(&utc_iso()),
        uuid::Uuid::new_v4().simple()
    )
}

fn next_schedule_delay(schedule: &AutomationSchedule) -> Result<Duration> {
    match schedule {
        AutomationSchedule::Interval { every_seconds } => {
            if *every_seconds == 0 {
                bail!("automation interval every_seconds must be positive");
            }
            Ok(Duration::from_secs(*every_seconds))
        }
        AutomationSchedule::Daily { at } => {
            let target = parse_daily_seconds(at)?;
            let now = chrono::Local::now();
            let now_seconds = now.time().num_seconds_from_midnight();
            let mut delta = target as i64 - now_seconds as i64;
            if delta <= 0 {
                delta += 24 * 60 * 60;
            }
            Ok(Duration::from_secs(delta as u64))
        }
    }
}

fn parse_daily_seconds(at: &str) -> Result<u32> {
    let parts = at.split(':').collect::<Vec<_>>();
    if !(2..=3).contains(&parts.len()) {
        bail!("daily schedule `at` must be HH:MM or HH:MM:SS");
    }
    let hour: u32 = parts[0].parse().context("parse daily schedule hour")?;
    let minute: u32 = parts[1].parse().context("parse daily schedule minute")?;
    let second: u32 = if parts.len() == 3 {
        parts[2].parse().context("parse daily schedule second")?
    } else {
        0
    };
    if hour > 23 || minute > 59 || second > 59 {
        bail!("daily schedule `at` is out of range");
    }
    Ok(hour * 3600 + minute * 60 + second)
}

fn resolve_config_path(base_dir: &Path, configured: &str) -> PathBuf {
    let path = PathBuf::from(configured);
    let joined = if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    };
    std::fs::canonicalize(&joined).unwrap_or(joined)
}

fn sanitize_filename(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "run".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn automation_config_loads_from_automation_yaml() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join(AUTOMATION_CONFIG_FILE),
            r#"
enabled: true
jobs:
  - id: daily_digest
    enabled: true
    workspace_dir: .
    session:
      mode: fixed
      session_id: abc-123
    schedule:
      type: daily
      at: "09:00"
    prompt: "总结今天"
    response_detail: detailed
    notify:
      - channel: weixin
      - channel: feishu
        recipient:
          type: chat
          id: oc_xxx
"#,
        )
        .unwrap();

        let config = load_automation_config(tmp.path()).unwrap();

        assert!(config.enabled);
        assert!(config.has_enabled_jobs());
        assert_eq!(config.jobs.len(), 1);
        assert_eq!(config.jobs[0].id, "daily_digest");
        assert!(matches!(
            &config.jobs[0].session,
            AutomationSessionConfig::Fixed { session_id } if session_id == "abc-123"
        ));
        assert_eq!(
            config.jobs[0].response_detail,
            ChannelResponseDetail::Detailed
        );
        assert_eq!(config.jobs[0].notify.len(), 2);
    }

    #[test]
    fn automation_session_config_accepts_new_and_sticky() {
        let new_job: AutomationJobConfig = serde_yaml::from_str(
            r#"
id: one
session:
  mode: new
schedule:
  type: interval
  every_seconds: 1
prompt: run
"#,
        )
        .unwrap();
        assert!(matches!(new_job.session, AutomationSessionConfig::New));

        let sticky_job: AutomationJobConfig = serde_yaml::from_str(
            r#"
id: two
session:
  mode: sticky
schedule:
  type: interval
  every_seconds: 1
prompt: run
"#,
        )
        .unwrap();
        assert!(matches!(
            sticky_job.session,
            AutomationSessionConfig::Sticky
        ));
    }

    #[test]
    fn interval_schedule_requires_positive_seconds() {
        let err =
            next_schedule_delay(&AutomationSchedule::Interval { every_seconds: 0 }).unwrap_err();
        assert!(err.to_string().contains("positive"));
    }

    #[test]
    fn daily_schedule_accepts_hh_mm_and_hh_mm_ss() {
        assert_eq!(parse_daily_seconds("09:30").unwrap(), 9 * 3600 + 30 * 60);
        assert_eq!(
            parse_daily_seconds("23:59:58").unwrap(),
            23 * 3600 + 59 * 60 + 58
        );
        assert!(parse_daily_seconds("24:00").is_err());
    }

    #[test]
    fn sanitize_filename_removes_path_separators() {
        assert_eq!(sanitize_filename("daily:digest/one"), "daily-digest-one");
    }

    #[test]
    fn automation_run_dir_lives_under_session_dir() {
        let session_dir = PathBuf::from("sessions")
            .join("2026")
            .join("06")
            .join("12")
            .join("session-1");
        let run_dir = automation_run_dir(&session_dir, "daily:digest/one", "run-1");
        assert_eq!(
            run_dir,
            session_dir
                .join("automation")
                .join("daily-digest-one")
                .join("runs")
                .join("run-1")
        );
    }

    #[test]
    fn sticky_state_path_uses_sanitized_job_id() {
        let root = PathBuf::from("agent");
        assert_eq!(
            sticky_state_path(&root, "daily:digest/one"),
            root.join("automation_state")
                .join("daily-digest-one")
                .join("state.yaml")
        );
    }
}

fn read_sticky_state(path: &Path) -> Result<Option<AutomationStickyState>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = read_utf8_text(path)?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let state: AutomationStickyState =
        serde_yaml::from_str(trimmed).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(state))
}

fn write_sticky_state(path: &Path, job: &AutomationJobConfig, session_id: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let state = AutomationStickyState {
        job_id: job.id.clone(),
        session_id: session_id.to_string(),
        updated_at: utc_iso(),
    };
    let text = serde_yaml::to_string(&state)?;
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn sticky_state_path(agent_structure_dir: &Path, job_id: &str) -> PathBuf {
    agent_structure_dir
        .join(AUTOMATION_STATE_DIR)
        .join(sanitize_filename(job_id))
        .join(AUTOMATION_STATE_FILE)
}

fn automation_holder(job_id: &str) -> String {
    format!("automation:{}", sanitize_filename(job_id))
}

fn default_workspace_dir() -> String {
    ".".to_string()
}

fn default_true() -> bool {
    true
}
