use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use super::builtin::subagent::{SpawnSubagentPayload, SubagentExecutor, ToolExecutionContext};
use super::builtin::terminal::{TerminalExecutor, TerminalSession};
use super::session::ToolSession;

pub(crate) enum SessionCreateRequest {
    Terminal(TerminalCreateRequest),
    Subagent(SpawnSubagentPayload),
}

impl SessionCreateRequest {
    pub(crate) fn parse(
        name: &str,
        args: &Map<String, Value>,
        context: Option<&ToolExecutionContext>,
    ) -> Result<Self> {
        match name {
            "terminal_exec" => Ok(Self::Terminal(TerminalCreateRequest::from_args(args))),
            "spawn_subagent" => {
                context.ok_or_else(|| {
                    anyhow::anyhow!("spawn_subagent requires tool execution context.")
                })?;
                Ok(Self::Subagent(SpawnSubagentPayload::from_json(
                    Value::Object(args.clone()),
                )?))
            }
            other => anyhow::bail!("Unknown tool: {other}"),
        }
    }

    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Terminal(_) => "terminal",
            Self::Subagent(_) => "subagent",
        }
    }

    pub(crate) fn requested_name(&self) -> Option<&str> {
        match self {
            Self::Terminal(request) => request.terminal_name.as_deref(),
            Self::Subagent(payload) => payload.subagent_name.as_deref(),
        }
    }

    pub(crate) async fn create_session(
        self,
        tool_call_id: &str,
        session_name: String,
        terminal_executor: &TerminalExecutor,
        subagent_executor: Option<Arc<dyn SubagentExecutor>>,
        context: Option<&ToolExecutionContext>,
    ) -> Result<Arc<Mutex<dyn ToolSession>>> {
        match self {
            Self::Terminal(request) => Ok(Arc::new(Mutex::new(TerminalSession::new(
                tool_call_id.to_string(),
                session_name,
                terminal_executor.clone(),
                request.command,
                request.env,
                request.timeout,
            )))),
            Self::Subagent(payload) => {
                let context = context.ok_or_else(|| {
                    anyhow::anyhow!("spawn_subagent requires tool execution context.")
                })?;
                let subagent = subagent_executor
                    .ok_or_else(|| anyhow::anyhow!("spawn_subagent has no executor attached."))?;
                subagent
                    .create_session(
                        tool_call_id,
                        &session_name,
                        &payload.task,
                        payload.policy.as_deref(),
                        context,
                    )
                    .await
            }
        }
    }
}

pub(crate) struct TerminalCreateRequest {
    terminal_name: Option<String>,
    command: String,
    env: Option<HashMap<String, String>>,
    timeout: f64,
}

impl TerminalCreateRequest {
    fn from_args(args: &Map<String, Value>) -> Self {
        Self {
            terminal_name: args
                .get("terminal_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            command: args
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            env: args.get("env").and_then(Value::as_object).map(|map| {
                map.iter()
                    .map(|(key, value)| (key.clone(), value.as_str().unwrap_or("").to_string()))
                    .collect()
            }),
            timeout: args.get("timeout").and_then(Value::as_f64).unwrap_or(30.0),
        }
    }
}
