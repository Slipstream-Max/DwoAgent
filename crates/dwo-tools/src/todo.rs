use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub const MAX_TODO_ENTRIES: usize = 50;
pub const MAX_TODO_CONTENT_CHARS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoAction {
    Get,
    Update,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoEntryPriority {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoEntryStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoEntryStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TodoEntry {
    pub content: String,
    pub priority: TodoEntryPriority,
    pub status: TodoEntryStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TodoRequest {
    pub action: TodoAction,
    #[serde(default)]
    pub entries: Vec<TodoEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoResponse {
    pub updated: bool,
    pub cleared: bool,
    pub entries: Vec<TodoEntry>,
}

pub type TodoHandler = Arc<
    dyn Fn(TodoRequest) -> Pin<Box<dyn Future<Output = Result<TodoResponse, String>> + Send>>
        + Send
        + Sync,
>;

pub fn validate_entries(entries: &mut [TodoEntry]) -> Result<(), String> {
    if entries.len() > MAX_TODO_ENTRIES {
        return Err(format!("todo entries must not exceed {MAX_TODO_ENTRIES}"));
    }
    let mut in_progress = 0;
    for entry in entries {
        entry.content = entry.content.trim().to_string();
        if entry.content.is_empty() {
            return Err("todo entry content must not be empty".to_string());
        }
        if entry.content.chars().count() > MAX_TODO_CONTENT_CHARS {
            return Err(format!(
                "todo entry content must not exceed {MAX_TODO_CONTENT_CHARS} characters"
            ));
        }
        in_progress += usize::from(entry.status == TodoEntryStatus::InProgress);
    }
    if in_progress > 1 {
        return Err("at most one todo entry can be in_progress".to_string());
    }
    Ok(())
}
