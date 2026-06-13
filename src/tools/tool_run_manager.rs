//! Unified tool dispatcher and session registry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use super::file_edit_runtime::file_edit_text;
use super::session::{Cap, ToolSession};
use super::subagent_tool_runtime::{
    SpawnSubagentPayload, ToolExecutionContext, subagent_not_found,
};
use super::terminal_runtime::{
    TerminalExecutor, TerminalSession, terminal_not_found, terminal_wait,
};
use crate::agent::activity::event::{ActivityEvent, ToolCallUpdateEvent};
use crate::config::models::AgentTools;
use crate::utils::perf::perf_log;
use crate::utils::policy::cancelled_tool_output;

/// Builder hook for spawning subagent sessions. The concrete impl lives in
/// `agent::subagent` (round 4); the tool runtime only needs the trait surface
/// and the spawn-payload plumbing.
#[async_trait::async_trait]
pub trait SubagentExecutor: Send + Sync {
    async fn create_session(
        &self,
        tool_call_id: &str,
        task: &str,
        policy: Option<&str>,
        context: &ToolExecutionContext,
    ) -> Result<Arc<Mutex<dyn ToolSession>>>;
}

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

const IMMEDIATE_TOOLS: &[&str] = &["file_edit"];
const CREATE_TOOLS: &[&str] = &["terminal_exec", "spawn_subagent"];
const LIST_TOOLS: &[&str] = &["list_terminals", "list_subagents"];
const OPERATE_TOOLS: &[&str] = &[
    "terminal_wait",
    "terminal_checkout",
    "terminal_kill",
    "wait_subagent",
    "checkout_subagent",
    "send_subagent",
    "close_subagent",
];

/// Dispatches tool calls to concrete runtimes and runs batches in parallel.
pub struct ToolRunManager {
    cwd: PathBuf,
    runtime_tools: AgentTools,
    terminal_executor: TerminalExecutor,
    subagent_executor: Mutex<Option<Arc<dyn SubagentExecutor>>>,
    channel_tool_executor: Mutex<Option<Arc<dyn ChannelToolExecutor>>>,
    state: Mutex<ToolManagerState>,
    finished_ttl_seconds: u64,
}

struct ToolManagerState {
    sessions: HashMap<String, Arc<Mutex<dyn ToolSession>>>,
    updated_at: HashMap<String, Instant>,
    closing: bool,
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
        let terminal_executor = TerminalExecutor::new(Some(runtime_cwd.clone()));
        Ok(Self {
            cwd: runtime_cwd,
            runtime_tools,
            terminal_executor,
            subagent_executor: Mutex::new(None),
            channel_tool_executor: Mutex::new(None),
            state: Mutex::new(ToolManagerState {
                sessions: HashMap::new(),
                updated_at: HashMap::new(),
                closing: false,
            }),
            finished_ttl_seconds: finished_ttl_seconds.max(30),
        })
    }

    pub async fn set_subagent_executor(&self, executor: Option<Arc<dyn SubagentExecutor>>) {
        let mut guard = self.subagent_executor.lock().await;
        *guard = executor;
    }

    pub async fn set_channel_tool_executor(&self, executor: Option<Arc<dyn ChannelToolExecutor>>) {
        let mut guard = self.channel_tool_executor.lock().await;
        *guard = executor;
    }

    pub async fn ashutdown(&self) {
        let mut state = self.state.lock().await;
        state.closing = true;
        let sessions: Vec<Arc<Mutex<dyn ToolSession>>> = state.sessions.values().cloned().collect();
        state.sessions.clear();
        state.updated_at.clear();
        drop(state);
        for session in sessions {
            let mut guard = session.lock().await;
            let _ = guard.cancel().await;
        }
    }

    pub async fn cancel_running_tools(&self) {
        let mut state = self.state.lock().await;
        let sessions: Vec<Arc<Mutex<dyn ToolSession>>> = state.sessions.values().cloned().collect();
        state.sessions.clear();
        state.updated_at.clear();
        drop(state);

        for session in sessions {
            let mut guard = session.lock().await;
            let _ = guard.cancel().await;
        }
    }

    pub async fn cancel_tool_call(&self, tool_call_id: &str) -> bool {
        let normalized = tool_call_id.trim();
        let session = {
            let state = self.state.lock().await;
            state.sessions.get(normalized).cloned()
        };
        match session {
            None => false,
            Some(session) => {
                {
                    let mut guard = session.lock().await;
                    let _ = guard.cancel().await;
                }
                self.touch_session(&session).await;
                true
            }
        }
    }

    /// Run one ACP/model tool call through the session registry.
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
            return tool_error(tool_name, "tool_call_id is required.");
        }
        {
            let state = self.state.lock().await;
            if state.closing {
                return tool_error(tool_name, "Tool manager is shutting down.");
            }
        }
        self.prune_finished_sessions().await;

        let tool_args = match normalize_arguments(arguments) {
            Ok(args) => args,
            Err(err) => return tool_error(tool_name, &err.to_string()),
        };

        let channel_executor = {
            let guard = self.channel_tool_executor.lock().await;
            guard
                .as_ref()
                .filter(|exec| exec.handles_tool(tool_name))
                .cloned()
        };

        let is_known = channel_executor.is_some()
            || IMMEDIATE_TOOLS.contains(&tool_name)
            || CREATE_TOOLS.contains(&tool_name)
            || OPERATE_TOOLS.contains(&tool_name)
            || LIST_TOOLS.contains(&tool_name);
        if !is_known {
            return tool_error(tool_name, &format!("Unknown tool: {tool_name}"));
        }
        if let Some(executor) = channel_executor {
            return match executor
                .execute_channel_tool(tool_name, &tool_args, context)
                .await
            {
                Ok(value) => value,
                Err(err) => tool_error(tool_name, &format!("{err:#}")),
            };
        }
        if !self.is_tool_enabled(tool_name) {
            return tool_error(tool_name, &format!("Tool is disabled: {tool_name}"));
        }

        if LIST_TOOLS.contains(&tool_name) {
            return self.list_sessions(tool_name).await;
        }

        if tool_name == "terminal_wait" {
            let time = tool_args.get("time").and_then(Value::as_f64).unwrap_or(0.0);
            return terminal_wait(time)
                .await
                .unwrap_or_else(|err| tool_error(tool_name, &format!("{err:#}")));
        }

        if IMMEDIATE_TOOLS.contains(&tool_name) {
            let patch = tool_args
                .get("patch")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            return match file_edit_text(&patch, &self.cwd) {
                Ok(value) => value,
                Err(err) => tool_error(tool_name, &format!("{err:#}")),
            };
        }

        if CREATE_TOOLS.contains(&tool_name) {
            return match self
                .spawn_create_session(normalized_id, tool_name, &tool_args, context)
                .await
            {
                Ok(value) => value,
                Err(err) => tool_error(tool_name, &format!("{err:#}")),
            };
        }

        // OPERATE tools.
        let session = self.resolve_operate_session(tool_name, &tool_args).await;
        let Some(session) = session else {
            return session_not_found(tool_name, &tool_args);
        };
        match self.dispatch_op(&session, tool_name, &tool_args).await {
            Ok(value) => {
                self.touch_session(&session).await;
                value
            }
            Err(err) => tool_error(tool_name, &format!("{err:#}")),
        }
    }

    /// Run multiple managed tool calls concurrently and preserve order.
    ///
    /// Matches the Python `execute_tool_calls` semantics: every call's future
    /// is started immediately and driven concurrently on the current task,
    /// with a 100ms poll window that checks the cancel event.
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
        let has_multiple_file_edits = tool_calls
            .iter()
            .filter(|call| {
                call.get("name").and_then(Value::as_str).map(str::trim) == Some("file_edit")
            })
            .count()
            > 1;

        // Save per-call metadata so the cancel path can emit updates.
        let mut call_metadata: Vec<(String, Option<Value>)> = Vec::with_capacity(total);

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
            call_metadata.push((tool_call_id.clone(), tool_args.clone()));
            self.emit_tool_update(
                context,
                &tool_call_id,
                "in_progress",
                Some(&tool_name),
                tool_args.clone(),
                None,
            )
            .await;

            if has_multiple_file_edits && tool_name == "file_edit" {
                let output = multiple_file_edit_error();
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
                let output = cancelled_tool_output();
                *slot = Some(output.clone());
                // Emit a "failed" tool_call_update so the client UI sees the
                // transition from in_progress → failed for each cancelled slot.
                let (ref tool_call_id, ref tool_args) = call_metadata[index];
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
                    json!({
                        "status": "completed_error",
                        "error": "Tool output missing due to interrupted execution.",
                    })
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

    fn is_tool_enabled(&self, tool_name: &str) -> bool {
        match tool_name {
            "file_edit" => self.runtime_tools.file_edit_enabled(),
            "terminal_exec" | "list_terminals" | "terminal_wait" | "terminal_checkout"
            | "terminal_kill" => self.runtime_tools.terminal_enabled(),
            "spawn_subagent" | "list_subagents" | "wait_subagent" | "checkout_subagent"
            | "send_subagent" | "close_subagent" => self.runtime_tools.subagent_enabled(),
            _ => true,
        }
    }

    async fn spawn_create_session(
        &self,
        tool_call_id: &str,
        name: &str,
        args: &Map<String, Value>,
        context: Option<&ToolExecutionContext>,
    ) -> Result<Value> {
        let session: Arc<Mutex<dyn ToolSession>> = match name {
            "terminal_exec" => {
                let command = args
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let env = args.get("env").and_then(Value::as_object).map(|map| {
                    map.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                        .collect::<HashMap<_, _>>()
                });
                let timeout = args.get("timeout").and_then(Value::as_f64).unwrap_or(30.0);
                let terminal = TerminalSession::new(
                    tool_call_id.to_string(),
                    self.terminal_executor.clone(),
                    command,
                    env,
                    timeout,
                );
                Arc::new(Mutex::new(terminal))
            }
            "spawn_subagent" => {
                let context = context.ok_or_else(|| {
                    anyhow::anyhow!("spawn_subagent requires tool execution context.")
                })?;
                let payload = SpawnSubagentPayload::from_json(Value::Object(args.clone()))?;
                let subagent_guard = self.subagent_executor.lock().await;
                let subagent = subagent_guard
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("spawn_subagent has no executor attached."))?
                    .clone();
                drop(subagent_guard);
                subagent
                    .create_session(
                        tool_call_id,
                        &payload.task,
                        payload.policy.as_deref(),
                        context,
                    )
                    .await?
            }
            other => anyhow::bail!("Unknown tool: {other}"),
        };

        self.save_session(tool_call_id, session.clone()).await;
        let output = {
            let mut guard = session.lock().await;
            guard
                .start(args)
                .await
                .unwrap_or_else(|err| tool_error(name, &format!("{err:#}")))
        };
        self.touch_session(&session).await;

        if let Some(runtime_id) = output
            .get("runtime")
            .and_then(Value::as_object)
            .and_then(|obj| obj.get("id"))
            .and_then(Value::as_str)
        {
            let trimmed = runtime_id.trim().to_string();
            if !trimmed.is_empty() && trimmed != tool_call_id {
                self.save_session(&trimmed, session.clone()).await;
            }
        }
        Ok(output)
    }

    async fn resolve_operate_session(
        &self,
        tool_name: &str,
        args: &Map<String, Value>,
    ) -> Option<Arc<Mutex<dyn ToolSession>>> {
        let key = if tool_name.starts_with("terminal_") {
            args.get("id")
        } else {
            args.get("subagent_run_id")
        }
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
        if key.is_empty() {
            return None;
        }
        let state = self.state.lock().await;
        state.sessions.get(&key).cloned()
    }

    async fn dispatch_op(
        &self,
        session: &Arc<Mutex<dyn ToolSession>>,
        name: &str,
        args: &Map<String, Value>,
    ) -> Result<Value> {
        let mut guard = session.lock().await;
        match name {
            "wait_subagent" => {
                if !guard.capabilities().contains(&Cap::Wait) {
                    return Ok(tool_error(name, "session does not support wait"));
                }
                let timeout = args.get("timeout").and_then(Value::as_f64).unwrap_or(30.0);
                guard.wait(timeout, &Map::new()).await
            }
            "terminal_checkout" => {
                if !guard.capabilities().contains(&Cap::Checkout) {
                    return Ok(tool_error(name, "session does not support checkout"));
                }
                let mut op_args = Map::new();
                if let Some(v) = args.get("tail_line_num").cloned() {
                    op_args.insert("tail_line_num".to_string(), v);
                }
                guard.checkout(&op_args).await
            }
            "checkout_subagent" => {
                if !guard.capabilities().contains(&Cap::Checkout) {
                    return Ok(tool_error(name, "session does not support checkout"));
                }
                guard.checkout(&Map::new()).await
            }
            "terminal_kill" | "close_subagent" => {
                guard.cancel().await?;
                guard.checkout(&Map::new()).await
            }
            "send_subagent" => {
                if !guard.capabilities().contains(&Cap::Send) {
                    return Ok(tool_error(name, "session does not support send"));
                }
                let message = args
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let interrupt = args
                    .get("interrupt")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                guard.send(&message, interrupt).await
            }
            other => Ok(tool_error(other, &format!("Unknown tool: {other}"))),
        }
    }

    async fn save_session(&self, key: &str, session: Arc<Mutex<dyn ToolSession>>) {
        let key = key.trim().to_string();
        if key.is_empty() {
            return;
        }
        let mut state = self.state.lock().await;
        state.updated_at.insert(key.clone(), Instant::now());
        state.sessions.insert(key, session);
    }

    async fn touch_session(&self, session: &Arc<Mutex<dyn ToolSession>>) {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        let keys: Vec<String> = state
            .sessions
            .iter()
            .filter(|(_, existing)| Arc::ptr_eq(existing, session))
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            state.updated_at.insert(key, now);
        }
    }

    async fn list_sessions(&self, tool_name: &str) -> Value {
        let target_kind = if tool_name == "list_terminals" {
            "terminal"
        } else {
            "subagent"
        };
        let sessions: Vec<Arc<Mutex<dyn ToolSession>>> = {
            let state = self.state.lock().await;
            let mut unique: Vec<Arc<Mutex<dyn ToolSession>>> = Vec::new();
            for session in state.sessions.values() {
                if !unique.iter().any(|existing| Arc::ptr_eq(existing, session)) {
                    unique.push(session.clone());
                }
            }
            unique
        };

        let mut items: Vec<Value> = Vec::new();
        for session in sessions {
            let guard = session.lock().await;
            let item = guard.list_item();
            let kind = item.get("kind").and_then(Value::as_str).unwrap_or("");
            if kind != target_kind {
                continue;
            }
            let done = item.get("done").and_then(Value::as_bool).unwrap_or(false);
            if done {
                continue;
            }
            items.push(item);
        }
        json!({
            "status": "ok",
            "kind": target_kind,
            "items": items,
        })
    }

    async fn prune_finished_sessions(&self) {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        let ttl = Duration::from_secs(self.finished_ttl_seconds);
        let mut expired: Vec<String> = Vec::new();
        let snapshot: Vec<(String, Arc<Mutex<dyn ToolSession>>, Instant)> = state
            .sessions
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.clone(),
                    state.updated_at.get(k).copied().unwrap_or(now),
                )
            })
            .collect();
        for (key, session, updated) in snapshot {
            let is_done = session.try_lock().map(|g| g.is_done()).unwrap_or(false);
            if is_done && now.saturating_duration_since(updated) >= ttl {
                expired.push(key);
            }
        }
        for key in expired {
            state.sessions.remove(&key);
            state.updated_at.remove(&key);
        }
    }

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
        // Keep subagent's structured payload for the model result path, but render
        // the ACP tool card as readable flow markdown instead of raw JSON.
        let (card_raw_output, card_content) = match &raw_output {
            Some(output) if is_subagent_output(output) => {
                let raw = output.get("model_result").cloned();
                let content = render_subagent_as_content(output);
                (raw, content)
            }
            _ => (raw_output, None),
        };
        let mut event = ToolCallUpdateEvent::new(tool_call_id, status);
        event.title = title.map(str::to_string);
        event.kind = title.map(|_| "other".to_string());
        event.raw_input = raw_input;
        event.raw_output = card_raw_output;
        event.content = card_content;
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

fn tool_error(tool_name: &str, message: &str) -> Value {
    json!({
        "status": "error",
        "done": true,
        "error": message,
        "tool": tool_name,
    })
}

fn multiple_file_edit_error() -> Value {
    tool_error(
        "file_edit",
        "Multiple file_edit calls in one assistant turn are not allowed. Combine all file operations into one file_edit patch.",
    )
}

fn is_subagent_output(output: &Value) -> bool {
    output
        .get("runtime")
        .and_then(Value::as_object)
        .and_then(|runtime| runtime.get("kind"))
        .and_then(Value::as_str)
        == Some("subagent")
}

/// Render subagent tool output as markdown content suitable for the Zed tool
/// card. The structured JSON remains the value returned to the model.
fn render_subagent_as_content(output: &Value) -> Option<Value> {
    if let Some(progress) = output.get("progress").and_then(Value::as_str)
        && !progress.trim().is_empty()
    {
        return markdown_tool_content(progress);
    }

    let flow = output.get("flow").and_then(Value::as_array);
    if let Some(flow) = flow
        && !flow.is_empty()
    {
        let rendered = render_flow_items(flow);
        if !rendered.trim().is_empty() {
            return markdown_tool_content(&rendered);
        }
    }

    let subagent_id = output
        .get("subagent_run_id")
        .and_then(Value::as_str)
        .unwrap_or("subagent");
    let status = output
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("running");

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Subagent `{subagent_id}`"));
    lines.push(String::new());
    lines.push(format!("Status: `{status}`"));
    if let Some(task) = output.get("task").and_then(Value::as_str)
        && !task.trim().is_empty()
    {
        lines.push(String::new());
        lines.push("Task:".to_string());
        lines.push(String::new());
        lines.push(task.to_string());
    }
    if let Some(message) = output.get("message").and_then(Value::as_str)
        && !message.trim().is_empty()
    {
        lines.push(String::new());
        lines.push(message.to_string());
    }
    if let Some(error) = output.get("error").and_then(Value::as_str)
        && !error.trim().is_empty()
    {
        lines.push(String::new());
        lines.push(format!("Error: {error}"));
    }
    markdown_tool_content(&lines.join("\n"))
}

fn render_flow_items(flow: &[Value]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for item in flow {
        let kind = item.get("kind").and_then(Value::as_str).unwrap_or("");
        let text = item.get("text").and_then(Value::as_str).unwrap_or("");
        if text.is_empty() && kind != "execution" {
            continue;
        }
        match kind {
            "thinking" => {
                lines.push("**[Thinking]**".to_string());
                lines.push(String::new());
                lines.push(text.to_string());
                lines.push(String::new());
            }
            "execution" => {
                lines.push(format!("**[Tool]** {text}"));
                lines.push(String::new());
            }
            "response" => {
                lines.push("**[Response]**".to_string());
                lines.push(String::new());
                lines.push(text.to_string());
                lines.push(String::new());
            }
            _ => {
                if !text.is_empty() {
                    lines.push(text.to_string());
                    lines.push(String::new());
                }
            }
        }
    }
    lines.join("\n").trim_end().to_string()
}

fn markdown_tool_content(markdown: &str) -> Option<Value> {
    let text = markdown.trim_end();
    if text.is_empty() {
        return None;
    }
    Some(json!([{"type": "content", "content": {"type": "text", "text": text}}]))
}

fn tool_status_to_update_status(output: &Value) -> &'static str {
    let raw = output
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match raw.as_str() {
        "running" | "in_progress" => "in_progress",
        "completed_error" | "failed" | "error" => "failed",
        _ => "completed",
    }
}

fn session_not_found(tool_name: &str, args: &Map<String, Value>) -> Value {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if tool_name.starts_with("terminal_") && !id.is_empty() {
        return terminal_not_found(id);
    }
    let sub_id = args
        .get("subagent_run_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if !sub_id.is_empty() {
        return subagent_not_found(sub_id);
    }
    tool_error(tool_name, "session not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_running_output_renders_markdown_card_content() {
        let output = json!({
            "done": false,
            "runtime": {
                "id": "call_00",
                "kind": "subagent",
                "status": "running"
            },
            "status": "running",
            "subagent_run_id": "call_00",
            "message": "subagent started"
        });

        let content = render_subagent_as_content(&output).unwrap();

        assert_eq!(content[0]["content"]["type"], "text");
        assert_eq!(
            content[0]["content"]["text"],
            "Subagent `call_00`\n\nStatus: `running`\n\nsubagent started"
        );
    }

    #[test]
    fn subagent_progress_output_uses_flow_markdown() {
        let output = json!({
            "runtime": {"kind": "subagent"},
            "progress": "Agent Flow:\n\n[tool] terminal_exec"
        });

        let content = render_subagent_as_content(&output).unwrap();

        assert_eq!(
            content[0]["content"]["text"],
            "Agent Flow:\n\n[tool] terminal_exec"
        );
    }

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
    }

    #[tokio::test]
    async fn terminal_wait_uses_time_without_session_id() {
        let manager = ToolRunManager::new(None, 30, AgentTools::default())
            .await
            .unwrap();

        let output = manager
            .execute_tool_call("wait-1", "terminal_wait", Some(&json!({"time": 0.1})), None)
            .await;

        assert_eq!(output["status"], "ok");
        assert_eq!(output["done"], true);
        assert_eq!(output["waited_seconds"], 0.1);
    }

    #[tokio::test]
    async fn terminal_checkout_uses_id_and_tail_line_num() {
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
                Some(&json!({"command": command, "timeout": 5})),
                None,
            )
            .await;
        assert_eq!(started["id"], "term-1");

        let checked = manager
            .execute_tool_call(
                "check-1",
                "terminal_checkout",
                Some(&json!({"id": "term-1", "tail_line_num": 2})),
                None,
            )
            .await;

        assert_eq!(checked["returned_lines"], 2);
        assert_eq!(
            checked["output"].as_str().unwrap().replace("\r\n", "\n"),
            "line4\nline5\n"
        );
    }

    #[tokio::test]
    async fn multiple_file_edits_are_rejected_but_other_tools_run() {
        let tmp = tempfile::tempdir().unwrap();
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
                        "name": "file_edit",
                        "arguments": {"patch": "*** Begin Patch\n*** Add File: b.txt\n+beta\n*** End Patch"},
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
                    .contains("Multiple file_edit calls")
            );
        }
        assert_eq!(outputs[1]["status"], "completed_success");
        assert!(
            outputs[1]["output"]
                .as_str()
                .unwrap()
                .contains("terminal-ok")
        );
        assert!(!tmp.path().join("a.txt").exists());
        assert!(!tmp.path().join("b.txt").exists());
    }
}
