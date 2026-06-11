//! Session-level activity stream management.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use self::event::{
    ActivityBoxEvent, ActivityEvent, EVENT_TOOL_CALL_UPDATE, ToolCallEvent, ToolCallUpdateEvent,
    tool_permission_payload, update_type,
};
use super::session::{Session, SessionPersistence};
use crate::config::loader::utc_iso;
use crate::config::models::{AgentState, SessionTranscriptEvent};
use crate::context::manager::CancelEvent;
use crate::llm::client::{
    LlmCancelToken, LlmRequestOptions, LlmRetryCallback, LlmRetryEvent, LlmRetryKind,
    StreamChunkCallback,
};
use crate::tools::subagent_tool_runtime::{
    PermissionRequester, StateSetter, ToolExecutionContext, UpdateEmitter,
};

pub mod event;

#[derive(Clone)]
pub struct SessionActivity {
    inner: Arc<SessionActivityInner>,
}

struct SessionActivityInner {
    session_id: String,
    session: Option<Arc<Mutex<Session>>>,
    persistence: Option<Arc<SessionPersistence>>,
}

#[derive(Clone)]
pub struct ActivityTurnHandle {
    activity: SessionActivity,
    emit_update: UpdateEmitter,
    request_permission: PermissionRequester,
    set_state: StateSetter,
    cancel_event: CancelEvent,
}

pub struct AssistantStreamProbes {
    pub on_text_chunk: StreamChunkCallback,
    pub on_reasoning_chunk: StreamChunkCallback,
    pub options: LlmRequestOptions,
    pub retry_box: LazyActivityBox,
}

#[derive(Clone)]
pub struct LazyActivityBox {
    handle: ActivityTurnHandle,
    activity_id: String,
    title: String,
    started: Arc<Mutex<bool>>,
}

impl SessionActivity {
    pub fn new(
        session_id: String,
        session: Arc<Mutex<Session>>,
        persistence: Arc<SessionPersistence>,
    ) -> Self {
        Self {
            inner: Arc::new(SessionActivityInner {
                session_id,
                session: Some(session),
                persistence: Some(persistence),
            }),
        }
    }

    pub fn transient(session_id: String) -> Self {
        Self {
            inner: Arc::new(SessionActivityInner {
                session_id,
                session: None,
                persistence: None,
            }),
        }
    }

    pub fn bind_turn(
        &self,
        emit_update: UpdateEmitter,
        request_permission: PermissionRequester,
        cancel_event: CancelEvent,
    ) -> ActivityTurnHandle {
        let activity = self.clone();
        let set_state: StateSetter = Arc::new(move |state| {
            let activity = activity.clone();
            tokio::spawn(async move {
                let _ = activity.set_state(state).await;
            });
        });
        ActivityTurnHandle {
            activity: self.clone(),
            emit_update,
            request_permission,
            set_state,
            cancel_event,
        }
    }

    pub async fn record_user_input(&self, user_input: &Value, user_blocks: &[Value]) -> Result<()> {
        match user_input {
            Value::String(text) => {
                self.record_event(ActivityEvent::user_message_text(text))
                    .await?;
            }
            _ => {
                for block in user_blocks {
                    let Value::Object(block_obj) = block else {
                        continue;
                    };
                    self.record_event(ActivityEvent::user_message_content(block_obj))
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn set_state(&self, state: AgentState) -> Result<()> {
        let (Some(session), Some(persistence)) = (&self.inner.session, &self.inner.persistence)
        else {
            return Ok(());
        };
        {
            let mut session_guard = session.lock().await;
            session_guard.state = state;
            session_guard.updated_at = Some(utc_iso());
            persistence.save_session_meta(&session_guard)?;
        }
        Ok(())
    }

    async fn record_event(&self, event: ActivityEvent) -> Result<()> {
        self.record_update(event.into_update()).await
    }

    async fn record_update(&self, update: Map<String, Value>) -> Result<()> {
        let Some(persistence) = &self.inner.persistence else {
            return Ok(());
        };
        let event = SessionTranscriptEvent::new(utc_iso(), update)?;
        let payload = serde_json::to_value(&event)?;
        persistence.append_transcript_event(&payload)?;
        Ok(())
    }

    fn session_id(&self) -> &str {
        &self.inner.session_id
    }
}

impl ActivityTurnHandle {
    pub fn transient(
        session_id: String,
        emit_update: UpdateEmitter,
        request_permission: PermissionRequester,
        set_state: StateSetter,
        cancel_event: CancelEvent,
    ) -> Self {
        Self {
            activity: SessionActivity::transient(session_id),
            emit_update,
            request_permission,
            set_state,
            cancel_event,
        }
    }

    pub fn session_id(&self) -> &str {
        self.activity.session_id()
    }

    pub fn cancel_event(&self) -> &CancelEvent {
        &self.cancel_event
    }

    pub async fn emit_event(&self, event: ActivityEvent) -> Result<()> {
        self.emit_update_map(event.into_update()).await
    }

    pub async fn emit_event_unrecorded(&self, event: ActivityEvent) -> Result<()> {
        let update = event.into_update();
        (self.emit_update)(self.session_id().to_string(), update).await
    }

    pub async fn emit_update_map(&self, update: Map<String, Value>) -> Result<()> {
        if should_persist_update(&update) {
            self.activity.record_update(update.clone()).await?;
        }
        (self.emit_update)(self.session_id().to_string(), update).await
    }

    pub fn update_emitter(&self) -> UpdateEmitter {
        let handle = self.clone();
        Arc::new(move |_target: String, update: Map<String, Value>| {
            let handle = handle.clone();
            Box::pin(async move { handle.emit_update_map(update).await })
        })
    }

    pub fn permission_requester(&self) -> PermissionRequester {
        self.request_permission.clone()
    }

    pub fn state_setter(&self) -> StateSetter {
        self.set_state.clone()
    }

    pub fn tool_execution_context(&self, mode_id: String) -> ToolExecutionContext {
        ToolExecutionContext {
            session_id: self.session_id().to_string(),
            tool_call_id: String::new(),
            mode_id,
            cancel_event: self.cancel_event.clone(),
            emit_update: self.update_emitter(),
            request_permission: self.permission_requester(),
            set_state: self.state_setter(),
        }
    }

    pub async fn agent_message_chunk(&self, chunk: &str) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        self.emit_event(ActivityEvent::agent_message_text(chunk))
            .await
    }

    pub async fn agent_thought_chunk(&self, chunk: &str) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        self.emit_event(ActivityEvent::agent_thought_text(chunk))
            .await
    }

    pub async fn tool_call_pending(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        raw_input: Value,
    ) -> Result<()> {
        let mut event = ToolCallEvent::new(tool_call_id, tool_name);
        event.raw_input = Some(raw_input);
        self.emit_event(ActivityEvent::ToolCall(event)).await
    }

    pub async fn tool_call_update(
        &self,
        tool_call_id: &str,
        status: &str,
        title: Option<&str>,
        kind: Option<&str>,
        raw_input: Option<Value>,
        raw_output: Option<Value>,
        content: Option<Value>,
    ) -> Result<()> {
        let mut event = ToolCallUpdateEvent::new(tool_call_id, status);
        event.title = title.map(str::to_string);
        event.kind = kind.map(str::to_string);
        event.raw_input = raw_input;
        event.raw_output = raw_output;
        event.content = content;
        self.emit_event(ActivityEvent::ToolCallUpdate(event)).await
    }

    pub async fn current_mode_update(&self, mode_id: &str) -> Result<()> {
        self.emit_event(ActivityEvent::CurrentModeUpdate {
            mode_id: mode_id.to_string(),
        })
        .await
    }

    pub async fn session_info_update(&self, title: &str, updated_at: Option<&str>) -> Result<()> {
        self.emit_event_unrecorded(ActivityEvent::SessionInfoUpdate {
            title: title.to_string(),
            updated_at: updated_at.map(str::to_string),
        })
        .await
    }

    pub async fn request_tool_permission(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        tool_args: &Map<String, Value>,
    ) -> Result<String> {
        (self.set_state)(AgentState::WaitingUserConfirm);
        let payload = tool_permission_payload(tool_call_id, tool_name, tool_args);
        let request_permission = self.request_permission.clone();
        let session_id = self.session_id().to_string();
        let cancel_event = self.cancel_event.clone();
        let decision = tokio::select! {
            result = request_permission(session_id, payload) => result,
            _ = cancel_event.wait_cancelled() => Ok(crate::agent::constants::PERMISSION_CANCELLED.to_string()),
        };
        if !self.cancel_event.is_set() {
            (self.set_state)(AgentState::Running);
        }
        decision
    }

    pub fn activity_box(&self, title: &str) -> LazyActivityBox {
        LazyActivityBox {
            handle: self.clone(),
            activity_id: format!("runtime:activity:{}", uuid::Uuid::new_v4()),
            title: title.to_string(),
            started: Arc::new(Mutex::new(false)),
        }
    }

    pub fn assistant_stream_probes(&self) -> AssistantStreamProbes {
        let retry_box = self.activity_box("Model response");
        AssistantStreamProbes {
            on_text_chunk: self.model_chunk_callback(ModelChunkKind::Message),
            on_reasoning_chunk: self.model_chunk_callback(ModelChunkKind::Thought),
            options: self.llm_request_options(Some(retry_box.retry_callback())),
            retry_box,
        }
    }

    pub fn llm_request_options(&self, on_retry: Option<LlmRetryCallback>) -> LlmRequestOptions {
        LlmRequestOptions {
            retry: Default::default(),
            cancel: Some(self.llm_cancel_token()),
            on_retry,
        }
    }

    fn llm_cancel_token(&self) -> LlmCancelToken {
        let cancel_for_check = self.cancel_event.clone();
        let cancel_for_wait = self.cancel_event.clone();
        LlmCancelToken::new(
            move || cancel_for_check.is_set(),
            move || {
                let cancel = cancel_for_wait.clone();
                async move { cancel.wait_cancelled().await }
            },
        )
    }

    fn model_chunk_callback(&self, kind: ModelChunkKind) -> StreamChunkCallback {
        let handle = self.clone();
        Arc::new(move |chunk: String| {
            let handle = handle.clone();
            Box::pin(async move {
                if handle.cancel_event.is_set() {
                    anyhow::bail!("turn cancelled");
                }
                match kind {
                    ModelChunkKind::Message => handle.agent_message_chunk(&chunk).await,
                    ModelChunkKind::Thought => handle.agent_thought_chunk(&chunk).await,
                }
            })
        })
    }
}

impl LazyActivityBox {
    pub async fn start_or_update(&self, status: &str, text: &str) -> Result<()> {
        let mut started_guard = self.started.lock().await;
        let event = ActivityBoxEvent::new(&self.activity_id, &self.title, status, text);
        let payload = if *started_guard {
            ActivityEvent::ActivityBoxUpdate(event)
        } else {
            *started_guard = true;
            ActivityEvent::ActivityBox(event)
        };
        drop(started_guard);
        self.handle.emit_event(payload).await
    }

    pub async fn update_if_started(&self, status: &str, text: &str) -> Result<()> {
        if !*self.started.lock().await {
            return Ok(());
        }
        let event = ActivityBoxEvent::new(&self.activity_id, &self.title, status, text);
        self.handle
            .emit_event(ActivityEvent::ActivityBoxUpdate(event))
            .await
    }

    pub async fn complete_if_started(&self, text: &str) -> Result<()> {
        self.update_if_started("completed", text).await
    }

    pub async fn fail_if_started(&self, text: &str) -> Result<()> {
        self.update_if_started("failed", text).await
    }

    pub fn retry_callback(&self) -> LlmRetryCallback {
        let activity_box = self.clone();
        Arc::new(move |event: LlmRetryEvent| {
            let activity_box = activity_box.clone();
            Box::pin(async move {
                let text = retry_event_text(&event);
                activity_box.start_or_update("in_progress", &text).await
            }) as Pin<Box<dyn Future<Output = Result<()>> + Send>>
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum ModelChunkKind {
    Message,
    Thought,
}

fn retry_event_text(event: &LlmRetryEvent) -> String {
    let delay_ms = event.delay.as_millis();
    match event.kind {
        LlmRetryKind::Request => format!(
            "Model request failed before the stream opened. Retrying {}/{} in {}ms. {}",
            event.attempt, event.max_retries, delay_ms, event.error
        ),
        LlmRetryKind::Stream => format!(
            "Model stream interrupted. Reconnecting {}/{} in {}ms. {}",
            event.attempt, event.max_retries, delay_ms, event.error
        ),
    }
}

fn should_persist_update(update: &Map<String, Value>) -> bool {
    let update_type = update_type(update);
    if update_type == EVENT_TOOL_CALL_UPDATE {
        let tool_call_id = update
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let title = update.get("title").and_then(Value::as_str).unwrap_or("");
        if !tool_call_id.is_empty() && title.starts_with("subagent:") {
            let status = update.get("status").and_then(Value::as_str).unwrap_or("");
            return matches!(status, "completed" | "failed" | "cancelled");
        }
    }
    true
}
