//! Automation/job ingress runtime.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::Timelike;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::config::{
    AutomationJobConfig, AutomationNotifyChannel, AutomationNotifyConfig, AutomationSchedule,
};
use super::response::ChannelUpdateCollector;
use crate::agent::constants::PERMISSION_REJECT_ONCE;
use crate::agent::service::AgentService;
use crate::config::loader::utc_iso;
use crate::context::content_block;
use crate::tools::subagent_tool_runtime::PermissionRequester;

const AUTOMATION_SESSION_SUBDIR: &str = "automation";
const RUN_FILE: &str = "run.yaml";

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

pub struct AutomationRuntime {
    agent: Arc<AgentService>,
    agent_structure_dir: PathBuf,
    jobs: Vec<AutomationJobConfig>,
    sinks: AutomationNotificationSinks,
}

impl AutomationRuntime {
    pub fn new(
        agent: Arc<AgentService>,
        agent_structure_dir: &Path,
        jobs: Vec<AutomationJobConfig>,
        sinks: AutomationNotificationSinks,
    ) -> Self {
        Self {
            agent,
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
        let session = self
            .agent
            .new_session(&workspace_dir.to_string_lossy())
            .await?;
        let run_id = new_run_id();
        let run_dir = self
            .agent
            .channel_session_dir()
            .join(AUTOMATION_SESSION_SUBDIR)
            .join(sanitize_filename(&job.id))
            .join("runs")
            .join(&run_id);
        let run_path = run_dir.join(RUN_FILE);
        let started_at = utc_iso();
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
        write_run_record(&run_path, &record)?;

        let blocks = vec![content_block::text(&job.prompt)?];
        let user_input = Value::String(job.prompt.clone());
        let collector = ChannelUpdateCollector::new(job.response_detail);
        let run_result = session
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
        write_run_record(&run_path, &record)?;
        Ok(record)
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

fn render_notification_text(job: &AutomationJobConfig, record: &AutomationRunRecord) -> String {
    let status = match record.status {
        AutomationRunStatus::Running => "running",
        AutomationRunStatus::Completed => "completed",
        AutomationRunStatus::Failed => "failed",
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
}
