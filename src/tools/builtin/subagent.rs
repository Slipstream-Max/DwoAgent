//! Payload models for subagent tools.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use crate::context::manager::CancelEvent;

use crate::config::models::AgentState;

use crate::tools::session::ToolSession;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnSubagentPayload {
    #[serde(default)]
    pub subagent_name: Option<String>,
    pub task: String,
    #[serde(default)]
    pub policy: Option<String>,
}

impl SpawnSubagentPayload {
    pub fn from_json(value: Value) -> Result<Self> {
        let mut payload: Self = serde_json::from_value(value)?;
        if let Some(name) = payload.subagent_name.as_ref() {
            let trimmed = name.trim();
            payload.subagent_name = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
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
    pub subagent_name: String,
}

impl SubagentIdPayload {
    pub fn from_json(value: Value) -> Result<Self> {
        let mut payload: Self = serde_json::from_value(value)?;
        payload.subagent_name = payload.subagent_name.trim().to_string();
        if payload.subagent_name.is_empty() {
            bail!("subagent_name cannot be empty");
        }
        Ok(payload)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendSubagentPayload {
    pub subagent_name: String,
    pub message: String,
    #[serde(default)]
    pub interrupt: bool,
}

impl SendSubagentPayload {
    pub fn from_json(value: Value) -> Result<Self> {
        let mut payload: Self = serde_json::from_value(value)?;
        payload.subagent_name = payload.subagent_name.trim().to_string();
        payload.message = payload.message.trim().to_string();
        if payload.subagent_name.is_empty() {
            bail!("subagent_name cannot be empty");
        }
        if payload.message.is_empty() {
            bail!("message cannot be empty");
        }
        Ok(payload)
    }
}

pub fn subagent_not_found(tool: &str, subagent_name: &str) -> Value {
    json!({
        "tool": tool,
        "kind": "subagent",
        "name": subagent_name,
        "status": "error",
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

/// Builder hook for spawning subagent sessions. The concrete impl lives in
/// `agent::subagent`; the tool runtime only needs this trait surface.
#[async_trait::async_trait]
pub trait SubagentExecutor: Send + Sync {
    async fn create_session(
        &self,
        tool_call_id: &str,
        session_name: &str,
        task: &str,
        policy: Option<&str>,
        context: &ToolExecutionContext,
    ) -> Result<Arc<Mutex<dyn ToolSession>>>;
}
