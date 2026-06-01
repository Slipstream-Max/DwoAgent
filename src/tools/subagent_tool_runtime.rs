//! Payload models for subagent tools.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::context::manager::CancelEvent;

use crate::config::models::AgentState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnSubagentPayload {
    pub task: String,
    #[serde(default)]
    pub policy: Option<String>,
}

impl SpawnSubagentPayload {
    pub fn from_json(value: Value) -> Result<Self> {
        let mut payload: Self = serde_json::from_value(value)?;
        payload.task = payload.task.trim().to_string();
        if payload.task.is_empty() {
            bail!("subagent task cannot be empty");
        }
        if let Some(p) = payload.policy.as_ref() {
            let trimmed = p.trim();
            payload.policy = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
        Ok(payload)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentIdPayload {
    pub subagent_run_id: String,
}

impl SubagentIdPayload {
    pub fn from_json(value: Value) -> Result<Self> {
        let mut payload: Self = serde_json::from_value(value)?;
        payload.subagent_run_id = payload.subagent_run_id.trim().to_string();
        if payload.subagent_run_id.is_empty() {
            bail!("subagent_run_id cannot be empty");
        }
        Ok(payload)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitSubagentPayload {
    pub subagent_run_id: String,
    #[serde(default = "default_wait_timeout")]
    pub timeout: f64,
}

fn default_wait_timeout() -> f64 {
    30.0
}

impl WaitSubagentPayload {
    pub fn from_json(value: Value) -> Result<Self> {
        let mut payload: Self = serde_json::from_value(value)?;
        payload.subagent_run_id = payload.subagent_run_id.trim().to_string();
        if payload.subagent_run_id.is_empty() {
            bail!("subagent_run_id cannot be empty");
        }
        if !(payload.timeout > 0.0) {
            bail!("timeout must be > 0");
        }
        Ok(payload)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendSubagentPayload {
    pub subagent_run_id: String,
    pub message: String,
    #[serde(default)]
    pub interrupt: bool,
}

impl SendSubagentPayload {
    pub fn from_json(value: Value) -> Result<Self> {
        let mut payload: Self = serde_json::from_value(value)?;
        payload.subagent_run_id = payload.subagent_run_id.trim().to_string();
        payload.message = payload.message.trim().to_string();
        if payload.subagent_run_id.is_empty() {
            bail!("subagent_run_id cannot be empty");
        }
        if payload.message.is_empty() {
            bail!("message cannot be empty");
        }
        Ok(payload)
    }
}

pub fn subagent_not_found(subagent_run_id: &str) -> Value {
    json!({
        "subagent_run_id": subagent_run_id,
        "runtime": {
            "kind": "subagent",
            "id": subagent_run_id,
            "status": "not_found",
        },
        "status": "not_found",
        "done": true,
        "error": "subagent not found",
    })
}

/// Async callback used by the tool runtime to stream session updates back to
/// the host. Matches Python's `UpdateEmitter = Callable[[str, dict], Awaitable[None]]`.
pub type UpdateEmitter = Arc<
    dyn Fn(String, Map<String, Value>) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>
        + Send
        + Sync,
>;

pub type PermissionRequester = Arc<
    dyn Fn(String, Map<String, Value>) -> Pin<Box<dyn Future<Output = Result<String>> + Send>>
        + Send
        + Sync,
>;

pub type StateSetter = Arc<dyn Fn(AgentState) + Send + Sync>;

/// Bundle of runtime hooks handed down to sessions that need to stream
/// updates or request user approval. Mirror of Python's
/// `ToolExecutionContext` dataclass.
#[derive(Clone)]
pub struct ToolExecutionContext {
    pub session_id: String,
    pub tool_call_id: String,
    pub mode_id: String,
    pub cancel_event: CancelEvent,
    pub emit_update: UpdateEmitter,
    pub request_permission: PermissionRequester,
    pub set_state: StateSetter,
}
