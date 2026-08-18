use std::path::Path;

use dwo_agent_service::{SessionId, SessionSnapshot};
use dwo_tools::SessionMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionIdDisplay {
    Full,
    Short,
}

pub fn render_status(snapshot: &SessionSnapshot, id_display: SessionIdDisplay) -> String {
    let id = match id_display {
        SessionIdDisplay::Full => snapshot.record.info.id.as_str().to_string(),
        SessionIdDisplay::Short => short_session_id(&snapshot.record.info.id),
    };
    let mut lines = vec![format!(
        "Session: {}\nID: {}\nCwd: {}\nPolicy: {}\nModel: {}\nReasoning: {}\nState: {:?}",
        snapshot.record.info.title,
        id,
        display_path(&snapshot.record.info.cwd),
        policy_name(snapshot.record.info.mode),
        snapshot.record.llm.model,
        snapshot
            .record
            .llm
            .reasoning
            .as_deref()
            .unwrap_or("default"),
        snapshot.phase,
    )];
    if !snapshot.partial_message.is_empty() {
        lines.push(format!("Current: {}", snapshot.partial_message));
    }
    if let Some(permission) = &snapshot.pending_permission {
        lines.push(format!("Pending permission: {}", permission.request_id));
    }
    lines.join("\n")
}

pub fn policy_name(mode: SessionMode) -> &'static str {
    match mode {
        SessionMode::FullAccess => "full_access",
        SessionMode::Confirm => "confirm",
        SessionMode::Watch => "watch",
    }
}

pub fn display_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if let Some(path) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else if let Some(path) = raw.strip_prefix(r"\\?\") {
        path.to_string()
    } else {
        raw.into_owned()
    }
}

pub fn short_session_id(id: &SessionId) -> String {
    short_session_id_str(id.as_str())
}

pub fn short_session_id_str(id: &str) -> String {
    id.strip_prefix("session-")
        .unwrap_or(id)
        .chars()
        .take(8)
        .collect()
}
