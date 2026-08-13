use anyhow::Result;
use dwo_agent_service::{RuntimePhase, SessionStatusSnapshot};
use serde_json::Value;

use crate::automation::{
    AutomationJobStatus, AutomationNewBehavior, AutomationRunStatus, AutomationSession,
};

use super::output;

pub fn write_value(value: &Value) -> Result<()> {
    output::write(format_args!("{}", render_value(value)))?;
    Ok(())
}

pub fn render_value(value: &Value) -> String {
    serde_yaml::to_string(value).unwrap_or_else(|_| "value: <unrenderable>\n".to_string())
}

pub fn write_session_list(value: &Value) -> Result<()> {
    let statuses: Vec<SessionStatusSnapshot> = serde_json::from_value(value.clone())?;
    if statuses.is_empty() {
        output::line(format_args!("No sessions"))?;
        return Ok(());
    }
    let running = statuses
        .iter()
        .filter(|status| status.phase != RuntimePhase::Idle)
        .count();
    output::line(format_args!(
        "Sessions: {}  running={}  idle={}",
        statuses.len(),
        running,
        statuses.len() - running
    ))?;
    output::line(format_args!(""))?;
    output::line(format_args!(
        "STATUS              ID  MODEL  UPDATED  TITLE"
    ))?;
    for status in statuses {
        output::line(format_args!(
            "{:<18}  {}  {}  {}  {}",
            phase_name(status.phase),
            status.record.info.id,
            status.record.llm.model,
            timestamp(status.record.info.updated_at_ms),
            status.record.info.title
        ))?;
    }
    Ok(())
}

pub fn write_session_status(value: &Value) -> Result<()> {
    let status: SessionStatusSnapshot = serde_json::from_value(value.clone())?;
    output::line(format_args!("id: {}", status.record.info.id))?;
    output::line(format_args!("status: {}", phase_name(status.phase)))?;
    output::line(format_args!(
        "title: {}",
        yaml_scalar(&status.record.info.title)
    ))?;
    if let Some(parent) = &status.record.info.parent_session_id {
        output::line(format_args!("parentSessionId: {parent}"))?;
    }
    if status.record.info.ephemeral {
        output::line(format_args!("ephemeral: true"))?;
        output::line(format_args!("completed: {}", status.record.info.completed))?;
        if let Some(delete_after_ms) = status.record.info.delete_after_ms {
            output::line(format_args!("deleteAfter: {}", timestamp(delete_after_ms)))?;
        }
    }
    output::line(format_args!("cwd: {}", status.record.info.cwd.display()))?;
    output::line(format_args!(
        "policy: {}",
        match status.record.info.mode {
            dwo_tools::SessionMode::FullAccess => "full_access",
            dwo_tools::SessionMode::Confirm => "confirm",
            dwo_tools::SessionMode::Watch => "watch",
        }
    ))?;
    output::line(format_args!("model: {}", status.record.llm.model))?;
    if let Some(reasoning) = &status.record.llm.reasoning {
        output::line(format_args!("reasoning: {reasoning}"))?;
    }
    output::line(format_args!(
        "updatedAt: {}",
        timestamp(status.record.info.updated_at_ms)
    ))?;
    output::line(format_args!(
        "usage: {}/{}",
        status.usage.used, status.usage.size
    ))?;
    if let Some(turn) = status.active_turn_id {
        output::line(format_args!("activeTurnId: {turn}"))?;
    }
    if let Some(answer) = status.last_answer {
        output::line(format_args!("lastAnswer: {}", yaml_scalar(&answer)))?;
    }
    Ok(())
}

pub fn write_automation_list(statuses: &[AutomationJobStatus]) -> Result<()> {
    if statuses.is_empty() {
        output::line(format_args!("No automation jobs configured"))?;
        return Ok(());
    }
    let enabled = statuses
        .iter()
        .filter(|status| status.scheduler_enabled && status.job.enabled)
        .count();
    let running = statuses
        .iter()
        .map(|status| status.active_runs.len())
        .sum::<usize>();
    output::line(format_args!(
        "Automations: {}  enabled={}  disabled={}  running={}",
        statuses.len(),
        enabled,
        statuses.len() - enabled,
        running
    ))?;
    output::line(format_args!(""))?;
    output::line(format_args!("ENABLED  NAME  NEXT RUN  ACTIVE"))?;
    for status in statuses {
        output::line(format_args!(
            "{:<7}  {}  {}  {}",
            if status.scheduler_enabled && status.job.enabled {
                "yes"
            } else {
                "no"
            },
            status.job.name,
            status.next_run_at.as_deref().unwrap_or("-"),
            status.active_runs.len()
        ))?;
    }
    Ok(())
}

pub fn write_automation_status(status: &AutomationJobStatus) -> Result<()> {
    output::line(format_args!("name: {}", status.job.name))?;
    output::line(format_args!(
        "enabled: {}",
        status.scheduler_enabled && status.job.enabled
    ))?;
    output::line(format_args!(
        "schedule: {}",
        yaml_scalar(&status.job.schedule.cron)
    ))?;
    output::line(format_args!("timezone: {}", status.job.schedule.timezone))?;
    output::line(format_args!("model: {}", status.effective_model))?;
    output::line(format_args!(
        "modelSource: {}",
        if status.bound_session_id.is_some() {
            "session"
        } else {
            "profile_default"
        }
    ))?;
    output::line(format_args!(
        "nextRunAt: {}",
        status.next_run_at.as_deref().unwrap_or("-")
    ))?;
    match &status.job.session {
        AutomationSession::New {
            behavior,
            cwd,
            title,
        } => {
            output::line(format_args!("sessionMode: new"))?;
            output::line(format_args!(
                "behavior: {}",
                match behavior {
                    AutomationNewBehavior::EveryTime => "every_time",
                    AutomationNewBehavior::Once => "once",
                }
            ))?;
            output::line(format_args!("cwd: {}", cwd.display()))?;
            if let Some(title) = title {
                output::line(format_args!("title: {}", yaml_scalar(title)))?;
            }
        }
        AutomationSession::Fixed { .. } => {
            output::line(format_args!("sessionMode: fixed"))?;
        }
    }
    if let Some(session) = &status.bound_session_id {
        output::line(format_args!("sessionId: {session}"))?;
    }
    output::line(format_args!("activeRuns: {}", status.active_runs.len()))?;
    output::line(format_args!("recentRuns: {}", status.recent_runs.len()))?;
    for run in status.active_runs.iter().chain(status.recent_runs.iter()) {
        output::line(format_args!("  - runId: {}", run.run_id))?;
        output::line(format_args!("    status: {}", run_status_name(run.status)))?;
        if let Some(session) = &run.session_id {
            output::line(format_args!("    sessionId: {session}"))?;
        }
        if let Some(turn) = &run.turn_id {
            output::line(format_args!("    turnId: {turn}"))?;
        }
        if !run.response.is_empty() {
            output::line(format_args!("    answer: {}", yaml_scalar(&run.response)))?;
        }
        if let Some(error) = &run.error {
            output::line(format_args!("    error: {}", yaml_scalar(error)))?;
        }
    }
    Ok(())
}

fn phase_name(phase: RuntimePhase) -> &'static str {
    match phase {
        RuntimePhase::Idle => "idle",
        RuntimePhase::Running => "running",
        RuntimePhase::WaitingPermission => "waiting_permission",
        RuntimePhase::Cancelling => "cancelling",
        RuntimePhase::Closing => "closing",
    }
}

fn run_status_name(status: AutomationRunStatus) -> &'static str {
    match status {
        AutomationRunStatus::Queued => "queued",
        AutomationRunStatus::Running => "running",
        AutomationRunStatus::Completed => "completed",
        AutomationRunStatus::Failed => "failed",
        AutomationRunStatus::Cancelled => "cancelled",
    }
}

fn timestamp(milliseconds: u64) -> String {
    chrono::DateTime::from_timestamp_millis(milliseconds as i64)
        .map(|value| value.to_rfc3339())
        .unwrap_or_else(|| milliseconds.to_string())
}

fn yaml_scalar(value: &str) -> String {
    let rendered = serde_yaml::to_string(value).unwrap_or_else(|_| "''\n".to_string());
    rendered
        .strip_prefix("---\n")
        .unwrap_or(&rendered)
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_scalar_quotes_ambiguous_values() {
        assert_eq!(yaml_scalar("true"), "'true'");
        assert_eq!(yaml_scalar("plain title"), "plain title");
    }
}
