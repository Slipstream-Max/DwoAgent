use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::call::ParsedToolCall;
use crate::file_edit::FileEditManager;
use crate::policy::{SessionMode, ToolPolicyEngine};
use crate::result::ToolResult;
use crate::terminal::TerminalManager;

pub type ConfirmationHandler = Arc<
    dyn Fn(ConfirmationRequest) -> Pin<Box<dyn Future<Output = ConfirmationDecision> + Send>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone)]
pub struct ConfirmationRequest {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone)]
pub struct ConfirmationDecision {
    pub allowed: bool,
    pub reason: Option<String>,
}

/// Per-batch values captured by the session before dispatch.
#[derive(Clone)]
pub struct ExecutionContext {
    pub mode: SessionMode,
    pub confirmation: Option<ConfirmationHandler>,
}

impl ExecutionContext {
    pub fn new(mode: SessionMode) -> Self {
        Self {
            mode,
            confirmation: None,
        }
    }
}

/// The only public execution facade owned by a loaded session.
pub struct ToolManager {
    pub(crate) cwd: PathBuf,
    pub(crate) policy: Arc<ToolPolicyEngine>,
    pub(crate) terminals: Arc<TerminalManager>,
    pub(crate) file_edit: Arc<FileEditManager>,
}

impl ToolManager {
    pub fn new(
        cwd: PathBuf,
        policy: Arc<ToolPolicyEngine>,
        file_edit: Arc<FileEditManager>,
    ) -> Result<Self> {
        Self::new_with_environment(cwd, policy, file_edit, [])
    }

    pub fn new_with_environment(
        cwd: PathBuf,
        policy: Arc<ToolPolicyEngine>,
        file_edit: Arc<FileEditManager>,
        environment: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self> {
        let terminals = Arc::new(TerminalManager::new_with_environment(
            &cwd,
            environment.into_iter().collect::<HashMap<_, _>>(),
        )?);
        Ok(Self {
            cwd,
            policy,
            terminals,
            file_edit,
        })
    }

    pub async fn execute(&self, call: ParsedToolCall, context: &ExecutionContext) -> ToolResult {
        crate::exec::execute(self, call, context).await
    }

    pub async fn execute_batch(
        &self,
        raw_calls: Vec<Value>,
        context: &ExecutionContext,
    ) -> Vec<ToolResult> {
        crate::exec::execute_batch(self, raw_calls, context).await
    }

    pub async fn shutdown(&self) {
        self.terminals.shutdown_all().await;
    }

    pub fn schemas(&self) -> Vec<Value> {
        crate::schema::tool_schemas()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::PolicyConfig;

    fn manager(cwd: PathBuf) -> ToolManager {
        ToolManager::new(
            cwd,
            Arc::new(ToolPolicyEngine::new(PolicyConfig::default())),
            Arc::new(FileEditManager::new()),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn batch_preserves_order_and_contains_per_call_errors() {
        let dir = tempfile::tempdir().unwrap();
        let outputs = manager(dir.path().to_path_buf())
            .execute_batch(
                vec![
                    json!({"id":"a", "name":"unknown", "arguments":{}}),
                    json!({"id":"b", "name":"file_edit", "arguments":{"patch":"*** Begin Patch\n*** Add File: x\n+x\n*** End Patch"}}),
                    json!({"id":"c", "name":"terminal", "arguments":{}}),
                ],
                &ExecutionContext::new(SessionMode::FullAccess),
            )
            .await;
        assert_eq!(outputs.len(), 3);
        assert_eq!(outputs[0].output["status"], "error");
        assert_eq!(outputs[1].output["status"], "completed");
        assert_eq!(outputs[2].output["status"], "error");
    }

    #[tokio::test]
    async fn confirmation_happens_at_the_single_policy_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let mut context = ExecutionContext::new(SessionMode::Confirm);
        context.confirmation = Some(Arc::new({
            let count = count.clone();
            move |_| {
                let count = count.clone();
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    ConfirmationDecision {
                        allowed: false,
                        reason: Some("no".to_string()),
                    }
                })
            }
        }));
        let call = ParsedToolCall::parse(json!({
            "id":"a", "name":"file_edit", "arguments":{"patch":"*** Begin Patch\n*** Add File: x\n+x\n*** End Patch"}
        }))
        .unwrap();
        let output = manager(dir.path().to_path_buf())
            .execute(call, &context)
            .await;
        assert_eq!(output.output["status"], "blocked_by_policy");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(!dir.path().join("x").exists());
    }

    #[tokio::test]
    async fn multiple_file_edits_in_one_batch_are_all_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let terminal_command = if cfg!(windows) {
            "Write-Output terminal-ok"
        } else {
            "printf terminal-ok"
        };
        let outputs = manager(dir.path().to_path_buf())
            .execute_batch(
                vec![
                    json!({"id":"a", "name":"file_edit", "arguments":{"patch":"*** Begin Patch\n*** Add File: x\n+one\n*** End Patch"}}),
                    json!({"id":"b", "name":"file_edit", "arguments":{"patch":"*** Begin Patch\n*** Update File: x\n@@\n-one\n+two\n*** End Patch"}}),
                    json!({"id":"terminal", "name":"terminal", "arguments":{"action":"run", "command":terminal_command}}),
                ],
                &ExecutionContext::new(SessionMode::FullAccess),
            )
            .await;
        assert!(outputs[..2].iter().all(|output| {
            output.output["status"] == "error"
                && output.output["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("Only one file_edit"))
        }));
        assert_eq!(outputs[2].output["status"], "completed");
        assert!(
            outputs[2].output["output"]
                .as_str()
                .is_some_and(|output| output.contains("terminal-ok"))
        );
        assert!(!dir.path().join("x").exists());
    }

    #[tokio::test]
    async fn one_file_edit_call_can_change_multiple_files() {
        let dir = tempfile::tempdir().unwrap();
        let outputs = manager(dir.path().to_path_buf())
            .execute_batch(
                vec![json!({
                    "id":"files",
                    "name":"file_edit",
                    "arguments":{
                        "patch":"*** Begin Patch\n*** Add File: x\n+x\n*** Add File: y\n+y\n*** End Patch"
                    }
                })],
                &ExecutionContext::new(SessionMode::FullAccess),
            )
            .await;
        assert_eq!(outputs[0].output["status"], "completed");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("x")).unwrap(),
            "x\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("y")).unwrap(),
            "y\n"
        );
    }

    #[tokio::test]
    async fn file_worker_runs_while_terminal_is_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(manager(dir.path().to_path_buf()));
        let terminal_command = if cfg!(windows) {
            "Start-Sleep -Seconds 2"
        } else {
            "sleep 2"
        };
        let task = tokio::spawn({
            let manager = manager.clone();
            let context = ExecutionContext::new(SessionMode::FullAccess);
            async move {
                manager
                    .execute_batch(
                        vec![
                            json!({"id":"terminal", "name":"terminal", "arguments":{"action":"run", "command":terminal_command, "yield_ms":5000}}),
                            json!({"id":"file", "name":"file_edit", "arguments":{"patch":"*** Begin Patch\n*** Add File: concurrent.txt\n+done\n*** End Patch"}}),
                        ],
                        &context,
                    )
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(dir.path().join("concurrent.txt").is_file());
        assert_eq!(task.await.unwrap().len(), 2);
    }
}
