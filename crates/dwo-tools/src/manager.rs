use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::ToolEventHandler;
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
    pub allow_image_input: bool,
    pub events: Option<ToolEventHandler>,
}

impl ExecutionContext {
    pub fn new(mode: SessionMode) -> Self {
        Self {
            mode,
            confirmation: None,
            allow_image_input: false,
            events: None,
        }
    }
}

/// The only public execution facade owned by a loaded session.
pub struct ToolManager {
    pub(crate) cwd: PathBuf,
    pub(crate) policy: Arc<ToolPolicyEngine>,
    pub(crate) terminals: Arc<TerminalManager>,
    pub(crate) file_edit: Arc<FileEditManager>,
    schemas: Vec<Value>,
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
            schemas: crate::schema::tool_schemas(),
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

    pub fn schemas(&self) -> &[Value] {
        &self.schemas
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::{PolicyConfig, ToolEvent};

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
                    json!({"id":"terminal", "name":"terminal", "arguments":{"command":terminal_command}}),
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
    async fn file_edit_telemetry_contains_absolute_changes_and_git_patch() {
        let dir = tempfile::tempdir().unwrap();
        let recorded = Arc::new(StdMutex::new(Vec::new()));
        let mut context = ExecutionContext::new(SessionMode::FullAccess);
        context.events = Some(Arc::new({
            let recorded = recorded.clone();
            move |event| recorded.lock().unwrap().push(event)
        }));
        let call = ParsedToolCall::parse(json!({
            "id": "edit-1",
            "name": "file_edit",
            "arguments": {
                "patch": "*** Begin Patch\n*** Add File: telemetry.txt\n+observed\n*** End Patch"
            }
        }))
        .unwrap();

        let result = manager(dir.path().to_path_buf())
            .execute(call, &context)
            .await;
        assert_eq!(result.output["status"], "completed");

        let events = recorded.lock().unwrap();
        let [
            ToolEvent::FileChanged {
                tool_call_id,
                changes,
                patch,
            },
        ] = events.as_slice()
        else {
            panic!("unexpected telemetry: {events:?}");
        };
        assert_eq!(tool_call_id, "edit-1");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, "add");
        assert!(changes[0].path.is_absolute());
        assert!(patch.starts_with("diff --git "));
        assert!(patch.contains("+observed"));
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
                            json!({"id":"terminal", "name":"terminal", "arguments":{"command":terminal_command, "yield_ms":5000}}),
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

    #[tokio::test]
    async fn read_file_only_adds_images_for_capable_models() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("image.bin"), b"\x89PNG\r\n\x1a\nimage").unwrap();
        let call = || {
            ParsedToolCall::parse(json!({
                "id":"image",
                "name":"read_file",
                "arguments":{"path":"image.bin"}
            }))
            .unwrap()
        };
        let manager = manager(dir.path().to_path_buf());

        let denied = manager
            .execute(call(), &ExecutionContext::new(SessionMode::FullAccess))
            .await;
        assert_eq!(denied.output["status"], "error");
        assert!(denied.model_context.is_empty());

        let mut capable = ExecutionContext::new(SessionMode::FullAccess);
        capable.allow_image_input = true;
        let allowed = manager.execute(call(), &capable).await;
        assert_eq!(allowed.output, json!({"status":"completed"}));
        assert_eq!(allowed.model_context.len(), 1);
        assert!(allowed.model_context[0].contains_images());
        assert_eq!(
            serde_json::to_value(&allowed).unwrap(),
            json!({
                "tool_call_id":"image",
                "tool_name":"read_file",
                "output":{"status":"completed"}
            })
        );
    }
}
