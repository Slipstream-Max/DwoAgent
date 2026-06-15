//! Unified tool dispatcher.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use super::builtin::file::execute_file_tool;
use super::builtin::subagent::{SubagentExecutor, ToolExecutionContext};
use super::session_manager::SessionManager;
pub(crate) use super::tool_catalog::tool_kind;
use super::tool_catalog::{ToolRoute, is_file_write_tool, lookup_tool};
use super::tool_output::ToolOutput;
use crate::agent::activity::event::{ActivityEvent, ToolCallUpdateEvent};
use crate::config::models::AgentTools;
use crate::utils::perf::perf_log;

#[async_trait::async_trait]
pub trait ChannelToolExecutor: Send + Sync {
    fn handles_tool(&self, name: &str) -> bool;

    async fn execute_channel_tool(
        &self,
        name: &str,
        args: &Map<String, Value>,
        context: Option<&ToolExecutionContext>,
    ) -> Result<Value>;
}

/// Dispatches tool calls to concrete runtimes and runs batches in parallel.
pub struct ToolRunManager {
    cwd: PathBuf,
    runtime_tools: AgentTools,
    session_manager: SessionManager,
    channel_tool_executor: Mutex<Option<Arc<dyn ChannelToolExecutor>>>,
}

impl ToolRunManager {
    pub async fn new(
        cwd: Option<&Path>,
        finished_ttl_seconds: u64,
        runtime_tools: AgentTools,
    ) -> Result<Self> {
        let runtime_cwd = match cwd {
            Some(p) => std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()),
            None => std::env::current_dir().context("resolve current dir")?,
        };
        let session_manager = SessionManager::new(runtime_cwd.clone(), finished_ttl_seconds);
        Ok(Self {
            cwd: runtime_cwd,
            runtime_tools,
            session_manager,
            channel_tool_executor: Mutex::new(None),
        })
    }

    pub async fn set_subagent_executor(&self, executor: Option<Arc<dyn SubagentExecutor>>) {
        self.session_manager.set_subagent_executor(executor).await;
    }

    pub async fn set_channel_tool_executor(&self, executor: Option<Arc<dyn ChannelToolExecutor>>) {
        let mut guard = self.channel_tool_executor.lock().await;
        *guard = executor;
    }

    pub async fn ashutdown(&self) {
        self.session_manager.shutdown().await;
    }

    pub async fn cancel_running_tools(&self) {
        self.session_manager.cancel_running_tools().await;
    }

    pub async fn cancel_tool_call(&self, tool_call_id: &str) -> bool {
        self.session_manager.cancel_tool_call(tool_call_id).await
    }

    /// Run one ACP/model tool call through the tool dispatcher.
    pub async fn execute_tool_call(
        &self,
        tool_call_id: &str,
        name: &str,
        arguments: Option<&Value>,
        context: Option<&ToolExecutionContext>,
    ) -> Value {
        let normalized_id = tool_call_id.trim();
        let tool_name = name.trim();
        if normalized_id.is_empty() {
            return ToolOutput::error(tool_name, "tool_call_id is required.");
        }
        if self.session_manager.is_closing().await {
            return ToolOutput::error(tool_name, "Tool manager is shutting down.");
        }
        self.session_manager.prune_finished_sessions().await;

        let tool_args = match normalize_arguments(arguments) {
            Ok(args) => args,
            Err(err) => return ToolOutput::error(tool_name, err.to_string()),
        };

        let channel_executor = {
            let guard = self.channel_tool_executor.lock().await;
            guard
                .as_ref()
                .filter(|exec| exec.handles_tool(tool_name))
                .cloned()
        };

        let spec = lookup_tool(tool_name);
        if channel_executor.is_none() && spec.is_none() {
            return ToolOutput::error(tool_name, format!("Unknown tool: {tool_name}"));
        }
        if let Some(executor) = channel_executor {
            return match executor
                .execute_channel_tool(tool_name, &tool_args, context)
                .await
            {
                Ok(value) => value,
                Err(err) => ToolOutput::error(tool_name, format!("{err:#}")),
            };
        }
        let Some(spec) = spec else {
            return ToolOutput::error(tool_name, format!("Unknown tool: {tool_name}"));
        };
        if !spec.is_enabled(&self.runtime_tools) {
            return ToolOutput::error(tool_name, format!("Tool is disabled: {tool_name}"));
        }

        match spec.route {
            ToolRoute::ListSessions => self.session_manager.list(spec).await,
            ToolRoute::Wait => self.session_manager.wait(&tool_args).await,
            ToolRoute::Immediate => match execute_file_tool(spec.name, &tool_args, &self.cwd) {
                Ok(value) => value,
                Err(err) => ToolOutput::error(tool_name, format!("{err:#}")),
            },
            ToolRoute::CreateSession => {
                self.session_manager
                    .create_and_register(normalized_id, tool_name, &tool_args, context)
                    .await
            }
            ToolRoute::OperateSession => self.session_manager.operate(spec, &tool_args).await,
        }
    }

    /// Run multiple managed tool calls concurrently and preserve order.
    pub async fn execute_tool_calls(
        &self,
        tool_calls: Vec<Value>,
        context: Option<&ToolExecutionContext>,
    ) -> Vec<Value> {
        let started = Instant::now();
        let names: Vec<String> = tool_calls
            .iter()
            .map(|c| {
                c.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
        perf_log(
            "tool_batch_start",
            &json!({
                "count": tool_calls.len(),
                "names": names,
            }),
        );

        let total = tool_calls.len();
        let mut indexed_outputs: Vec<Option<Value>> = vec![None; total];
        let has_multiple_file_writes = tool_calls
            .iter()
            .filter(|call| {
                call.get("name")
                    .and_then(Value::as_str)
                    .map(is_file_write_tool)
                    .unwrap_or(false)
            })
            .count()
            > 1;

        // Save per-call metadata so the cancel path can emit updates.
        let mut call_metadata: Vec<(String, String, Option<Value>)> = Vec::with_capacity(total);

        // Emit the `in_progress` update up front (Python emits this before
        // the batch's `asyncio.wait` loop begins) so UIs see every call enter
        // the running state at the same moment.
        let mut pending = FuturesUnordered::new();
        for (index, call) in tool_calls.into_iter().enumerate() {
            let tool_call_id = call
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let tool_name = call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let tool_args = call.get("arguments").cloned();
            call_metadata.push((tool_call_id.clone(), tool_name.clone(), tool_args.clone()));
            self.emit_tool_update(
                context,
                &tool_call_id,
                "in_progress",
                Some(&tool_name),
                tool_args.clone(),
                None,
            )
            .await;

            if has_multiple_file_writes && is_file_write_tool(&tool_name) {
                let output = ToolOutput::error(
                    &tool_name,
                    "Multiple file write tool calls in one assistant turn are not allowed. Combine the changes into one file_edit patch or one text_replace call.",
                );
                self.emit_tool_update(
                    context,
                    &tool_call_id,
                    "failed",
                    None,
                    tool_args.clone(),
                    Some(output.clone()),
                )
                .await;
                indexed_outputs[index] = Some(output);
                continue;
            }

            let call_context = context.map(|parent| ToolExecutionContext {
                session_id: parent.session_id.clone(),
                tool_call_id: tool_call_id.clone(),
                mode_id: parent.mode_id.clone(),
                cancel_event: parent.cancel_event.clone(),
                emit_update: parent.emit_update.clone(),
                request_permission: parent.request_permission.clone(),
                set_state: parent.set_state.clone(),
            });

            let tool_call_id_for_future = tool_call_id.clone();
            let tool_name_for_future = tool_name.clone();
            let tool_args_for_future = tool_args.clone();
            pending.push(async move {
                let output = self
                    .execute_tool_call(
                        &tool_call_id_for_future,
                        &tool_name_for_future,
                        tool_args_for_future.as_ref(),
                        call_context.as_ref(),
                    )
                    .await;
                (index, tool_call_id, tool_name, tool_args, output)
            });
        }

        let mut cancelled_batch = false;
        while !pending.is_empty() {
            // Check cancellation between slices, mirroring Python's 0.1s
            // `asyncio.wait(..., timeout=0.1)` polling cadence.
            if let Some(ctx) = context
                && ctx.cancel_event.is_set()
            {
                cancelled_batch = true;
                break;
            }

            tokio::select! {
                biased;

                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    // Loop back and re-check the cancel flag.
                }
                Some((index, tool_call_id, _tool_name, tool_args, output)) = pending.next() => {
                    let status = tool_status_to_update_status(&output);
                    self.emit_tool_update(
                        context,
                        &tool_call_id,
                        status,
                        None,
                        tool_args,
                        Some(output.clone()),
                    )
                    .await;
                    indexed_outputs[index] = Some(output);
                }
            }
        }

        if cancelled_batch {
            self.cancel_running_tools().await;
            drop(pending);
            for (index, slot) in indexed_outputs.iter_mut().enumerate() {
                if slot.is_some() {
                    continue;
                }
                let output = ToolOutput::cancelled(
                    &call_metadata[index].1,
                    "Tool call cancelled because user interrupt.",
                );
                *slot = Some(output.clone());
                // Emit a "failed" tool_call_update so the client UI sees the
                // transition from in_progress → failed for each cancelled slot.
                let (ref tool_call_id, _, ref tool_args) = call_metadata[index];
                self.emit_tool_update(
                    context,
                    tool_call_id,
                    "failed",
                    None,
                    tool_args.clone(),
                    Some(output.clone()),
                )
                .await;
            }
        }

        let outputs: Vec<Value> = indexed_outputs
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    ToolOutput::error_with_kind(
                        "unknown",
                        "unknown",
                        "Tool output missing due to interrupted execution.",
                    )
                })
            })
            .collect();

        perf_log(
            "tool_batch_done",
            &json!({
                "count": outputs.len(),
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "statuses": outputs
                    .iter()
                    .map(|v| v.get("status").and_then(Value::as_str).unwrap_or("").to_string())
                    .collect::<Vec<_>>(),
                "cancelled": cancelled_batch,
            }),
        );

        outputs
    }

    // ── Internal helpers ────────────────────────────────────────────────────

    async fn emit_tool_update(
        &self,
        context: Option<&ToolExecutionContext>,
        tool_call_id: &str,
        status: &str,
        title: Option<&str>,
        raw_input: Option<Value>,
        raw_output: Option<Value>,
    ) {
        let Some(ctx) = context else {
            return;
        };
        let mut event = ToolCallUpdateEvent::new(tool_call_id, status);
        event.title = title.map(str::to_string);
        event.kind = title.map(|_| "other".to_string());
        event.raw_input = raw_input;
        event.raw_output = raw_output;
        let obj = ActivityEvent::ToolCallUpdate(event).into_update();
        let emitter = ctx.emit_update.clone();
        let _ = emitter(ctx.session_id.clone(), obj).await;
    }
}

fn normalize_arguments(arguments: Option<&Value>) -> Result<Map<String, Value>> {
    match arguments {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(map)) => Ok(map.clone()),
        Some(_) => anyhow::bail!("Tool arguments must be an object."),
    }
}

fn tool_status_to_update_status(output: &Value) -> &'static str {
    let raw = output
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match raw.as_str() {
        "running" | "in_progress" | "timeout" => "in_progress",
        "failed" | "error" | "cancelled" => "failed",
        _ => "completed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_running_tools_keeps_manager_open() {
        let manager = ToolRunManager::new(None, 30, AgentTools::default())
            .await
            .unwrap();

        manager.cancel_running_tools().await;

        let output = manager
            .execute_tool_call("call-1", "file_edit", Some(&json!({"patch": ""})), None)
            .await;

        assert_ne!(
            output.get("error").and_then(Value::as_str),
            Some("Tool manager is shutting down.")
        );
    }

    #[tokio::test]
    async fn disabled_file_edit_is_rejected() {
        let tools = AgentTools {
            file_edit: crate::config::models::ToolSwitch::Disable,
            ..AgentTools::default()
        };
        let manager = ToolRunManager::new(None, 30, tools).await.unwrap();

        let output = manager
            .execute_tool_call("call-1", "file_edit", Some(&json!({"patch": ""})), None)
            .await;

        assert_eq!(
            output.get("error").and_then(Value::as_str),
            Some("Tool is disabled: file_edit")
        );

        let output = manager
            .execute_tool_call(
                "call-2",
                "text_replace",
                Some(&json!({"path": "notes.txt", "old_text": "a", "new_text": "b"})),
                None,
            )
            .await;

        assert_eq!(
            output.get("error").and_then(Value::as_str),
            Some("Tool is disabled: text_replace")
        );
    }

    #[tokio::test]
    async fn file_edit_accepts_patch_text_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ToolRunManager::new(Some(tmp.path()), 30, AgentTools::default())
            .await
            .unwrap();

        let output = manager
            .execute_tool_call(
                "call-1",
                "file_edit",
                Some(&json!({"patchText": "*** Begin Patch\n*** Add File: notes.txt\n+alpha\n*** End Patch"})),
                None,
            )
            .await;

        assert_eq!(output["status"], "completed");
        assert!(tmp.path().join("notes.txt").is_file());
    }

    #[tokio::test]
    async fn text_replace_updates_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "alpha\nbeta\n").unwrap();
        let manager = ToolRunManager::new(Some(tmp.path()), 30, AgentTools::default())
            .await
            .unwrap();

        let output = manager
            .execute_tool_call(
                "call-1",
                "text_replace",
                Some(&json!({"path": "notes.txt", "old_text": "beta", "new_text": "gamma"})),
                None,
            )
            .await;

        assert_eq!(output["status"], "completed");
        assert_eq!(output["replacements"], 1);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("notes.txt")).unwrap(),
            "alpha\ngamma\n"
        );
    }

    #[tokio::test]
    async fn wait_uses_seconds_without_session_name() {
        let manager = ToolRunManager::new(None, 30, AgentTools::default())
            .await
            .unwrap();

        let output = manager
            .execute_tool_call("wait-1", "wait", Some(&json!({"seconds": 0.1})), None)
            .await;

        assert_eq!(output["tool"], "wait");
        assert_eq!(output["kind"], "wait");
        assert_eq!(output["status"], "completed");
        assert_eq!(output["seconds"], 0.1);
    }

    #[tokio::test]
    async fn terminal_checkout_uses_name_and_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ToolRunManager::new(Some(tmp.path()), 30, AgentTools::default())
            .await
            .unwrap();
        let command = if cfg!(windows) {
            "1..5 | ForEach-Object { Write-Output \"line$_\" }"
        } else {
            "for i in 1 2 3 4 5; do echo line$i; done"
        };

        let started = manager
            .execute_tool_call(
                "term-1",
                "terminal_exec",
                Some(&json!({"terminal_name": "build", "command": command, "timeout": 5})),
                None,
            )
            .await;
        assert_eq!(started["id"], "term-1");
        assert_eq!(started["name"], "build");

        let checked = manager
            .execute_tool_call(
                "check-1",
                "terminal_checkout",
                Some(&json!({"terminal_name": "build", "lines": 2})),
                None,
            )
            .await;

        assert_eq!(
            checked["output"].as_str().unwrap().replace("\r\n", "\n"),
            "line4\nline5\n"
        );
    }

    #[tokio::test]
    async fn multiple_file_writes_are_rejected_but_other_tools_run() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("existing.txt"), "alpha\n").unwrap();
        let tools = AgentTools {
            subagent: crate::config::models::ToolSwitch::Disable,
            ..AgentTools::default()
        };
        let manager = ToolRunManager::new(Some(tmp.path()), 30, tools)
            .await
            .unwrap();
        let terminal_command = if cfg!(windows) {
            "Write-Output terminal-ok"
        } else {
            "printf 'terminal-ok\\n'"
        };

        let outputs = manager
            .execute_tool_calls(
                vec![
                    json!({
                        "tool_call_id": "edit-1",
                        "name": "file_edit",
                        "arguments": {"patch": "*** Begin Patch\n*** Add File: a.txt\n+alpha\n*** End Patch"},
                    }),
                    json!({
                        "tool_call_id": "term-1",
                        "name": "terminal_exec",
                        "arguments": {"command": terminal_command, "timeout": 5},
                    }),
                    json!({
                        "tool_call_id": "edit-2",
                        "name": "text_replace",
                        "arguments": {"path": "existing.txt", "old_text": "alpha", "new_text": "beta"},
                    }),
                ],
                None,
            )
            .await;

        assert_eq!(outputs.len(), 3);
        for output in [&outputs[0], &outputs[2]] {
            assert_eq!(output["status"], "error");
            assert!(
                output["error"]
                    .as_str()
                    .unwrap()
                    .contains("Multiple file write tool calls")
            );
        }
        assert_eq!(outputs[1]["status"], "completed");
        assert!(
            outputs[1]["output"]
                .as_str()
                .unwrap()
                .contains("terminal-ok")
        );
        assert!(!tmp.path().join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("existing.txt")).unwrap(),
            "alpha\n"
        );
    }
}
