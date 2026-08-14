use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub const MAX_PLAN_ENTRIES: usize = 50;
pub const MAX_PLAN_CONTENT_CHARS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanAction {
    Get,
    Update,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryPriority {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl PlanEntryStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanEntry {
    pub content: String,
    pub priority: PlanEntryPriority,
    pub status: PlanEntryStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRequest {
    pub action: PlanAction,
    #[serde(default)]
    pub entries: Vec<PlanEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanResponse {
    pub updated: bool,
    pub cleared: bool,
    pub entries: Vec<PlanEntry>,
}

pub type PlanHandler = Arc<
    dyn Fn(PlanRequest) -> Pin<Box<dyn Future<Output = Result<PlanResponse, String>> + Send>>
        + Send
        + Sync,
>;

pub fn validate_entries(entries: &mut [PlanEntry]) -> Result<(), String> {
    if entries.len() > MAX_PLAN_ENTRIES {
        return Err(format!("plan entries must not exceed {MAX_PLAN_ENTRIES}"));
    }
    let mut in_progress = 0;
    for entry in entries {
        entry.content = entry.content.trim().to_string();
        if entry.content.is_empty() {
            return Err("plan entry content must not be empty".to_string());
        }
        if entry.content.chars().count() > MAX_PLAN_CONTENT_CHARS {
            return Err(format!(
                "plan entry content must not exceed {MAX_PLAN_CONTENT_CHARS} characters"
            ));
        }
        in_progress += usize::from(entry.status == PlanEntryStatus::InProgress);
    }
    if in_progress > 1 {
        return Err("at most one plan entry can be in_progress".to_string());
    }
    Ok(())
}
