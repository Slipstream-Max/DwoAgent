//! Code executor and session implementation.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use uuid::Uuid;

use super::monty_backend::MontyBackend;
use crate::tools::session::{Cap, ToolArgs, ToolSession};

/// Execute Monty code with MCP access bridged through external functions.
///
/// The executor holds a `MontyBackend` handle that talks to a dedicated
/// worker thread where both the MCP client and the Monty interpreter live.
/// Every call is driven through `mpsc` / `oneshot`, so the executor itself
/// stays `Send + Sync` and cooperates with the rest of the tool manager.
pub struct CodeExecutor {
    backend: MontyBackend,
}

impl CodeExecutor {
    /// Build an executor from an optional MCP config file on disk. Mirror of
    /// Python's `CodeExecutor(config=...)` where config is pulled from
    /// `resources/mcp.json`.
    pub async fn from_mcp_config(path: Option<&Path>) -> Result<Self> {
        let backend = MontyBackend::spawn(path).await?;
        Ok(Self { backend })
    }

    pub fn server_names(&self) -> &[String] {
        self.backend.server_names()
    }

    pub async fn shutdown(&self) {
        self.backend.shutdown().await;
    }

    pub async fn execute(
        &self,
        code: &str,
        timeout_secs: f64,
        output_limit: usize,
        run_id: Option<&str>,
    ) -> Value {
        let codemode_run_id = run_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| Uuid::new_v4().simple().to_string());

        match self.execute_inner(code, timeout_secs, output_limit).await {
            Ok(output) => json!({
                "toolcall_id": codemode_run_id,
                "runtime": {
                    "kind": "codemode",
                    "id": codemode_run_id,
                    "status": "completed_success",
                },
                "status": "completed_success",
                "done": true,
                "result": output,
            }),
            Err(err) => json!({
                "toolcall_id": codemode_run_id,
                "runtime": {
                    "kind": "codemode",
                    "id": codemode_run_id,
                    "status": "completed_error",
                },
                "status": "completed_error",
                "done": true,
                "error": format_error(&err),
            }),
        }
    }

    async fn execute_inner(
        &self,
        code: &str,
        timeout_secs: f64,
        output_limit: usize,
    ) -> Result<Value> {
        if code.trim().is_empty() {
            bail!("code must be non-empty");
        }
        if !(timeout_secs > 0.0) {
            bail!("timeout must be > 0");
        }
        if output_limit == 0 {
            bail!("outputlimit must be >= 1");
        }
        self.backend.execute(code, timeout_secs, output_limit).await
    }
}

fn format_error(err: &anyhow::Error) -> String {
    let text = format!("{err:#}");
    if text.is_empty() {
        "error".to_string()
    } else {
        text
    }
}

/// One exec_chain run — wraps a single call to [`CodeExecutor::execute`].
pub struct CodeExecSession {
    session_id: String,
    executor: Arc<CodeExecutor>,
    code: String,
    timeout: f64,
    output_limit: usize,
    done: bool,
    result: Option<Value>,
}

impl CodeExecSession {
    pub fn new(
        session_id: impl Into<String>,
        executor: Arc<CodeExecutor>,
        code: impl Into<String>,
        timeout_secs: f64,
        output_limit: usize,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            executor,
            code: code.into(),
            timeout: timeout_secs,
            output_limit,
            done: false,
            result: None,
        }
    }
}

#[async_trait]
impl ToolSession for CodeExecSession {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn capabilities(&self) -> HashSet<Cap> {
        HashSet::new()
    }

    async fn start(&mut self, _args: &ToolArgs) -> Result<Value> {
        let result = self
            .executor
            .execute(
                &self.code,
                self.timeout,
                self.output_limit,
                Some(&self.session_id),
            )
            .await;
        self.result = Some(result.clone());
        self.done = true;
        Ok(result)
    }

    async fn cancel(&mut self) -> Result<()> {
        self.done = true;
        Ok(())
    }

    fn is_done(&self) -> bool {
        self.done
    }

    fn list_item(&self) -> Value {
        json!({
            "id": self.session_id,
            "kind": "codemode",
            "status": if self.done { "completed_success" } else { "running" },
            "done": self.done,
        })
    }
}
