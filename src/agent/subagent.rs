//! Native subagent sessions with ACP-visible progress updates.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, Notify};

use super::activity::{
    ActivityTurnHandle,
    event::{
        ActivityEvent, EVENT_AGENT_MESSAGE_CHUNK, EVENT_AGENT_THOUGHT_CHUNK, EVENT_TOOL_CALL,
        ToolCallEvent, ToolCallUpdateEvent,
    },
};
use super::constants::{STOP_CANCELLED, STOP_COMPLETED, STOP_MAX_TURNS};
use super::factory::SessionAgentFactory;
use super::policy::clamp_subagent_policy;
use super::turn::{PolicyModeGetter, TurnRuntime, run_turn};
use crate::config::models::{AgentState, AgentTools, ToolSwitch};
use crate::context::manager::{CancelEvent, ConversationContextManager};
use crate::llm::client::BaseLlmClient;
use crate::tools::session::{Cap, ToolArgs, ToolSession};
use crate::tools::subagent_tool_runtime::{
    PermissionRequester, StateSetter, ToolExecutionContext, UpdateEmitter,
};
use crate::tools::tool_run_manager::{SubagentExecutor as SubagentExecutorTrait, ToolRunManager};
use crate::tools::tool_schemas;
use crate::utils::perf::{messages_size, perf_log};

const SNAPSHOT_FLOW_LIMIT: usize = 3;

/// Collect the subagent tool schemas (codemode + terminal) that match the
/// Python `_SUBAGENT_TOOL_SCHEMA_PATHS` slice.
fn subagent_tool_schemas(tools: &AgentTools) -> Vec<Value> {
    let subagent_tools = AgentTools {
        mcp: tools.mcp,
        file_edit: ToolSwitch::Disable,
        terminal: tools.terminal,
        subagent: ToolSwitch::Disable,
    };
    tool_schemas(&subagent_tools)
}

// ── Card state ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct FlowItem {
    kind: String,
    text: String,
}

/// Progress card state mirroring Python's `SubagentCardState`.
pub struct SubagentCardState {
    task: String,
    policy: String,
    flow: Vec<FlowItem>,
}

impl SubagentCardState {
    pub fn new(task: impl Into<String>, policy: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            policy: policy.into(),
            flow: Vec::new(),
        }
    }

    pub fn append_thinking(&mut self, text: &str) {
        self.append_flow_text("thinking", text);
    }

    pub fn append_execution(&mut self, title: &str) {
        let trimmed = title.trim();
        if !trimmed.is_empty() {
            self.flow.push(FlowItem {
                kind: "execution".to_string(),
                text: trimmed.to_string(),
            });
        }
    }

    pub fn append_response(&mut self, text: &str) {
        self.append_flow_text("response", text);
    }

    fn append_flow_text(&mut self, kind: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.flow.last_mut()
            && last.kind == kind
        {
            last.text.push_str(text);
            return;
        }
        self.flow.push(FlowItem {
            kind: kind.to_string(),
            text: text.to_string(),
        });
    }

    pub fn render(&self) -> String {
        render_card(&self.task, &self.policy, &self.flow, false)
    }
}

fn render_card(task: &str, policy: &str, flow: &[FlowItem], is_window: bool) -> String {
    let mut flow_lines: Vec<String> = Vec::new();
    for item in flow {
        match item.kind.as_str() {
            "thinking" => {
                flow_lines.push("[thinking]".to_string());
                flow_lines.push(String::new());
                flow_lines.push(item.text.clone());
                flow_lines.push(String::new());
            }
            "execution" => {
                flow_lines.push(format!("[tool] {}", item.text));
                flow_lines.push(String::new());
            }
            "response" => {
                flow_lines.push("[response]".to_string());
                flow_lines.push(String::new());
                flow_lines.push(item.text.clone());
                flow_lines.push(String::new());
            }
            _ => {}
        }
    }
    if flow_lines.is_empty() {
        flow_lines.push(String::new());
    }

    let header = if is_window {
        "Recent Agent Flow:"
    } else {
        "Agent Flow:"
    };
    let mut sections = vec![
        "Task:".to_string(),
        String::new(),
        task.to_string(),
        String::new(),
        "Policy:".to_string(),
        String::new(),
        policy.to_string(),
        String::new(),
        header.to_string(),
        String::new(),
    ];
    sections.extend(flow_lines);
    sections.join("\n").trim_end().to_string()
}

// ── Executor (trait impl) ──────────────────────────────────────────────────

/// Builds subagent sessions on demand. Mirror of Python's `SubagentExecutor`.
pub struct SubagentExecutor {
    parent_session_id: String,
    main_system_message: Value,
    max_running_turn: Option<u32>,
    runtime_factory_shape: SessionAgentFactory,
    subagent_tools: Vec<Value>,
}

impl SubagentExecutor {
    pub fn new(
        parent_session_id: String,
        main_system_message: Value,
        max_running_turn: Option<u32>,
        runtime_factory_shape: SessionAgentFactory,
        tools: AgentTools,
    ) -> Self {
        Self {
            parent_session_id,
            main_system_message,
            max_running_turn,
            runtime_factory_shape,
            subagent_tools: subagent_tool_schemas(&tools),
        }
    }
}

#[async_trait]
impl SubagentExecutorTrait for SubagentExecutor {
    async fn create_session(
        &self,
        tool_call_id: &str,
        task: &str,
        policy: Option<&str>,
        context: &ToolExecutionContext,
    ) -> Result<Arc<Mutex<dyn ToolSession>>> {
        let effective_policy = clamp_subagent_policy(&context.mode_id, policy)?;
        let parts = self
            .runtime_factory_shape
            .create_subagent_parts(self.main_system_message.clone())
            .await?;
        let session = SubagentSession::new(
            tool_call_id.to_string(),
            self.parent_session_id.clone(),
            task.to_string(),
            effective_policy,
            self.max_running_turn,
            context.cancel_event.clone(),
            context.emit_update.clone(),
            context.request_permission.clone(),
            self.subagent_tools.clone(),
            parts.model_client,
            parts.tool_manager,
            parts.context_manager,
        );
        Ok(Arc::new(Mutex::new(session)))
    }
}

// ── SubagentSession ────────────────────────────────────────────────────────

pub struct SubagentSession {
    session_id: String,
    parent_session_id: String,
    task: String,
    policy: String,
    max_running_turn: Option<u32>,
    cancel_event: CancelEvent,
    emit_parent_update: UpdateEmitter,
    parent_request_permission: PermissionRequester,
    confirm_lock: Arc<Mutex<()>>,
    inner: Arc<SubagentInner>,
}

struct SubagentInner {
    model_client: Mutex<Option<BaseLlmClient>>,
    tool_schemas: Vec<Value>,
    tool_manager: Arc<ToolRunManager>,
    context_manager: Mutex<Option<ConversationContextManager>>,
    status: Mutex<String>,
    last_result: Mutex<String>,
    error: Mutex<String>,
    card: Mutex<SubagentCardState>,
    started_at: Mutex<Option<Instant>>,
    updated_at: Mutex<Option<Instant>>,
    update_counts: Mutex<HashMap<String, u64>>,
    turn_done: Notify,
    turn_running: Mutex<bool>,
    cancel_token: Mutex<CancelEvent>,
}

impl SubagentSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: String,
        parent_session_id: String,
        task: String,
        policy: String,
        max_running_turn: Option<u32>,
        cancel_event: CancelEvent,
        emit_parent_update: UpdateEmitter,
        parent_request_permission: PermissionRequester,
        tool_schemas: Vec<Value>,
        model_client: BaseLlmClient,
        tool_manager: Arc<ToolRunManager>,
        context_manager: ConversationContextManager,
    ) -> Self {
        let card = SubagentCardState::new(task.clone(), policy.clone());
        Self {
            session_id,
            parent_session_id,
            task,
            policy,
            max_running_turn,
            cancel_event,
            emit_parent_update,
            parent_request_permission,
            // Per-subagent confirm lock: serialises permission prompts
            // inside this subagent only, so unrelated subagents (and the
            // main agent) never block on each other's permission UI.
            confirm_lock: Arc::new(Mutex::new(())),
            inner: Arc::new(SubagentInner {
                model_client: Mutex::new(Some(model_client)),
                tool_schemas,
                tool_manager,
                context_manager: Mutex::new(Some(context_manager)),
                status: Mutex::new("created".to_string()),
                last_result: Mutex::new(String::new()),
                error: Mutex::new(String::new()),
                card: Mutex::new(card),
                started_at: Mutex::new(None),
                updated_at: Mutex::new(None),
                update_counts: Mutex::new(HashMap::new()),
                turn_done: Notify::new(),
                turn_running: Mutex::new(false),
                cancel_token: Mutex::new(CancelEvent::new()),
            }),
        }
    }

    async fn start_turn(self: &Self, message: String) {
        {
            let mut status = self.inner.status.lock().await;
            *status = "running".to_string();
        }
        {
            let mut started_at = self.inner.started_at.lock().await;
            if started_at.is_none() {
                *started_at = Some(Instant::now());
            }
        }
        {
            let mut running = self.inner.turn_running.lock().await;
            *running = true;
        }
        let cancel_token = CancelEvent::new();
        {
            let mut token = self.inner.cancel_token.lock().await;
            *token = cancel_token.clone();
        }

        let inner = self.inner.clone();
        let session_id = self.session_id.clone();
        let parent_session_id = self.parent_session_id.clone();
        let emit_parent = self.emit_parent_update.clone();
        let parent_perm = self.parent_request_permission.clone();
        let confirm_lock = self.confirm_lock.clone();
        let parent_cancel = self.cancel_event.clone();
        let max_running_turn = self.max_running_turn;
        let policy = self.policy.clone();

        tokio::spawn(async move {
            let outcome = run_subagent_turn(
                &inner,
                session_id.clone(),
                parent_session_id,
                message,
                policy,
                max_running_turn,
                parent_cancel,
                cancel_token,
                emit_parent,
                parent_perm,
                confirm_lock,
            )
            .await;

            let (status_text, error_text) = match outcome {
                Ok(stop_reason) => match stop_reason.as_str() {
                    STOP_COMPLETED => ("waiting_input".to_string(), String::new()),
                    STOP_CANCELLED => (
                        "cancelled".to_string(),
                        "subagent was cancelled".to_string(),
                    ),
                    STOP_MAX_TURNS => (
                        "failed".to_string(),
                        "subagent reached max turns".to_string(),
                    ),
                    other => (
                        "failed".to_string(),
                        format!("subagent stopped with reason: {other}"),
                    ),
                },
                Err(err) => ("failed".to_string(), format!("{err:#}")),
            };

            {
                let mut status = inner.status.lock().await;
                *status = status_text.clone();
            }
            if !error_text.is_empty() {
                let mut card = inner.card.lock().await;
                card.append_response(&error_text);
                let mut err_slot = inner.error.lock().await;
                *err_slot = error_text;
            } else {
                // Capture final response from context messages for completed runs.
                let guard = inner.context_manager.lock().await;
                if let Some(cm) = guard.as_ref() {
                    let final_response = extract_last_assistant_response(cm.messages());
                    let mut last_result = inner.last_result.lock().await;
                    *last_result = final_response;
                }
            }
            {
                let mut updated_at = inner.updated_at.lock().await;
                *updated_at = Some(Instant::now());
            }
            {
                let mut running = inner.turn_running.lock().await;
                *running = false;
            }
            inner.turn_done.notify_waiters();
        });
    }

    async fn cancel_turn(&self, target_status: &str) {
        let running = *self.inner.turn_running.lock().await;
        if running {
            let token = self.inner.cancel_token.lock().await.clone();
            token.set();
            // Wait for the background task to flip `turn_running` off.
            for _ in 0..200 {
                if !*self.inner.turn_running.lock().await {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        {
            let mut status = self.inner.status.lock().await;
            *status = target_status.to_string();
        }
        {
            let mut updated_at = self.inner.updated_at.lock().await;
            *updated_at = Some(Instant::now());
        }
        let status_for_update = if target_status == "closed" {
            "completed".to_string()
        } else {
            "cancelled".to_string()
        };
        let _ = emit_record_update(
            &self.emit_parent_update,
            &self.parent_session_id,
            &self.session_id,
            &status_for_update,
            &self.render_card().await,
        )
        .await;
    }

    async fn render_card(&self) -> String {
        self.inner.card.lock().await.render()
    }

    async fn snapshot(&self, done: bool, message: Option<&str>, error: Option<&str>) -> Value {
        let status = self.inner.status.lock().await.clone();
        let error_slot = self.inner.error.lock().await.clone();
        let last_result = self.inner.last_result.lock().await.clone();
        let card = self.inner.card.lock().await;
        let total_flow = card.flow.len();
        let recent_flow: Vec<FlowItem> = card
            .flow
            .iter()
            .rev()
            .take(SNAPSHOT_FLOW_LIMIT)
            .cloned()
            .collect();
        let recent_flow: Vec<FlowItem> = recent_flow.into_iter().rev().collect();
        let progress = render_card(&self.task, &self.policy, &recent_flow, true);
        let flow_values: Vec<Value> = recent_flow
            .iter()
            .map(|item| json!({ "kind": item.kind, "text": item.text }))
            .collect();

        let mut payload = json!({
            "subagent_run_id": self.session_id,
            "runtime": {
                "kind": "subagent",
                "id": self.session_id,
                "status": status,
            },
            "status": status,
            "done": done,
            "task": self.task,
            "policy": self.policy,
            "progress": progress,
            "flow": flow_values,
            "flow_items": total_flow,
            "flow_window": format!("last_{SNAPSHOT_FLOW_LIMIT}"),
            "flow_truncated": total_flow > SNAPSHOT_FLOW_LIMIT,
            "result": last_result,
            "model_result": self.model_result_payload(&status, &last_result, &error_slot),
        });
        if let Some(msg) = message
            && let Value::Object(obj) = &mut payload
        {
            obj.insert("message".to_string(), Value::String(msg.to_string()));
        }
        let final_error = match error {
            Some(e) if !e.is_empty() => Some(e.to_string()),
            _ if !error_slot.is_empty() => Some(error_slot.clone()),
            _ => None,
        };
        if let Some(err) = final_error
            && let Value::Object(obj) = &mut payload
        {
            obj.insert("error".to_string(), Value::String(err));
        }
        payload
    }

    fn model_result_payload(&self, status: &str, last_result: &str, error: &str) -> Value {
        let done = !matches!(status, "created" | "running");
        let mut payload = json!({
            "subagent_run_id": self.session_id,
            "runtime": {
                "kind": "subagent",
                "id": self.session_id,
                "status": status,
            },
            "status": status,
            "done": done,
        });
        if !last_result.is_empty()
            && let Value::Object(obj) = &mut payload
        {
            obj.insert("result".to_string(), Value::String(last_result.to_string()));
        }
        if !error.is_empty()
            && let Value::Object(obj) = &mut payload
        {
            obj.insert("error".to_string(), Value::String(error.to_string()));
        }
        payload
    }
}

#[async_trait]
impl ToolSession for SubagentSession {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn capabilities(&self) -> HashSet<Cap> {
        let mut caps = HashSet::new();
        caps.insert(Cap::Wait);
        caps.insert(Cap::Checkout);
        caps.insert(Cap::Send);
        caps
    }

    async fn start(&mut self, _args: &ToolArgs) -> Result<Value> {
        let current_status = self.inner.status.lock().await.clone();
        if current_status != "created" {
            return Ok(self.snapshot(self.is_done(), None, None).await);
        }
        let context_size = {
            let guard = self.inner.context_manager.lock().await;
            match guard.as_ref() {
                Some(cm) => (cm.messages().len(), messages_size(cm.messages())),
                None => (0, 0),
            }
        };
        perf_log(
            "subagent_start",
            &json!({
                "subagent_run_id": self.session_id,
                "policy": self.policy,
                "context_messages": context_size.0,
                "context_chars": context_size.1,
                "task_chars": self.task.chars().count(),
            }),
        );
        let card_text = self.render_card().await;
        let _ = emit_start_card(
            &self.emit_parent_update,
            &self.parent_session_id,
            &self.session_id,
            &card_text,
        )
        .await;

        self.start_turn(self.task.clone()).await;
        Ok(self.snapshot(false, Some("subagent started"), None).await)
    }

    async fn wait(&mut self, timeout_secs: f64, _args: &ToolArgs) -> Result<Value> {
        let running = *self.inner.turn_running.lock().await;
        if running {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs_f64(timeout_secs.max(0.1))) => {
                    let running_now = *self.inner.turn_running.lock().await;
                    if running_now {
                        return Ok(self.snapshot(false, Some("subagent still running after timeout"), None).await);
                    }
                }
                _ = self.inner.turn_done.notified() => {}
            }
        }
        Ok(self.snapshot(self.is_done(), None, None).await)
    }

    async fn checkout(&mut self, _args: &ToolArgs) -> Result<Value> {
        Ok(self.snapshot(self.is_done(), None, None).await)
    }

    async fn send(&mut self, message: &str, interrupt: bool) -> Result<Value> {
        let text = message.trim().to_string();
        if text.is_empty() {
            return Ok(self
                .snapshot(self.is_done(), None, Some("message cannot be empty"))
                .await);
        }
        let status = self.inner.status.lock().await.clone();
        if status == "closed" {
            return Ok(self.snapshot(true, None, Some("subagent is closed")).await);
        }
        let running = *self.inner.turn_running.lock().await;
        if status == "running" || running {
            if !interrupt {
                return Ok(self
                    .snapshot(
                        false,
                        None,
                        Some("subagent is already running; set interrupt=true to replace the current turn"),
                    )
                    .await);
            }
            self.cancel_turn("cancelled").await;
        }

        {
            let mut status = self.inner.status.lock().await;
            *status = "running".to_string();
        }
        {
            let mut err = self.inner.error.lock().await;
            err.clear();
        }
        {
            let mut updated_at = self.inner.updated_at.lock().await;
            *updated_at = Some(Instant::now());
        }
        self.start_turn(text).await;
        let _ = emit_record_update(
            &self.emit_parent_update,
            &self.parent_session_id,
            &self.session_id,
            "in_progress",
            &self.render_card().await,
        )
        .await;
        Ok(self
            .snapshot(false, Some("subagent message sent"), None)
            .await)
    }

    async fn cancel(&mut self) -> Result<()> {
        let status = self.inner.status.lock().await.clone();
        if status == "closed" {
            return Ok(());
        }
        self.cancel_turn("closed").await;
        self.inner.tool_manager.ashutdown().await;
        Ok(())
    }

    fn is_done(&self) -> bool {
        match self.inner.status.try_lock() {
            Ok(guard) => !matches!(guard.as_str(), "created" | "running"),
            Err(_) => false,
        }
    }

    fn list_item(&self) -> Value {
        let status = self
            .inner
            .status
            .try_lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "running".to_string());
        json!({
            "id": self.session_id,
            "kind": "subagent",
            "status": status,
            "done": self.is_done(),
        })
    }
}

// ── Free helpers ───────────────────────────────────────────────────────────

async fn emit_start_card(
    emit: &UpdateEmitter,
    parent_session_id: &str,
    session_id: &str,
    rendered: &str,
) -> Result<()> {
    let title = format!("subagent:{session_id}");
    let mut event = ToolCallEvent::new(session_id, &title);
    event.status = "in_progress".to_string();
    event.content =
        Some(json!([{"type": "content", "content": {"type": "text", "text": rendered}}]));
    let obj = ActivityEvent::ToolCall(event).into_update();
    emit(parent_session_id.to_string(), obj).await
}

async fn emit_record_update(
    emit: &UpdateEmitter,
    parent_session_id: &str,
    session_id: &str,
    status: &str,
    rendered: &str,
) -> Result<()> {
    let title = format!("subagent:{session_id}");
    let mut event = ToolCallUpdateEvent::new(session_id, status);
    event.title = Some(title);
    event.content =
        Some(json!([{"type": "content", "content": {"type": "text", "text": rendered}}]));
    let obj = ActivityEvent::ToolCallUpdate(event).into_update();
    emit(parent_session_id.to_string(), obj).await
}

fn extract_last_assistant_response(messages: &[Value]) -> String {
    for message in messages.iter().rev() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some()
        {
            continue;
        }
        if let Some(text) = message.get("content").and_then(Value::as_str) {
            return text.trim().to_string();
        }
    }
    String::new()
}

#[allow(clippy::too_many_arguments)]
async fn run_subagent_turn(
    inner: &Arc<SubagentInner>,
    session_id: String,
    parent_session_id: String,
    message: String,
    policy: String,
    max_running_turn: Option<u32>,
    parent_cancel: CancelEvent,
    own_cancel: CancelEvent,
    emit_parent: UpdateEmitter,
    parent_request_permission: PermissionRequester,
    confirm_lock: Arc<Mutex<()>>,
) -> Result<String> {
    // Build the recording emit/permission wrappers that update the card.
    let card_for_emit = inner.clone();
    let parent_session_id_for_emit = parent_session_id.clone();
    let session_id_for_emit = session_id.clone();
    let emit_for_wrapper = emit_parent.clone();
    let recording_emit: UpdateEmitter =
        Arc::new(move |_target: String, update: Map<String, Value>| {
            let inner = card_for_emit.clone();
            let parent_session_id = parent_session_id_for_emit.clone();
            let session_id = session_id_for_emit.clone();
            let emit = emit_for_wrapper.clone();
            Box::pin(async move {
                let session_update = update
                    .get("session_update")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut should_emit_card = false;
                {
                    let mut card = inner.card.lock().await;
                    match session_update.as_str() {
                        s if s == EVENT_AGENT_THOUGHT_CHUNK => {
                            let text = extract_update_text(&update);
                            if !text.is_empty() {
                                card.append_thinking(&text);
                                should_emit_card = true;
                                bump_count(&inner.update_counts, "thought_chunk").await;
                            }
                        }
                        s if s == EVENT_AGENT_MESSAGE_CHUNK => {
                            let text = extract_update_text(&update);
                            if !text.is_empty() {
                                card.append_response(&text);
                                should_emit_card = true;
                                bump_count(&inner.update_counts, "message_chunk").await;
                            }
                        }
                        s if s == EVENT_TOOL_CALL => {
                            let title = update
                                .get("title")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string();
                            card.append_execution(&title);
                            should_emit_card = true;
                            bump_count(&inner.update_counts, "tool_call").await;
                        }
                        _ => {}
                    }
                }
                if should_emit_card {
                    let rendered = inner.card.lock().await.render();
                    emit_record_update(
                        &emit,
                        &parent_session_id,
                        &session_id,
                        "in_progress",
                        &rendered,
                    )
                    .await?;
                }
                Ok(())
            })
        });

    let emit_for_permission = emit_parent.clone();
    let inner_for_permission = inner.clone();
    let parent_session_id_for_permission = parent_session_id.clone();
    let session_id_for_permission = session_id.clone();
    let parent_request = parent_request_permission.clone();
    let recording_permission: PermissionRequester =
        Arc::new(move |_target: String, mut payload: Map<String, Value>| {
            let inner = inner_for_permission.clone();
            let parent_session_id = parent_session_id_for_permission.clone();
            let session_id = session_id_for_permission.clone();
            let emit = emit_for_permission.clone();
            let confirm_lock = confirm_lock.clone();
            let parent_request = parent_request.clone();
            Box::pin(async move {
                let _lock = confirm_lock.lock().await;
                let rendered = inner.card.lock().await.render();
                let _ = emit_record_update(
                    &emit,
                    &parent_session_id,
                    &session_id,
                    "pending",
                    &rendered,
                )
                .await;
                let original_title = payload
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                payload.insert(
                    "title".to_string(),
                    Value::String(format!("[subagent:{session_id}] {original_title}")),
                );
                parent_request(parent_session_id, payload).await
            })
        });

    let recording_state: StateSetter = Arc::new({
        let inner = inner.clone();
        let emit = emit_parent.clone();
        let parent_session_id = parent_session_id.clone();
        let session_id = session_id.clone();
        move |state: AgentState| {
            let inner = inner.clone();
            let emit = emit.clone();
            let parent_session_id = parent_session_id.clone();
            let session_id = session_id.clone();
            tokio::spawn(async move {
                let status = if state == AgentState::WaitingUserConfirm {
                    "pending"
                } else {
                    "in_progress"
                };
                let rendered = inner.card.lock().await.render();
                let _ =
                    emit_record_update(&emit, &parent_session_id, &session_id, status, &rendered)
                        .await;
            });
        }
    });

    // Choose which cancel event governs the turn — listen to parent + own.
    // We model this by spawning a relay: parent cancel → own cancel.
    let parent_cancel_clone = parent_cancel.clone();
    let own_cancel_for_relay = own_cancel.clone();
    tokio::spawn(async move {
        parent_cancel_clone.wait_cancelled().await;
        own_cancel_for_relay.set();
    });

    let mut ctx_guard = inner.context_manager.lock().await;
    let context_manager = ctx_guard
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("subagent context_manager missing"))?;
    let mut client_guard = inner.model_client.lock().await;
    let model_client = client_guard
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("subagent model_client missing"))?;
    let reasoning_mode = model_client.default_reasoning_mode.clone();
    let get_policy_mode: PolicyModeGetter = Arc::new({
        let policy = policy.clone();
        move || {
            let policy = policy.clone();
            Box::pin(async move { policy })
        }
    });

    let activity = ActivityTurnHandle::transient(
        parent_session_id.clone(),
        recording_emit,
        recording_permission,
        recording_state,
        own_cancel.clone(),
    );
    let turn_runtime = TurnRuntime {
        user_input: Value::String(message.clone()),
        get_policy_mode,
        max_running_turn,
        activity,
        reasoning_mode,
        model_client,
        tool_schemas: inner.tool_schemas.clone(),
        tool_manager: &inner.tool_manager,
        context_manager,
        rebuild_system_messages: None,
    };
    let turn_result = run_turn(turn_runtime).await?;
    Ok(turn_result.stop_reason)
}

fn extract_update_text(update: &Map<String, Value>) -> String {
    let Some(content) = update.get("content").and_then(Value::as_object) else {
        return String::new();
    };
    if content.get("type").and_then(Value::as_str) != Some("text") {
        return String::new();
    }
    content
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

async fn bump_count(counts: &Mutex<HashMap<String, u64>>, key: &str) {
    let mut guard = counts.lock().await;
    *guard.entry(key.to_string()).or_insert(0) += 1;
}
