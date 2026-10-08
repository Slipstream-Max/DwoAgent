use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use dwo_context::{
    ContentBlock, ContextManager, ContextMessage, MessageContent, MessageKind,
    PendingContextMessage, SessionContext, SystemPromptBuilder,
};
use dwo_model_client::{ModelClient, ModelClientError, ModelReply, ModelSelection};
use dwo_tools::{ConfirmationDecision, TodoAction, TodoRequest, TodoResponse, ToolManager};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::TurnId;
use crate::compaction::{self, CompactionRequest, CompactionResult};
use crate::error::SessionServiceError;
use crate::events::{
    ActiveStepSnapshot, ActiveToolCall, ClientTranscriptEvent, CompactionTrigger, FileChange,
    NotificationLevel, RuntimePhase, SessionEvent, SessionEventPayload, SessionNotification,
    SessionSnapshot, SessionSubscription, SessionUsageSnapshot, TerminalView, project_tool_call,
};
use crate::permission::{PermissionRequestEnvelope, PermissionRequester, PermissionState};
use crate::repository::SessionRepository;
use crate::session_record::{
    ExecutionPlan, SessionConfig, SessionConfigUpdate, SessionRecord, SessionUpdate,
    SessionWorkspace, title_from_user_content,
};
use crate::session_service::SessionDeletionHook;
use crate::turn::{self, TurnExecution, TurnOutcome, TurnUpdate};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EndpointId(String);

const EPHEMERAL_GRACE_MS: u64 = 5 * 60 * 1000;

impl EndpointId {
    pub fn new() -> Self {
        Self(format!("endpoint-{}", Uuid::new_v4()))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err("endpoint id must not be empty".to_string());
        }
        Ok(Self(value))
    }
}

impl Default for EndpointId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for EndpointId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(String);

impl MessageId {
    pub fn new() -> Self {
        Self(format!("message-{}", Uuid::new_v4()))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err("message id must not be empty".to_string());
        }
        Ok(Self(value))
    }
}

impl Default for MessageId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptAccepted {
    pub message_id: MessageId,
    pub turn_id: TurnId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionAccepted {
    pub compaction_id: String,
}

enum PromptInput {
    User {
        origin: EndpointId,
        content: MessageContent,
    },
    Internal {
        content: MessageContent,
    },
}

struct PromptReceipt {
    turn_id: TurnId,
    message_id: Option<MessageId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActorControl {
    Continue,
    Stop,
}

pub(crate) enum ActorEvent {
    Turn(TurnUpdate),
    PersistContext {
        context: Box<SessionContext>,
        completed: oneshot::Sender<anyhow::Result<()>>,
    },
    Todo {
        turn_id: TurnId,
        request: TodoRequest,
        completed: oneshot::Sender<Result<TodoResponse, String>>,
    },
    CompactionFinished {
        compaction_id: String,
        result: Box<Result<CompactionResult, String>>,
    },
    PermissionRequested(PermissionRequestEnvelope),
    TitleGenerated {
        original_title: String,
        result: Result<ModelReply, ModelClientError>,
    },
    Expired,
}

pub struct SessionHandle {
    id: crate::SessionId,
    request: mpsc::Sender<SessionRequest>,
    terminated: Arc<std::sync::atomic::AtomicBool>,
}

impl SessionHandle {
    pub(crate) async fn snapshot(&self) -> Result<SessionSnapshot, SessionServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(SessionRequest::Snapshot { response }).await?;
        wait.await
            .map_err(|_| SessionServiceError::SessionClosed(self.id.clone()))
    }

    pub(crate) async fn subscribe(
        &self,
        checkpoint_cursor: Option<usize>,
    ) -> Result<SessionSubscription, SessionServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(SessionRequest::Subscribe {
            checkpoint_cursor,
            response,
        })
        .await?;
        let (snapshot, source) = wait
            .await
            .map_err(|_| SessionServiceError::SessionClosed(self.id.clone()))?;
        Ok(crate::events::subscription(
            snapshot,
            source,
            self.id.clone(),
        ))
    }

    pub(crate) async fn prompt(
        &self,
        origin: EndpointId,
        content: impl Into<MessageContent>,
    ) -> Result<PromptAccepted, SessionServiceError> {
        let content = content.into();
        let (response, wait) = oneshot::channel();
        self.send(SessionRequest::Prompt {
            input: PromptInput::User { origin, content },
            response,
        })
        .await?;
        let receipt = wait
            .await
            .map_err(|_| SessionServiceError::SessionClosed(self.id.clone()))??;
        Ok(PromptAccepted {
            message_id: receipt
                .message_id
                .expect("user prompt acceptance includes a message id"),
            turn_id: receipt.turn_id,
        })
    }

    pub(crate) async fn compact(
        &self,
        origin: EndpointId,
    ) -> Result<CompactionAccepted, SessionServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(SessionRequest::Compact { origin, response })
            .await?;
        wait.await
            .map_err(|_| SessionServiceError::SessionClosed(self.id.clone()))?
    }

    pub(crate) async fn cancel(
        &self,
        expected_turn_id: Option<TurnId>,
    ) -> Result<(), SessionServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(SessionRequest::Cancel {
            expected_turn_id,
            response,
        })
        .await?;
        wait.await
            .map_err(|_| SessionServiceError::SessionClosed(self.id.clone()))?
    }

    pub(crate) async fn refresh_config_options(&self) -> Result<(), SessionServiceError> {
        self.send(SessionRequest::RefreshConfigOptions).await
    }

    pub(crate) async fn set_config(
        &self,
        update: SessionConfigUpdate,
    ) -> Result<(), SessionServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(SessionRequest::SetConfig { update, response })
            .await?;
        wait.await
            .map_err(|_| SessionServiceError::SessionClosed(self.id.clone()))?
    }

    pub(crate) async fn set(&self, update: SessionUpdate) -> Result<(), SessionServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(SessionRequest::Set { update, response }).await?;
        wait.await
            .map_err(|_| SessionServiceError::SessionClosed(self.id.clone()))?
    }

    pub(crate) async fn set_workspace(
        &self,
        workspace: SessionWorkspace,
        cwd: std::path::PathBuf,
        tools: Arc<ToolManager>,
        prompt_builder: SystemPromptBuilder,
    ) -> Result<(), SessionServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(SessionRequest::SetWorkspace {
            workspace,
            cwd,
            tools,
            prompt_builder: Box::new(prompt_builder),
            response,
        })
        .await?;
        wait.await
            .map_err(|_| SessionServiceError::SessionClosed(self.id.clone()))?
    }

    pub(crate) async fn keep(&self) -> Result<bool, SessionServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(SessionRequest::Keep { response }).await?;
        wait.await
            .map_err(|_| SessionServiceError::SessionClosed(self.id.clone()))?
    }

    pub(crate) async fn prompt_internal(
        &self,
        content: impl Into<MessageContent>,
    ) -> Result<TurnId, SessionServiceError> {
        let content = content.into();
        let (response, wait) = oneshot::channel();
        self.send(SessionRequest::Prompt {
            input: PromptInput::Internal { content },
            response,
        })
        .await?;
        let receipt = wait
            .await
            .map_err(|_| SessionServiceError::SessionClosed(self.id.clone()))??;
        Ok(receipt.turn_id)
    }

    pub(crate) async fn publish_notification(
        &self,
        notification: SessionNotification,
    ) -> Result<MessageId, SessionServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(SessionRequest::PublishNotification {
            notification,
            response,
        })
        .await?;
        wait.await
            .map_err(|_| SessionServiceError::SessionClosed(self.id.clone()))?
    }

    pub(crate) async fn respond_permission(
        &self,
        origin: EndpointId,
        request_id: String,
        decision: ConfirmationDecision,
    ) -> Result<(), SessionServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(SessionRequest::RespondPermission {
            origin,
            request_id,
            decision,
            response,
        })
        .await?;
        wait.await
            .map_err(|_| SessionServiceError::SessionClosed(self.id.clone()))?
    }

    pub(crate) async fn unload(&self) -> Result<(), SessionServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(SessionRequest::Unload { response }).await?;
        wait.await
            .map_err(|_| SessionServiceError::SessionClosed(self.id.clone()))?
    }

    async fn send(&self, request: SessionRequest) -> Result<(), SessionServiceError> {
        self.request
            .send(request)
            .await
            .map_err(|_| SessionServiceError::SessionClosed(self.id.clone()))
    }

    pub(crate) fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::Acquire)
    }
}

enum SessionRequest {
    RefreshConfigOptions,
    Subscribe {
        checkpoint_cursor: Option<usize>,
        response: oneshot::Sender<(SessionSnapshot, broadcast::Receiver<SessionEvent>)>,
    },
    Snapshot {
        response: oneshot::Sender<SessionSnapshot>,
    },
    Prompt {
        input: PromptInput,
        response: oneshot::Sender<Result<PromptReceipt, SessionServiceError>>,
    },
    Compact {
        origin: EndpointId,
        response: oneshot::Sender<Result<CompactionAccepted, SessionServiceError>>,
    },
    Cancel {
        expected_turn_id: Option<TurnId>,
        response: oneshot::Sender<Result<(), SessionServiceError>>,
    },
    SetConfig {
        update: SessionConfigUpdate,
        response: oneshot::Sender<Result<(), SessionServiceError>>,
    },
    Set {
        update: SessionUpdate,
        response: oneshot::Sender<Result<(), SessionServiceError>>,
    },
    SetWorkspace {
        workspace: SessionWorkspace,
        cwd: std::path::PathBuf,
        tools: Arc<ToolManager>,
        prompt_builder: Box<SystemPromptBuilder>,
        response: oneshot::Sender<Result<(), SessionServiceError>>,
    },
    Keep {
        response: oneshot::Sender<Result<bool, SessionServiceError>>,
    },
    PublishNotification {
        notification: SessionNotification,
        response: oneshot::Sender<Result<MessageId, SessionServiceError>>,
    },
    RespondPermission {
        origin: EndpointId,
        request_id: String,
        decision: ConfirmationDecision,
        response: oneshot::Sender<Result<(), SessionServiceError>>,
    },
    Unload {
        response: oneshot::Sender<Result<(), SessionServiceError>>,
    },
}

struct ActiveTurnState {
    id: TurnId,
    cancellation: CancellationToken,
    partial_message: String,
    partial_reasoning: String,
    step_id: u64,
    step_revision: u64,
    message_id: MessageId,
    thought_message_id: MessageId,
    tools: Vec<ActiveToolCall>,
    steer: mpsc::UnboundedSender<PendingContextMessage>,
}

struct ActiveCompaction {
    id: String,
    cancellation: CancellationToken,
    origin: EndpointId,
}

const TERMINAL_VIEW_BYTES: usize = 256 * 1024;
const TERMINAL_VIEWS_TOTAL_BYTES: usize = 1024 * 1024;
const TERMINAL_VIEW_COUNT: usize = 64;
const TERMINAL_VIEW_METADATA_BYTES: usize = 16 * 1024;
const TERMINAL_VIEWS_METADATA_BYTES: usize = 64 * 1024;

/// Owned exclusively by the actor, so retention and subscription use the same
/// ordering. Reading TerminalManager here would race queued telemetry and could
/// include bytes newer than snapshot.seq (and its accessor renders lossy text).
#[derive(Default)]
struct TerminalViewCache {
    // Least recently updated first. Keep metadata even when its bytes are trimmed.
    views: VecDeque<CachedTerminalView>,
}

struct CachedTerminalView {
    terminal_id: String,
    tool_call_id: String,
    command: String,
    cwd: std::path::PathBuf,
    output: VecDeque<u8>,
    dropped_bytes: u64,
    status: String,
    exit_code: Option<i32>,
}

impl TerminalViewCache {
    fn observe(&mut self, event: &dwo_tools::ToolEvent) {
        use dwo_tools::ToolEvent;
        match event {
            ToolEvent::TerminalOpened {
                tool_call_id,
                terminal_id,
                command,
                cwd,
            } => {
                if self
                    .views
                    .iter()
                    .any(|view| view.terminal_id == *terminal_id)
                {
                    return;
                }
                // Bound metadata too; opaque IDs and paths must not be truncated.
                // Oversized entries remain available through transcript fallback.
                let metadata_bytes = terminal_id
                    .len()
                    .saturating_add(tool_call_id.len())
                    .saturating_add(command.len())
                    .saturating_add(cwd.as_os_str().len());
                if metadata_bytes > TERMINAL_VIEW_METADATA_BYTES {
                    return;
                }
                while self.views.len() == TERMINAL_VIEW_COUNT
                    || self
                        .views
                        .iter()
                        .map(|view| {
                            view.terminal_id.len()
                                + view.tool_call_id.len()
                                + view.command.len()
                                + view.cwd.as_os_str().len()
                        })
                        .sum::<usize>()
                        + metadata_bytes
                        > TERMINAL_VIEWS_METADATA_BYTES
                {
                    let index = self
                        .views
                        .iter()
                        .position(|view| view.status != "running")
                        .unwrap_or(0);
                    self.views.remove(index);
                }
                self.views.push_back(CachedTerminalView {
                    terminal_id: terminal_id.clone(),
                    tool_call_id: tool_call_id.clone(),
                    command: command.clone(),
                    cwd: cwd.clone(),
                    output: VecDeque::new(),
                    dropped_bytes: 0,
                    status: "running".into(),
                    exit_code: None,
                });
            }
            ToolEvent::TerminalOutput { terminal_id, data } => {
                let Some(index) = self
                    .views
                    .iter()
                    .position(|view| view.terminal_id == *terminal_id)
                else {
                    return;
                };
                let mut view = self.views.remove(index).expect("located terminal view");
                // Trim before extending: a single oversized chunk must not allocate
                // beyond the per-terminal budget. Do not align or decode UTF-8/ANSI.
                let incoming = data.len().min(TERMINAL_VIEW_BYTES);
                let remove = (view.output.len() + incoming).saturating_sub(TERMINAL_VIEW_BYTES);
                Self::trim(&mut view, remove);
                view.dropped_bytes = view
                    .dropped_bytes
                    .saturating_add((data.len() - incoming) as u64);
                view.output.extend(&data[data.len() - incoming..]);
                self.views.push_back(view);
                let mut excess = self
                    .views
                    .iter()
                    .map(|view| view.output.len())
                    .sum::<usize>()
                    .saturating_sub(TERMINAL_VIEWS_TOTAL_BYTES);
                for view in &mut self.views {
                    if excess == 0 {
                        break;
                    }
                    let remove = excess.min(view.output.len());
                    Self::trim(view, remove);
                    view.output.shrink_to_fit();
                    excess -= remove;
                }
            }
            ToolEvent::TerminalExited {
                terminal_id,
                exit_code,
                status,
            } => {
                if let Some(index) = self
                    .views
                    .iter()
                    .position(|view| view.terminal_id == *terminal_id)
                {
                    let mut view = self.views.remove(index).expect("located terminal view");
                    view.exit_code = *exit_code;
                    view.status = status.chars().take(64).collect();
                    self.views.push_back(view);
                }
            }
            _ => {}
        }
    }

    fn trim(view: &mut CachedTerminalView, count: usize) {
        view.output.drain(..count);
        view.dropped_bytes = view.dropped_bytes.saturating_add(count as u64);
    }

    fn snapshot(&self) -> Vec<TerminalView> {
        self.views
            .iter()
            .map(|view| TerminalView {
                terminal_id: view.terminal_id.clone(),
                tool_call_id: view.tool_call_id.clone(),
                command: view.command.clone(),
                cwd: view.cwd.clone(),
                output: encode_terminal_bytes(&view.output),
                dropped_bytes: view.dropped_bytes,
                status: view.status.clone(),
                exit_code: view.exit_code,
            })
            .collect()
    }
}

fn encode_terminal_bytes(bytes: &VecDeque<u8>) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes.iter().copied().collect::<Vec<_>>())
}

pub(crate) struct SessionActor {
    record: SessionRecord,
    transcript: Vec<ClientTranscriptEvent>,
    repository: Arc<dyn SessionRepository>,
    model: Arc<dyn ModelClient>,
    tools: Arc<ToolManager>,
    prompt_builder: SystemPromptBuilder,
    requests: mpsc::Receiver<SessionRequest>,
    actor_tx: mpsc::UnboundedSender<ActorEvent>,
    actor_events: mpsc::UnboundedReceiver<ActorEvent>,
    expiry_timer: Option<CancellationToken>,
    config_tx: watch::Sender<SessionConfig>,
    events: broadcast::Sender<SessionEvent>,
    seq: u64,
    terminal_views: TerminalViewCache,
    /// Terminal ids this daemon process can still reach. Fed by the same ordered
    /// telemetry stream as the view cache; empty until terminals open.
    live_terminals: HashSet<String>,
    phase: RuntimePhase,
    active: Option<ActiveTurnState>,
    active_compaction: Option<ActiveCompaction>,
    permission: PermissionState,
    closing_response: Option<oneshot::Sender<Result<(), SessionServiceError>>>,
    title_cancellation: Option<CancellationToken>,
    deletion_hook: Option<SessionDeletionHook>,
    terminated: Arc<std::sync::atomic::AtomicBool>,
}

pub(crate) struct SessionActorDependencies {
    pub(crate) repository: Arc<dyn SessionRepository>,
    pub(crate) model: Arc<dyn ModelClient>,
    pub(crate) tools: Arc<ToolManager>,
    pub(crate) prompt_builder: SystemPromptBuilder,
    pub(crate) deletion_hook: Option<SessionDeletionHook>,
}

impl SessionActor {
    pub(crate) fn spawn(
        record: SessionRecord,
        transcript: Vec<ClientTranscriptEvent>,
        dependencies: SessionActorDependencies,
    ) -> Arc<SessionHandle> {
        let SessionActorDependencies {
            repository,
            model,
            tools,
            prompt_builder,
            deletion_hook,
        } = dependencies;
        let id = record.info.id.clone();
        let (request_tx, request_rx) = mpsc::channel(128);
        let (actor_tx, actor_rx) = mpsc::unbounded_channel();
        let (config_tx, _) = watch::channel(record.config());
        let (events, _) = broadcast::channel(1024);
        let terminated = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut actor = Self {
            record,
            transcript,
            repository,
            model,
            tools,
            prompt_builder,
            deletion_hook,
            requests: request_rx,
            actor_tx,
            actor_events: actor_rx,
            expiry_timer: None,
            config_tx,
            events,
            seq: 0,
            terminal_views: TerminalViewCache::default(),
            live_terminals: HashSet::new(),
            phase: RuntimePhase::Idle,
            active: None,
            active_compaction: None,
            permission: PermissionState::default(),
            closing_response: None,
            title_cancellation: None,
            terminated: terminated.clone(),
        };
        actor.reset_expiry(actor.record.info.delete_after_ms);
        tokio::spawn(actor.run());
        Arc::new(SessionHandle {
            id,
            request: request_tx,
            terminated,
        })
    }

    async fn run(mut self) {
        loop {
            let control = tokio::select! {
                biased;
                request = self.requests.recv() => {
                    match request {
                        Some(request) => self.handle_request(request).await,
                        None => ActorControl::Stop,
                    }
                }
                event = self.actor_events.recv() => {
                    match event {
                        Some(event) => self.handle_event(event).await,
                        None => ActorControl::Stop,
                    }
                }
            };
            if control == ActorControl::Stop {
                break;
            }
        }
        if let Some(active) = self.active.take() {
            active.cancellation.cancel();
        }
        if let Some(active) = self.active_compaction.take() {
            active.cancellation.cancel();
        }
        if let Some(cancellation) = self.title_cancellation.take() {
            cancellation.cancel();
        }
        self.permission.reject("session closed");
        self.tools.shutdown().await;
        if let Some(timer) = self.expiry_timer.take() {
            timer.cancel();
        }
        self.terminated.store(true, Ordering::Release);
    }

    fn reset_expiry(&mut self, deadline_ms: Option<u64>) {
        if let Some(timer) = self.expiry_timer.take() {
            timer.cancel();
        }
        let Some(deadline_ms) = deadline_ms else {
            return;
        };
        let cancellation = CancellationToken::new();
        let child = cancellation.clone();
        let sender = self.actor_tx.clone();
        let now = current_time_ms();
        let delay = Duration::from_millis(deadline_ms.saturating_sub(now));
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(delay) => { let _ = sender.send(ActorEvent::Expired); }
                _ = child.cancelled() => {}
            }
        });
        self.expiry_timer = Some(cancellation);
    }

    async fn expire(&mut self) -> ActorControl {
        self.expiry_timer = None;
        let Some(deadline) = self.record.info.delete_after_ms else {
            return ActorControl::Continue;
        };
        if !self.record.info.ephemeral || deadline > current_time_ms() {
            self.reset_expiry(self.record.info.delete_after_ms);
            return ActorControl::Continue;
        }
        self.phase = RuntimePhase::Closing;
        self.permission.reject("ephemeral session expired");
        match self.repository.delete(&self.record.info.id).await {
            Ok(true) => {
                if let Some(hook) = &self.deletion_hook {
                    hook(&self.record.info.id, &self.record.info.workspace);
                }
            }
            Ok(false) => {}
            Err(error) => {
                tracing::error!(
                    event = "session.ephemeral_delete_failed",
                    session_id = %self.record.info.id,
                    error = %error,
                    "delete expired ephemeral session failed"
                );
            }
        }
        self.tools.shutdown().await;
        ActorControl::Stop
    }

    async fn handle_request(&mut self, request: SessionRequest) -> ActorControl {
        match request {
            SessionRequest::Subscribe {
                checkpoint_cursor,
                response,
            } => {
                let receiver = self.events.subscribe();
                let mut snapshot = self.snapshot();
                if let Some(cursor) = checkpoint_cursor {
                    let cursor = cursor.min(snapshot.transcript.len());
                    snapshot.transcript.drain(..cursor);
                }
                let _ = response.send((snapshot, receiver));
            }
            SessionRequest::Snapshot { response } => {
                let _ = response.send(self.snapshot());
            }
            SessionRequest::Prompt { input, response } => {
                let _ = response.send(self.prompt(input).await);
            }
            SessionRequest::Compact { origin, response } => {
                let _ = response.send(self.compact(origin).await);
            }
            SessionRequest::Cancel {
                expected_turn_id,
                response,
            } => {
                let _ = response.send(self.cancel(expected_turn_id));
            }
            SessionRequest::RefreshConfigOptions => {
                self.broadcast_event(SessionEventPayload::ConfigChanged {
                    config: self.record.config(),
                });
            }
            SessionRequest::SetConfig { update, response } => {
                let _ = response.send(self.set_config(update).await);
            }
            SessionRequest::Set { update, response } => {
                let _ = response.send(self.set_session(update).await);
            }
            SessionRequest::SetWorkspace {
                workspace,
                cwd,
                tools,
                prompt_builder,
                response,
            } => {
                let _ = response.send(
                    self.set_workspace(workspace, cwd, tools, *prompt_builder)
                        .await,
                );
            }
            SessionRequest::Keep { response } => {
                let _ = response.send(self.keep().await);
            }
            SessionRequest::PublishNotification {
                notification,
                response,
            } => {
                let _ = response.send(self.publish_notification(notification).await);
            }
            SessionRequest::RespondPermission {
                origin: responder,
                request_id,
                decision,
                response,
            } => {
                let _ = response.send(self.respond_permission(responder, request_id, decision));
            }
            SessionRequest::Unload { response } => {
                return self.unload(response).await;
            }
        }
        ActorControl::Continue
    }

    async fn prompt(&mut self, input: PromptInput) -> Result<PromptReceipt, SessionServiceError> {
        if self.phase == RuntimePhase::Closing {
            return Err(SessionServiceError::SessionClosed(
                self.record.info.id.clone(),
            ));
        }
        // Resume is an empty internal prompt. Check atomically inside the actor,
        // before any context, expiry, or steering changes (including cancellation).
        if matches!(&input, PromptInput::Internal { content } if content.is_empty())
            && let Some(active) = &self.active
        {
            return Ok(PromptReceipt {
                turn_id: active.id.clone(),
                message_id: None,
            });
        }
        if self.phase == RuntimePhase::Cancelling {
            return Err(SessionServiceError::PromptCancelled(
                self.record.info.id.clone(),
            ));
        }
        let (content, is_user, origin) = match input {
            PromptInput::User { origin, content } => (content, true, Some(origin)),
            PromptInput::Internal { content } => (content, false, None),
        };
        if is_user && self.record.info.ephemeral && self.record.info.completed {
            return Err(SessionServiceError::InvalidConfig(format!(
                "session {} has completed and cannot accept more prompts; keep it first",
                self.record.info.id
            )));
        }
        if content.contains_images() {
            let supported = self
                .model
                .supports_image_input(&self.record.llm.model)
                .map_err(|error| SessionServiceError::InvalidConfig(error.to_string()))?;
            if !supported {
                return Err(SessionServiceError::InvalidConfig(format!(
                    "model {} does not support image input",
                    self.record.llm.model
                )));
            }
        }
        if self.active_compaction.is_some() {
            return Err(SessionServiceError::SessionBusy(
                self.record.info.id.clone(),
            ));
        }
        if self.record.info.ephemeral && self.record.info.delete_after_ms.is_some() {
            self.record.info.delete_after_ms = None;
            self.reset_expiry(None);
            self.record.touch();
            self.repository.save(&self.record).await?;
        }

        let previous_usage = self.usage_snapshot();
        let turn_id = self.active.as_ref().map(|active| active.id.clone());
        if let Some(turn_id) = turn_id {
            let message_id = is_user.then(MessageId::new);
            if let (Some(origin), Some(message_id)) = (origin, message_id.clone()) {
                self.record_event(SessionEventPayload::UserPromptSubmitted {
                    message_id: message_id.clone(),
                    turn_id: turn_id.clone(),
                    origin,
                    content: content.clone(),
                })
                .await?;
            }
            let pending = if is_user {
                PendingContextMessage::User(content)
            } else {
                PendingContextMessage::Internal(content)
            };
            self.active
                .as_ref()
                .expect("active prompt has a turn")
                .steer
                .send(pending)
                .map_err(|_| SessionServiceError::SessionClosed(self.record.info.id.clone()))?;
            return Ok(PromptReceipt {
                turn_id,
                message_id,
            });
        }

        let turn_id = TurnId::new();
        let message_id = is_user.then(MessageId::new);
        let repaired_title = is_user
            .then(|| {
                self.record
                    .info
                    .title
                    .trim()
                    .is_empty()
                    .then(|| title_from_user_content(&content))
                    .flatten()
            })
            .flatten();
        if let Some(title) = &repaired_title {
            self.record.set_automatic_title(title.clone());
        }
        let title_generation = if is_user
            && repaired_title.is_none()
            && self.record.auto_title_pending()
            && self.title_cancellation.is_none()
        {
            title_source(&content).map(|source| (source, self.record.info.title.clone()))
        } else {
            None
        };
        let mut context = ContextManager::new(self.record.context.clone());
        if is_user {
            context.append_user(content.clone());
        } else if !content.is_empty() {
            context.append_internal(MessageKind::Runtime, content.clone());
        }
        context.refresh_usage(self.tools.schemas());
        self.record.context = context.into_context();
        self.record.touch();
        self.repository.save(&self.record).await?;
        if let Some((source, original_title)) = title_generation {
            self.title_cancellation = Some(spawn_title_generation(
                self.model.clone(),
                ModelSelection {
                    model: self.record.llm.model.clone(),
                    reasoning: None,
                },
                self.actor_tx.clone(),
                source,
                original_title,
            ));
        }
        if is_user {
            if let (Some(origin), Some(message_id)) = (origin, message_id.clone()) {
                self.record_event(SessionEventPayload::UserPromptSubmitted {
                    message_id,
                    turn_id: turn_id.clone(),
                    origin,
                    content: content.clone(),
                })
                .await?;
            }
            if let Some(title) = &repaired_title {
                self.record.set_automatic_title(title.clone());
                self.broadcast_event(SessionEventPayload::TitleChanged {
                    title: title.clone(),
                    updated_at_ms: self.record.info.updated_at_ms,
                });
            }
        }
        if self.usage_snapshot() != previous_usage {
            self.emit_usage_changed();
        }
        let cancellation = CancellationToken::new();
        let (steer_tx, steer_rx) = mpsc::unbounded_channel();
        self.active = Some(ActiveTurnState {
            id: turn_id.clone(),
            cancellation: cancellation.clone(),
            partial_message: String::new(),
            partial_reasoning: String::new(),
            step_id: 1,
            step_revision: 0,
            message_id: MessageId::new(),
            thought_message_id: MessageId::new(),
            tools: Vec::new(),
            steer: steer_tx,
        });
        self.phase = RuntimePhase::Running;
        self.broadcast_event(SessionEventPayload::TurnStarted {
            turn_id: turn_id.clone(),
        });
        let permission =
            PermissionRequester::new(turn_id.clone(), cancellation.clone(), self.actor_tx.clone());
        let turn = TurnExecution {
            session_id: self.record.info.id.clone(),
            turn_id: turn_id.clone(),
            context: ContextManager::new(self.record.context.clone()),
            prompt_builder: self.prompt_builder.clone(),
            model: self.model.clone(),
            tools: self.tools.clone(),
            config: self.config_tx.subscribe(),
            permission,
            cancellation,
            steer: steer_rx,
            actor: self.actor_tx.clone(),
        };
        tokio::spawn(turn::run(turn));
        Ok(PromptReceipt {
            turn_id,
            message_id,
        })
    }

    fn cancel(&mut self, expected_turn_id: Option<TurnId>) -> Result<(), SessionServiceError> {
        if let Some(active) = &self.active {
            if let Some(expected) = expected_turn_id
                && expected != active.id
            {
                return Err(SessionServiceError::TurnNotActive(expected));
            }
            active.cancellation.cancel();
            let tools = self.tools.clone();
            tokio::spawn(async move { tools.shutdown().await });
        } else if let Some(active) = &self.active_compaction {
            if let Some(expected) = expected_turn_id {
                return Err(SessionServiceError::TurnNotActive(expected));
            }
            active.cancellation.cancel();
        } else {
            return Err(SessionServiceError::TurnNotActive(
                expected_turn_id.unwrap_or_default(),
            ));
        }
        self.phase = RuntimePhase::Cancelling;
        self.permission.reject("turn cancelled");
        Ok(())
    }

    async fn set_config(&mut self, update: SessionConfigUpdate) -> Result<(), SessionServiceError> {
        let update = match update {
            SessionConfigUpdate::Mode(mode) => SessionUpdate {
                title: None,
                mode: Some(mode),
                model: None,
                reasoning: None,
                regenerate_title: None,
            },
            SessionConfigUpdate::Model(model) => SessionUpdate {
                title: None,
                mode: None,
                model: Some(model),
                reasoning: None,
                regenerate_title: None,
            },
            SessionConfigUpdate::Reasoning(reasoning) => SessionUpdate {
                title: None,
                mode: None,
                model: None,
                reasoning: Some(reasoning),
                regenerate_title: None,
            },
        };
        self.set_session(update).await
    }

    async fn set_session(&mut self, update: SessionUpdate) -> Result<(), SessionServiceError> {
        if self.phase == RuntimePhase::Closing {
            return Err(SessionServiceError::SessionClosed(
                self.record.info.id.clone(),
            ));
        }
        let SessionUpdate {
            title,
            mode,
            model,
            reasoning,
            regenerate_title,
        } = update;
        let title_changed = title
            .as_deref()
            .is_some_and(|title| title.trim() != self.record.info.title);
        let reasoning_changed = reasoning.is_some();
        let config_changed = mode.is_some() || model.is_some() || reasoning.is_some();
        let model_changed = model.is_some();
        let previous_model = self.record.llm.model.clone();
        let new_model_reasoning = model.as_deref().and_then(|model| {
            self.model
                .reasoning_modes(model)
                .ok()
                .and_then(|modes| middle_reasoning_mode(&modes))
        });
        let mut updated = self.record.clone();
        if let Some(mode) = mode {
            updated
                .apply_config(SessionConfigUpdate::Mode(mode), None)
                .map_err(SessionServiceError::InvalidConfig)?;
        }
        if let Some(model) = model {
            updated
                .apply_config(SessionConfigUpdate::Model(model), new_model_reasoning)
                .map_err(SessionServiceError::InvalidConfig)?;
        }
        if let Some(reasoning) = reasoning {
            updated
                .apply_config(SessionConfigUpdate::Reasoning(reasoning), None)
                .map_err(SessionServiceError::InvalidConfig)?;
        }
        if let Some(title) = title {
            updated
                .apply_title(title)
                .map_err(SessionServiceError::InvalidConfig)?;
        }
        let mut selection = ModelSelection {
            model: updated.llm.model.clone(),
            reasoning: updated.llm.reasoning.clone(),
        };
        let mut remember_reasoning = true;
        if let Err(error) = self.model.validate_selection(&selection) {
            if !model_changed || reasoning_changed || updated.llm.reasoning.is_none() {
                return Err(SessionServiceError::InvalidConfig(error.to_string()));
            }
            remember_reasoning = !updated
                .llm
                .reasoning_by_model
                .contains_key(&updated.llm.model);
            updated.llm.reasoning = None;
            selection.reasoning = None;
            self.model
                .validate_selection(&selection)
                .map_err(|_| SessionServiceError::InvalidConfig(error.to_string()))?;
        }
        if model_changed {
            normalize_context_for_model(
                self.model.as_ref(),
                self.tools.as_ref(),
                &mut updated,
                &previous_model,
            )?;
        }
        if remember_reasoning {
            updated.llm.remember_current_reasoning();
        }
        updated.touch();
        self.repository.save(&updated).await?;
        self.record = updated;
        if config_changed {
            let config = self.record.config();
            self.config_tx.send_replace(config.clone());
            self.broadcast_event(SessionEventPayload::ConfigChanged { config });
        }
        if title_changed {
            self.broadcast_event(SessionEventPayload::TitleChanged {
                title: self.record.info.title.clone(),
                updated_at_ms: self.record.info.updated_at_ms,
            });
        }
        if model_changed {
            self.emit_usage_changed();
        }
        if regenerate_title == Some(true) {
            self.spawn_title_regeneration()?;
        }
        Ok(())
    }

    async fn set_workspace(
        &mut self,
        workspace: SessionWorkspace,
        cwd: std::path::PathBuf,
        tools: Arc<ToolManager>,
        prompt_builder: SystemPromptBuilder,
    ) -> Result<(), SessionServiceError> {
        if self.phase != RuntimePhase::Idle {
            tools.shutdown().await;
            return Err(SessionServiceError::SessionBusy(
                self.record.info.id.clone(),
            ));
        }
        let old_cwd = self.record.info.cwd.clone();
        let mut context = ContextManager::new(self.record.context.clone());
        let prompt = prompt_builder.rebuild().map_err(anyhow::Error::from)?;
        context.replace_system_prompt(prompt);
        context.append_internal(
            MessageKind::Runtime,
            format!(
                "Workspace changed from {} to {}.",
                old_cwd.display(),
                cwd.display()
            ),
        );
        context.refresh_usage(tools.schemas());
        let mut updated = self.record.clone();
        updated.info.cwd = cwd.clone();
        updated.info.workspace = workspace.clone();
        updated.context = context.into_context();
        updated.touch();
        self.repository.save(&updated).await?;
        self.tools.shutdown().await;
        self.record = updated;
        self.tools = tools;
        self.prompt_builder = prompt_builder;
        self.emit_usage_changed();
        self.broadcast_event(SessionEventPayload::WorkspaceChanged { old_cwd, cwd });
        Ok(())
    }

    async fn keep(&mut self) -> Result<bool, SessionServiceError> {
        let mut updated = self.record.clone();
        let changed = updated.info.ephemeral || updated.info.delete_after_ms.is_some();
        updated.info.ephemeral = false;
        updated.info.delete_after_ms = None;
        if changed {
            updated.touch();
            self.repository.save(&updated).await?;
            self.record = updated;
            self.reset_expiry(None);
        }
        Ok(changed)
    }

    async fn publish_notification(
        &mut self,
        notification: SessionNotification,
    ) -> Result<MessageId, SessionServiceError> {
        if self.phase == RuntimePhase::Closing {
            return Err(SessionServiceError::SessionClosed(
                self.record.info.id.clone(),
            ));
        }
        let message_id = MessageId::new();
        self.record_event(SessionEventPayload::Notification {
            message_id: message_id.clone(),
            turn_id: self.active.as_ref().map(|active| active.id.clone()),
            origin: notification.origin,
            category: notification.category,
            level: notification.level,
            text: notification.text,
            data: notification.data,
        })
        .await?;
        Ok(message_id)
    }

    fn respond_permission(
        &mut self,
        responder: EndpointId,
        request_id: String,
        decision: ConfirmationDecision,
    ) -> Result<(), SessionServiceError> {
        let allowed = decision.allowed;
        let reason = decision.reason.clone();
        let resolved = self.permission.respond(&request_id, decision)?;
        self.phase = RuntimePhase::Running;
        self.broadcast_event(SessionEventPayload::PermissionResolved {
            turn_id: resolved.turn_id,
            request_id: resolved.request_id,
            responder,
            allowed,
            reason,
        });
        Ok(())
    }

    async fn unload(
        &mut self,
        response: oneshot::Sender<Result<(), SessionServiceError>>,
    ) -> ActorControl {
        if self.phase == RuntimePhase::Closing {
            let _ = response.send(Err(SessionServiceError::SessionClosed(
                self.record.info.id.clone(),
            )));
            return ActorControl::Continue;
        }
        self.phase = RuntimePhase::Closing;
        if let Some(cancellation) = self.title_cancellation.take() {
            cancellation.cancel();
        }
        self.broadcast_event(SessionEventPayload::Closing);
        if self.active.is_some() || self.active_compaction.is_some() {
            if let Some(active) = &self.active {
                active.cancellation.cancel();
                self.permission.reject("session closed");
                let tools = self.tools.clone();
                tokio::spawn(async move { tools.shutdown().await });
            }
            if let Some(active) = &self.active_compaction {
                active.cancellation.cancel();
            }
            self.closing_response = Some(response);
            return ActorControl::Continue;
        }
        self.permission.reject("session closed");
        self.tools.shutdown().await;
        let result = self.repository.save(&self.record).await.map_err(Into::into);
        let _ = response.send(result);
        ActorControl::Stop
    }

    async fn compact(
        &mut self,
        origin: EndpointId,
    ) -> Result<CompactionAccepted, SessionServiceError> {
        if self.phase != RuntimePhase::Idle {
            return Err(SessionServiceError::SessionBusy(
                self.record.info.id.clone(),
            ));
        }
        let previous_usage = self.usage_snapshot();
        let previous_model = self
            .record
            .context
            .usage
            .last_model
            .clone()
            .unwrap_or_else(|| self.record.llm.model.clone());
        let mut updated = self.record.clone();
        normalize_context_for_model(
            self.model.as_ref(),
            self.tools.as_ref(),
            &mut updated,
            &previous_model,
        )?;
        updated.touch();
        self.repository.save(&updated).await?;
        self.record = updated;
        if self.usage_snapshot() != previous_usage {
            self.emit_usage_changed();
        }

        let compaction_id = format!("cmp_{}", uuid::Uuid::new_v4().simple());
        let cancellation = CancellationToken::new();
        self.active_compaction = Some(ActiveCompaction {
            id: compaction_id.clone(),
            cancellation: cancellation.clone(),
            origin: origin.clone(),
        });
        self.phase = RuntimePhase::Compacting;
        let _ = self
            .record_event(SessionEventPayload::Notification {
                message_id: MessageId::new(),
                turn_id: None,
                origin: Some(origin),
                category: "compaction_started".to_string(),
                level: NotificationLevel::Info,
                text: "Compacting context...".to_string(),
                data: serde_json::json!({
                    "compactionId": compaction_id,
                    "trigger": "manual",
                }),
            })
            .await;

        let context = ContextManager::new(self.record.context.clone());
        let prompt_builder = self.prompt_builder.clone();
        let model = self.model.clone();
        let tools = self.tools.schemas().to_vec();
        let selection = ModelSelection {
            model: previous_model,
            reasoning: self.record.llm.reasoning.clone(),
        };
        let actor = self.actor_tx.clone();
        let id = compaction_id.clone();
        tokio::spawn(async move {
            let result = compaction::execute(
                context,
                &prompt_builder,
                &model,
                &tools,
                &cancellation,
                CompactionRequest {
                    selection,
                    trigger: CompactionTrigger::Manual,
                },
            )
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = actor.send(ActorEvent::CompactionFinished {
                compaction_id: id,
                result: Box::new(result),
            });
        });
        Ok(CompactionAccepted { compaction_id })
    }

    async fn handle_event(&mut self, event: ActorEvent) -> ActorControl {
        match event {
            ActorEvent::Turn(event) => {
                match event {
                    TurnUpdate::AssistantDelta { turn_id, delta } => {
                        let step = if let Some(active) =
                            self.active.as_mut().filter(|active| active.id == turn_id)
                        {
                            active.partial_message.push_str(&delta);
                            active.step_revision = active.step_revision.saturating_add(1);
                            Some((
                                active.step_id,
                                active.step_revision,
                                active.message_id.clone(),
                            ))
                        } else {
                            None
                        };
                        if let Some((step_id, revision, message_id)) = step {
                            self.broadcast_event(SessionEventPayload::AssistantDelta {
                                message_id,
                                turn_id,
                                step_id,
                                revision,
                                delta,
                            });
                        }
                    }
                    TurnUpdate::AssistantReasoningDelta { turn_id, delta } => {
                        let step = if let Some(active) =
                            self.active.as_mut().filter(|active| active.id == turn_id)
                        {
                            active.partial_reasoning.push_str(&delta);
                            active.step_revision = active.step_revision.saturating_add(1);
                            Some((
                                active.step_id,
                                active.step_revision,
                                active.thought_message_id.clone(),
                            ))
                        } else {
                            None
                        };
                        if let Some((step_id, revision, message_id)) = step {
                            self.broadcast_event(SessionEventPayload::AssistantReasoningDelta {
                                message_id,
                                turn_id,
                                step_id,
                                revision,
                                delta,
                            });
                        }
                    }
                    TurnUpdate::AssistantCompleted {
                        turn_id,
                        content,
                        reasoning,
                        tool_calls,
                    } => {
                        let accepted = if let Some(active) =
                            self.active.as_mut().filter(|active| active.id == turn_id)
                        {
                            let message_id = active.message_id.clone();
                            let thought_message_id = active.thought_message_id.clone();
                            active.partial_message.clear();
                            active.partial_reasoning.clear();
                            active.step_id = active.step_id.saturating_add(1);
                            active.step_revision = 0;
                            active.message_id = MessageId::new();
                            active.thought_message_id = MessageId::new();
                            Some((message_id, thought_message_id))
                        } else {
                            None
                        };
                        if let Some((message_id, thought_message_id)) = accepted {
                            let _ = self
                                .record_event(SessionEventPayload::AssistantCompleted {
                                    message_id,
                                    thought_message_id,
                                    turn_id,
                                    content,
                                    reasoning,
                                    tool_calls,
                                })
                                .await;
                        }
                    }
                    TurnUpdate::AssistantInterrupted {
                        turn_id,
                        content,
                        reasoning,
                        error_kind,
                    } => {
                        let accepted = if let Some(active) =
                            self.active.as_mut().filter(|active| active.id == turn_id)
                        {
                            let message_id = active.message_id.clone();
                            let thought_message_id = active.thought_message_id.clone();
                            active.partial_message.clear();
                            active.partial_reasoning.clear();
                            active.step_id = active.step_id.saturating_add(1);
                            active.step_revision = 0;
                            active.message_id = MessageId::new();
                            active.thought_message_id = MessageId::new();
                            Some((message_id, thought_message_id))
                        } else {
                            None
                        };
                        if let Some((message_id, thought_message_id)) = accepted {
                            let _ = self
                                .record_event(SessionEventPayload::AssistantInterrupted {
                                    message_id,
                                    thought_message_id,
                                    turn_id,
                                    content,
                                    reasoning,
                                    error_kind,
                                })
                                .await;
                        }
                    }
                    TurnUpdate::Notification {
                        turn_id,
                        category,
                        level,
                        text,
                        data,
                    } => {
                        let _ = self
                            .record_event(SessionEventPayload::Notification {
                                message_id: MessageId::new(),
                                turn_id: Some(turn_id),
                                origin: None,
                                category,
                                level,
                                text,
                                data,
                            })
                            .await;
                    }
                    TurnUpdate::ToolChanged { turn_id, call } => {
                        let payload = self
                            .active
                            .as_mut()
                            .filter(|active| active.id == turn_id)
                            .and_then(|active| {
                                project_tool_call(&mut active.tools, &turn_id, call)
                            });
                        if let Some(payload) = payload {
                            let _ = self.record_event(payload).await;
                        }
                    }
                    TurnUpdate::ToolCallsInterrupted { turn_id, status } => {
                        let calls = self
                            .active
                            .as_mut()
                            .filter(|active| active.id == turn_id)
                            .map(|active| std::mem::take(&mut active.tools))
                            .unwrap_or_default();
                        for mut call in calls {
                            if terminal_tool_status(&call.status) {
                                continue;
                            }
                            call.status = status.to_string();
                            let _ = self
                                .record_event(SessionEventPayload::ToolUpdated {
                                    turn_id: turn_id.clone(),
                                    call,
                                })
                                .await;
                        }
                    }
                    TurnUpdate::ToolCompleted { turn_id, result } => {
                        let accepted = if let Some(active) =
                            self.active.as_mut().filter(|active| active.id == turn_id)
                        {
                            active
                                .tools
                                .retain(|call| call.tool_call_id != result.tool_call_id);
                            true
                        } else {
                            false
                        };
                        if accepted {
                            let _ = self
                                .record_event(SessionEventPayload::ToolCompleted {
                                    turn_id,
                                    result,
                                })
                                .await;
                        }
                    }
                    TurnUpdate::ToolTelemetry { turn_id, event } => {
                        // A terminal is reachable from its open until its exit
                        // is observed; replay uses this set to separate live
                        // terminals from ones lost with a previous process.
                        match &event {
                            dwo_tools::ToolEvent::TerminalOpened { terminal_id, .. } => {
                                self.live_terminals.insert(terminal_id.clone());
                            }
                            dwo_tools::ToolEvent::TerminalExited { terminal_id, .. } => {
                                self.live_terminals.remove(terminal_id);
                            }
                            _ => {}
                        }
                        // Retain even without subscribers or an active model turn.
                        // This also survives a transcript persistence failure.
                        self.terminal_views.observe(&event);
                        match event {
                            dwo_tools::ToolEvent::TerminalOpened {
                                tool_call_id,
                                terminal_id,
                                command,
                                cwd,
                            } => {
                                let _ = self
                                    .record_event(SessionEventPayload::TerminalOpened {
                                        turn_id,
                                        tool_call_id,
                                        terminal_id,
                                        command,
                                        cwd,
                                    })
                                    .await;
                            }
                            dwo_tools::ToolEvent::TerminalOutput { terminal_id, data } => {
                                self.broadcast_event(SessionEventPayload::TerminalOutput {
                                    turn_id,
                                    terminal_id,
                                    data,
                                });
                            }
                            dwo_tools::ToolEvent::TerminalExited {
                                terminal_id,
                                exit_code,
                                status,
                            } => {
                                let _ = self
                                    .record_event(SessionEventPayload::TerminalExited {
                                        turn_id,
                                        terminal_id,
                                        exit_code,
                                        status,
                                    })
                                    .await;
                            }
                            dwo_tools::ToolEvent::FileRead { tool_call_id, path } => {
                                let _ = self
                                    .record_event(SessionEventPayload::FileRead {
                                        turn_id,
                                        tool_call_id,
                                        path,
                                    })
                                    .await;
                            }
                            dwo_tools::ToolEvent::FileChanged {
                                tool_call_id,
                                changes,
                                patch,
                            } => {
                                let _ = self
                                    .record_event(SessionEventPayload::FileChanged {
                                        turn_id,
                                        tool_call_id,
                                        changes: changes
                                            .into_iter()
                                            .map(|change| FileChange {
                                                path: change.path,
                                                kind: change.kind.to_string(),
                                                moved_to: change.moved_to,
                                            })
                                            .collect(),
                                        patch,
                                    })
                                    .await;
                            }
                        }
                    }
                    TurnUpdate::Finished { turn_id, outcome } => {
                        if self
                            .active
                            .as_ref()
                            .is_none_or(|active| active.id != turn_id)
                        {
                            return ActorControl::Continue;
                        }
                        self.active.take().expect("active turn was checked");
                        self.permission.reject("turn finished");
                        self.phase = RuntimePhase::Idle;
                        self.record.info.last_turn_finished_at_ms = Some(current_time_ms());
                        if self.record.info.ephemeral {
                            self.record.info.completed |= matches!(outcome, TurnOutcome::Completed);
                            self.record.info.delete_after_ms =
                                Some(current_time_ms().saturating_add(EPHEMERAL_GRACE_MS));
                            self.reset_expiry(self.record.info.delete_after_ms);
                            self.record.touch();
                            if let Err(error) = self.repository.save(&self.record).await {
                                tracing::error!(
                                    event = "session.ephemeral_outcome_save_failed",
                                    session_id = %self.record.info.id,
                                    error = %error,
                                    "persist ephemeral session outcome failed"
                                );
                            }
                        }
                        match &outcome {
                            TurnOutcome::Completed => {
                                self.broadcast_event(SessionEventPayload::TurnCompleted {
                                    turn_id: turn_id.clone(),
                                })
                            }
                            TurnOutcome::Cancelled => {
                                self.broadcast_event(SessionEventPayload::TurnCancelled {
                                    turn_id: turn_id.clone(),
                                })
                            }
                            TurnOutcome::Failed(error) => {
                                self.broadcast_event(SessionEventPayload::TurnFailed {
                                    turn_id: turn_id.clone(),
                                    error: error.clone(),
                                })
                            }
                        }
                        // 终态时间落盘：清单接口（session.list）直接读它，
                        // GUI 的轮询不用为每条会话翻 transcript。
                        if !self.record.info.ephemeral {
                            if let Err(error) = self.repository.save(&self.record).await {
                                tracing::error!(
                                    event = "session.turn_finished_save_failed",
                                    session_id = %self.record.info.id,
                                    error = %error,
                                    "persist turn finish time failed"
                                );
                            }
                        }
                        if let Some(response) = self.closing_response.take() {
                            self.phase = RuntimePhase::Closing;
                            self.tools.shutdown().await;
                            let result =
                                self.repository.save(&self.record).await.map_err(Into::into);
                            let _ = response.send(result);
                            return ActorControl::Stop;
                        }
                    }
                }
                return ActorControl::Continue;
            }
            ActorEvent::TitleGenerated {
                original_title,
                result,
            } => {
                self.apply_generated_title(&original_title, result).await;
            }
            ActorEvent::PersistContext { context, completed } => {
                let previous_usage = self.usage_snapshot();
                let mut updated = self.record.clone();
                updated.context = *context;
                updated.touch();
                let result = self.repository.save(&updated).await;
                if result.is_ok() {
                    self.record = updated;
                    if self.usage_snapshot() != previous_usage {
                        self.emit_usage_changed();
                    }
                }
                let _ = completed.send(result);
            }
            ActorEvent::Todo {
                turn_id,
                request,
                completed,
            } => {
                let result = if self
                    .active
                    .as_ref()
                    .is_none_or(|active| active.id != turn_id)
                {
                    Err("turn is no longer active".to_string())
                } else if request.action == TodoAction::Get {
                    Ok(todo_response(
                        false,
                        false,
                        self.record
                            .current_plan
                            .as_ref()
                            .map(|plan| plan.entries.clone())
                            .unwrap_or_default(),
                    ))
                } else {
                    let (plan, cleared) = if request.entries.is_empty() {
                        (
                            self.record
                                .current_plan
                                .as_ref()
                                .map(ExecutionPlan::terminalized)
                                .unwrap_or_else(|| ExecutionPlan::new(Vec::new())),
                            true,
                        )
                    } else if self
                        .record
                        .current_plan
                        .as_ref()
                        .is_some_and(|plan| plan.entries == request.entries)
                    {
                        let _ = completed.send(Ok(todo_response(false, false, request.entries)));
                        return ActorControl::Continue;
                    } else {
                        let mut plan = self
                            .record
                            .current_plan
                            .clone()
                            .unwrap_or_else(|| ExecutionPlan::new(request.entries.clone()));
                        plan.entries = request.entries;
                        let cleared = plan.is_finished();
                        (plan, cleared)
                    };
                    let mut updated = self.record.clone();
                    updated.current_plan = (!cleared).then_some(plan.clone());
                    updated.touch();
                    match self.repository.save(&updated).await {
                        Ok(()) => {
                            self.record = updated;
                            let _ = self
                                .record_event(SessionEventPayload::PlanUpdated {
                                    turn_id,
                                    plan: plan.clone(),
                                    cleared,
                                })
                                .await;
                            Ok(todo_response(true, cleared, plan.entries))
                        }
                        Err(error) => Err(format!("persist plan: {error:#}")),
                    }
                };
                let _ = completed.send(result);
            }
            ActorEvent::CompactionFinished {
                compaction_id,
                result,
            } => return self.apply_compaction_result(compaction_id, *result).await,
            ActorEvent::PermissionRequested(request) => {
                self.handle_permission_request(request);
            }
            ActorEvent::Expired => return self.expire().await,
        }
        ActorControl::Continue
    }

    async fn apply_compaction_result(
        &mut self,
        compaction_id: String,
        result: Result<CompactionResult, String>,
    ) -> ActorControl {
        let Some(active) = self.active_compaction.take() else {
            return ActorControl::Continue;
        };
        if active.id != compaction_id {
            self.active_compaction = Some(active);
            return ActorControl::Continue;
        }

        let previous_usage = self.usage_snapshot();
        let cancelled = active.cancellation.is_cancelled();
        let result = if cancelled {
            Err(None)
        } else {
            match result {
                Ok(compaction) => {
                    let summary = compaction.summary.clone();
                    let compacted = compaction.compacted;
                    let mut updated = self.record.clone();
                    updated.context = compaction.context.into_context();
                    updated.touch();
                    match self.repository.save(&updated).await {
                        Ok(()) => {
                            self.record = updated;
                            Ok((summary, compacted))
                        }
                        Err(error) => Err(Some(format!("persist compacted context: {error:#}"))),
                    }
                }
                Err(error) => Err(Some(error)),
            }
        };

        match result {
            Ok((summary, compacted)) => {
                let _ = self
                    .record_event(SessionEventPayload::Notification {
                        message_id: MessageId::new(),
                        turn_id: None,
                        origin: Some(active.origin),
                        category: "compaction_completed".to_string(),
                        level: NotificationLevel::Success,
                        text: if compacted {
                            "Context compacted.".to_string()
                        } else {
                            "Nothing to compact.".to_string()
                        },
                        data: serde_json::json!({
                            "compactionId": compaction_id,
                            "summary": summary,
                            "compacted": compacted,
                        }),
                    })
                    .await;
            }
            Err(None) => {
                let _ = self
                    .record_event(SessionEventPayload::Notification {
                        message_id: MessageId::new(),
                        turn_id: None,
                        origin: Some(active.origin),
                        category: "compaction_cancelled".to_string(),
                        level: NotificationLevel::Warning,
                        text: "Context compaction cancelled.".to_string(),
                        data: serde_json::json!({"compactionId": compaction_id}),
                    })
                    .await;
            }
            Err(Some(error)) => {
                let _ = self
                    .record_event(SessionEventPayload::Notification {
                        message_id: MessageId::new(),
                        turn_id: None,
                        origin: Some(active.origin),
                        category: "compaction_failed".to_string(),
                        level: NotificationLevel::Error,
                        text: "Context compaction failed.".to_string(),
                        data: serde_json::json!({
                            "compactionId": compaction_id,
                            "error": error,
                        }),
                    })
                    .await;
            }
        }
        if self.usage_snapshot() != previous_usage {
            self.emit_usage_changed();
        }

        if let Some(response) = self.closing_response.take() {
            self.phase = RuntimePhase::Closing;
            self.tools.shutdown().await;
            let result = self.repository.save(&self.record).await.map_err(Into::into);
            let _ = response.send(result);
            return ActorControl::Stop;
        }
        self.phase = RuntimePhase::Idle;
        ActorControl::Continue
    }

    fn handle_permission_request(&mut self, request: PermissionRequestEnvelope) {
        if self
            .active
            .as_ref()
            .is_none_or(|active| active.id != request.turn_id)
        {
            request.reject("turn is no longer active");
            return;
        }
        let turn_id = request.turn_id.clone();
        let permission = request.permission.clone();
        self.permission.register(request);
        self.phase = RuntimePhase::WaitingPermission;
        self.broadcast_event(SessionEventPayload::PermissionRequested {
            turn_id,
            permission,
        });
    }

    async fn apply_generated_title(
        &mut self,
        original_title: &str,
        result: Result<ModelReply, dwo_model_client::ModelClientError>,
    ) {
        self.title_cancellation = None;
        let Ok(reply) = result else {
            return;
        };
        let Some(title) = clean_generated_session_title(&reply.content) else {
            return;
        };
        if self.record.info.title != original_title {
            return;
        }

        let mut updated = self.record.clone();
        updated.set_automatic_title(title.clone());
        updated.touch();
        if self.repository.save(&updated).await.is_err() {
            return;
        }
        let updated_at_ms = updated.info.updated_at_ms;
        self.record = updated;
        self.broadcast_event(SessionEventPayload::TitleChanged {
            title,
            updated_at_ms,
        });
    }

    /// 按需重新生成标题（`session.set` 的 `regenerate_title`）：拿 transcript
    /// 里第一条用户消息当素材，走创建时自动标题同一套生成任务与落地保护
    /// （生成期间标题被改过就丢弃结果）。
    fn spawn_title_regeneration(&mut self) -> Result<(), SessionServiceError> {
        let source = self
            .transcript
            .iter()
            .find_map(|event| match &event.payload {
                SessionEventPayload::UserPromptSubmitted { content, .. } => title_source(content),
                _ => None,
            })
            .ok_or_else(|| {
                SessionServiceError::InvalidConfig(format!(
                    "session {} has no user message to generate a title from",
                    self.record.info.id
                ))
            })?;
        if let Some(in_flight) = self.title_cancellation.take() {
            in_flight.cancel();
        }
        self.title_cancellation = Some(spawn_title_generation(
            self.model.clone(),
            ModelSelection {
                model: self.record.llm.model.clone(),
                reasoning: None,
            },
            self.actor_tx.clone(),
            source,
            self.record.info.title.clone(),
        ));
        Ok(())
    }

    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            record: self.record.clone(),
            transcript: self.transcript.clone(),
            checkpoint_cursor: self.transcript.len(),
            usage: self.usage_snapshot(),
            phase: self.phase,
            active_turn_id: self.active.as_ref().map(|active| active.id.clone()),
            active_step: self.active.as_ref().map(|active| ActiveStepSnapshot {
                turn_id: active.id.clone(),
                step_id: active.step_id,
                revision: active.step_revision,
                message_id: active.message_id.clone(),
                thought_message_id: active.thought_message_id.clone(),
                reasoning: active.partial_reasoning.clone(),
                response: active.partial_message.clone(),
            }),
            partial_message: self
                .active
                .as_ref()
                .map(|active| active.partial_message.clone())
                .unwrap_or_default(),
            active_tool_calls: self
                .active
                .as_ref()
                .map(|active| active.tools.clone())
                .unwrap_or_default(),
            pending_permission: self.permission.snapshot(),
            terminal_views: self.terminal_views.snapshot(),
            live_terminal_ids: {
                let mut ids: Vec<String> = self.live_terminals.iter().cloned().collect();
                ids.sort();
                ids
            },
            seq: self.seq,
        }
    }

    fn usage_snapshot(&self) -> SessionUsageSnapshot {
        let used = self.record.context.usage.current_tokens;
        let size = self
            .model
            .model_limits(&self.record.llm.model)
            .map(|limits| limits.context_window_tokens)
            .unwrap_or(used);
        SessionUsageSnapshot { used, size }
    }

    fn emit_usage_changed(&mut self) {
        let usage = self.usage_snapshot();
        self.broadcast_event(SessionEventPayload::UsageChanged {
            used: usage.used,
            size: usage.size,
        });
    }

    async fn record_event(
        &mut self,
        payload: SessionEventPayload,
    ) -> Result<(), SessionServiceError> {
        let event = ClientTranscriptEvent::new(payload.clone());
        if let Err(error) = self
            .repository
            .append_transcript_event(&self.record.info.id, &event)
            .await
        {
            tracing::error!(
                event = "session.transcript_persist_failed",
                session_id = %self.record.info.id,
                error = %format!("{error:#}"),
                "persist client transcript event failed"
            );
            return Err(error.into());
        }
        self.transcript.push(event);
        self.broadcast_event(payload);
        Ok(())
    }

    fn broadcast_event(&mut self, payload: SessionEventPayload) {
        self.seq = self.seq.saturating_add(1);
        let _ = self.events.send(SessionEvent {
            seq: self.seq,
            session_id: self.record.info.id.clone(),
            payload,
        });
    }
}

fn spawn_title_generation(
    model: Arc<dyn ModelClient>,
    selection: ModelSelection,
    actor: mpsc::UnboundedSender<ActorEvent>,
    source: String,
    original_title: String,
) -> CancellationToken {
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    tokio::spawn(async move {
        let source = title_source_excerpt(&source);
        let messages = vec![
            ContextMessage::system(
                "Generate a concise conversation title. Return only the title, without quotes or explanation. Use the same language as the user. Keep it under 12 Chinese characters or 6 English words.",
            ),
            ContextMessage::user(format!("User message:\n{source}\n\nTitle:")),
        ];
        let result = dwo_model_client::request_with_retry(&task_cancellation, || {
            model.complete(
                selection.clone(),
                messages.clone(),
                task_cancellation.clone(),
            )
        })
        .await;
        let _ = actor.send(ActorEvent::TitleGenerated {
            original_title,
            result,
        });
    });
    cancellation
}

fn normalize_context_for_model(
    model: &dyn ModelClient,
    tools: &ToolManager,
    record: &mut SessionRecord,
    previous_model: &str,
) -> Result<(), SessionServiceError> {
    let provider = model
        .context_owner_id(&record.llm.model)
        .map_err(|error| SessionServiceError::InvalidConfig(error.to_string()))?;
    let previous_provider = match record.context.provider.clone() {
        Some(provider) => provider,
        None => model
            .context_owner_id(previous_model)
            .map_err(|error| SessionServiceError::InvalidConfig(error.to_string()))?,
    };
    let allow_image_input = model
        .supports_image_input(&record.llm.model)
        .map_err(|error| SessionServiceError::InvalidConfig(error.to_string()))?;
    let mut context = ContextManager::new(record.context.clone());
    context.normalize_for_selection(&provider, Some(&previous_provider), allow_image_input);
    context.refresh_usage(tools.schemas());
    record.context = context.into_context();
    Ok(())
}

fn terminal_tool_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled" | "canceled")
}

fn todo_response(updated: bool, cleared: bool, entries: Vec<dwo_tools::TodoEntry>) -> TodoResponse {
    TodoResponse {
        updated,
        cleared,
        entries,
    }
}

fn middle_reasoning_mode(modes: &[String]) -> Option<String> {
    let mut modes = modes.to_vec();
    modes.sort_by_key(|mode| reasoning_mode_rank(mode));
    modes.get(modes.len() / 2).cloned()
}

fn reasoning_mode_rank(mode: &str) -> u8 {
    match mode.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "nonthink" => 0,
        "auto" => 1,
        "minimal" => 2,
        "low" => 3,
        "medium" => 4,
        "high" => 5,
        "xhigh" => 6,
        "max" => 7,
        _ => 8,
    }
}

#[cfg(test)]
mod terminal_view_tests {
    use super::*;
    use dwo_tools::ToolEvent;

    fn opened(id: &str) -> ToolEvent {
        ToolEvent::TerminalOpened {
            tool_call_id: format!("run-{id}"),
            terminal_id: id.into(),
            command: "example".into(),
            cwd: std::env::current_dir().unwrap(),
        }
    }

    fn output(id: &str, data: &[u8]) -> ToolEvent {
        ToolEvent::TerminalOutput {
            terminal_id: id.into(),
            data: data.into(),
        }
    }

    fn actor() -> SessionActor {
        let cwd = std::env::current_dir().unwrap();
        let record = SessionRecord::new(
            crate::SessionId::new(),
            "terminal cache test".into(),
            SessionWorkspace::External { pwd: cwd.clone() },
            cwd.clone(),
            dwo_tools::SessionMode::FullAccess,
            crate::SessionLlmSettings::default(),
        );
        let (_, requests) = mpsc::channel(8);
        let (actor_tx, actor_events) = mpsc::unbounded_channel();
        let (config_tx, _) = watch::channel(record.config());
        let (events, _) = broadcast::channel(1024);
        // No providers exist in this client, so these tests cannot call a model.
        let model = crate::ConfiguredModelClient::from_resolved(crate::ModelClientConfig {
            default_model: "test".into(),
            default_reasoning: None,
            providers: Default::default(),
            models: Default::default(),
        })
        .unwrap();
        SessionActor {
            record,
            transcript: Vec::new(),
            repository: Arc::new(crate::MemorySessionRepository::default()),
            model,
            tools: Arc::new(
                ToolManager::new(
                    cwd.clone(),
                    Arc::new(dwo_tools::ToolPolicyEngine::new(Default::default())),
                    Arc::new(dwo_tools::FileEditManager::new()),
                )
                .unwrap(),
            ),
            prompt_builder: SystemPromptBuilder::new(cwd.clone(), cwd),
            requests,
            actor_tx,
            actor_events,
            expiry_timer: None,
            config_tx,
            events,
            seq: 0,
            terminal_views: TerminalViewCache::default(),
            live_terminals: HashSet::new(),
            phase: RuntimePhase::Idle,
            active: None,
            active_compaction: None,
            permission: PermissionState::default(),
            closing_response: None,
            title_cancellation: None,
            deletion_hook: None,
            terminated: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    async fn telemetry(actor: &mut SessionActor, event: ToolEvent) {
        actor
            .handle_event(ActorEvent::Turn(TurnUpdate::ToolTelemetry {
                turn_id: TurnId::parse("turn-terminal-test").unwrap(),
                event,
            }))
            .await;
    }

    #[tokio::test]
    async fn closed_gui_bytes_and_post_watermark_chunks_are_retained_once() {
        let mut actor = actor();
        actor.repository.save(&actor.record).await.unwrap();
        // No GUI or model turn is active. The background terminal still emits.
        telemetry(&mut actor, opened("term")).await;
        assert_eq!(actor.snapshot().live_terminal_ids, ["term"]);
        let prefix = b"\x1b[31m\xff\0\xe4\xb8";
        telemetry(&mut actor, output("term", prefix)).await;
        assert_eq!(actor.events.receiver_count(), 0);
        assert!(
            !actor
                .transcript
                .iter()
                .any(|event| matches!(event.payload, SessionEventPayload::TerminalOutput { .. }))
        );
        let (response, wait) = oneshot::channel();
        actor
            .handle_request(SessionRequest::Subscribe {
                checkpoint_cursor: Some(usize::MAX),
                response,
            })
            .await;
        let (snapshot, source) = wait.await.unwrap();
        assert!(snapshot.transcript.is_empty());
        assert_eq!(snapshot.terminal_views[0].output, "G1szMW3/AOS4");
        assert_eq!(snapshot.terminal_views[0].status, "running");
        let watermark = snapshot.seq;
        assert_eq!(watermark, 2);
        // Output queued immediately after subscription must not leak into the snapshot.
        let suffix = b"\xad\x1b[0mtail";
        telemetry(&mut actor, output("term", suffix)).await;
        telemetry(
            &mut actor,
            ToolEvent::TerminalExited {
                terminal_id: "term".into(),
                exit_code: None,
                status: "killed".into(),
            },
        )
        .await;
        assert!(actor.snapshot().live_terminal_ids.is_empty());
        let mut subscription =
            crate::events::subscription(snapshot, source, actor.record.info.id.clone());
        let event = tokio::time::timeout(Duration::from_secs(1), subscription.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.seq, watermark + 1);
        let SessionEventPayload::TerminalOutput { data, .. } = event.payload else {
            panic!("expected output")
        };
        let mut restored = prefix.to_vec();
        restored.extend(data);
        assert_eq!(restored, [prefix.as_slice(), suffix.as_slice()].concat());
        let exit = tokio::time::timeout(Duration::from_secs(1), subscription.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exit.seq, watermark + 2);
        assert!(matches!(
            exit.payload,
            SessionEventPayload::TerminalExited { .. }
        ));
        assert!(subscription.events.try_recv().is_err());
        let first = actor.snapshot();
        let second = actor.snapshot();
        assert_eq!(
            first.terminal_views[0].output,
            encode_terminal_bytes(&restored.into())
        );
        assert_eq!(
            first.terminal_views[0].output,
            second.terminal_views[0].output
        );
        assert_eq!(first.terminal_views[0].status, "killed");
        assert_eq!(first.terminal_views[0].exit_code, None);
    }

    #[tokio::test]
    async fn older_snapshot_without_terminal_views_still_deserializes() {
        let mut actor = actor();
        telemetry(&mut actor, opened("term")).await;
        telemetry(&mut actor, output("term", &[0xff, 0, 0xe4, 0xb8])).await;
        let mut value = serde_json::to_value(actor.snapshot()).unwrap();
        let restored: SessionSnapshot = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            restored.terminal_views[0].output,
            encode_terminal_bytes(&VecDeque::from([0xff, 0, 0xe4, 0xb8]))
        );
        value.as_object_mut().unwrap().remove("terminal_views");
        let legacy: SessionSnapshot = serde_json::from_value(value).unwrap();
        assert!(legacy.terminal_views.is_empty());
    }

    #[test]
    fn cache_bounds_bytes_without_decoding_or_losing_drop_counts() {
        let mut cache = TerminalViewCache::default();
        cache.observe(&opened("term"));
        cache.observe(&output("term", b"old"));
        let mut large = vec![b'x'; TERMINAL_VIEW_BYTES + 2];
        large[2..6].copy_from_slice(&[0xb8, 0xad, 0xff, 0x1b]);
        cache.observe(&output("term", &large));
        let view = &cache.views[0];
        assert_eq!(view.output, &large[2..]);
        assert_eq!(view.dropped_bytes, 5);
        assert!(view.output.capacity() <= TERMINAL_VIEW_BYTES);
        // Snapshots are independent replacements; reading one consumes nothing.
        let snapshot = cache.snapshot();
        cache.observe(&output("term", b"tail"));
        assert_eq!(
            snapshot[0].output,
            encode_terminal_bytes(&VecDeque::from(large[2..].to_vec()))
        );
        assert_eq!(cache.views[0].dropped_bytes, 9);
    }

    #[test]
    fn cache_bounds_total_output_and_keeps_trimmed_terminal_metadata() {
        let mut cache = TerminalViewCache::default();
        let count = TERMINAL_VIEWS_TOTAL_BYTES / TERMINAL_VIEW_BYTES + 1;
        for index in 0..count {
            let id = index.to_string();
            cache.observe(&opened(&id));
            cache.observe(&output(&id, &vec![index as u8; TERMINAL_VIEW_BYTES]));
        }
        assert_eq!(cache.views.len(), count);
        assert_eq!(
            cache
                .views
                .iter()
                .map(|view| view.output.len())
                .sum::<usize>(),
            TERMINAL_VIEWS_TOTAL_BYTES
        );
        assert_eq!(
            cache
                .views
                .iter()
                .map(|view| view.output.capacity())
                .sum::<usize>(),
            TERMINAL_VIEWS_TOTAL_BYTES
        );
        assert!(cache.views[0].output.is_empty());
        assert_eq!(cache.views[0].dropped_bytes, TERMINAL_VIEW_BYTES as u64);
        cache.observe(&output("0", b"new"));
        assert_eq!(cache.views.back().unwrap().output, b"new".as_slice());
        assert_eq!(cache.views[0].terminal_id, "1");
        assert_eq!(cache.views[0].dropped_bytes, 3);
    }

    #[test]
    fn cache_evicts_exited_views_first_and_bounds_metadata() {
        let mut cache = TerminalViewCache::default();
        for index in 0..TERMINAL_VIEW_COUNT {
            cache.observe(&opened(&index.to_string()));
        }
        cache.observe(&ToolEvent::TerminalExited {
            terminal_id: "1".into(),
            exit_code: Some(7),
            status: "error".into(),
        });
        assert_eq!(cache.views.back().unwrap().exit_code, Some(7));
        cache.observe(&opened("new"));
        assert_eq!(cache.views.len(), TERMINAL_VIEW_COUNT);
        assert!(cache.views.iter().any(|view| view.terminal_id == "0"));
        assert!(!cache.views.iter().any(|view| view.terminal_id == "1"));
        cache.observe(&opened("newer"));
        assert!(!cache.views.iter().any(|view| view.terminal_id == "0"));
        // Late telemetry cannot resurrect an evicted view with a false byte offset.
        cache.observe(&output("0", b"late"));
        assert!(!cache.views.iter().any(|view| view.terminal_id == "0"));
        let mut oversized = opened("oversized");
        if let ToolEvent::TerminalOpened { command, .. } = &mut oversized {
            *command = "x".repeat(TERMINAL_VIEW_METADATA_BYTES + 1);
        }
        cache.observe(&oversized);
        assert!(
            !cache
                .views
                .iter()
                .any(|view| view.terminal_id == "oversized")
        );
    }

    #[test]
    fn base64_encodes_padding_binary_and_wrapped_deques() {
        for (bytes, encoded) in [
            (b"".as_slice(), ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
            (&[0xff, 0, 0xe4, 0xb8], "/wDkuA=="),
        ] {
            assert_eq!(
                encode_terminal_bytes(&VecDeque::from(bytes.to_vec())),
                encoded
            );
        }
        let mut wrapped = VecDeque::with_capacity(6);
        wrapped.extend(b"xxxfoo");
        wrapped.drain(..3);
        wrapped.extend(b"bar");
        assert!(!wrapped.as_slices().1.is_empty());
        assert_eq!(encode_terminal_bytes(&wrapped), "Zm9vYmFy");
    }

    #[test]
    fn terminal_snapshot_wire_budget_leaves_room_below_four_mib() {
        let mut cache = TerminalViewCache::default();
        for index in 0..TERMINAL_VIEW_COUNT {
            let id = index.to_string();
            let mut event = opened(&id);
            if let ToolEvent::TerminalOpened { command, .. } = &mut event {
                // Control characters require six bytes each in JSON strings.
                *command = "\0".repeat(8 * 1024);
            }
            cache.observe(&event);
            cache.observe(&output(&id, &vec![0xff; TERMINAL_VIEW_BYTES]));
        }
        assert!(cache.views.len() < TERMINAL_VIEW_COUNT);
        assert_eq!(
            cache
                .views
                .iter()
                .map(|view| view.output.len())
                .sum::<usize>(),
            TERMINAL_VIEWS_TOTAL_BYTES
        );
        let wire = serde_json::to_vec(&cache.snapshot()).unwrap();
        assert!(
            wire.len() < 2 * 1024 * 1024,
            "terminal views used {} bytes",
            wire.len()
        );
    }
}

#[cfg(test)]
mod reasoning_mode_tests {
    use super::middle_reasoning_mode;

    fn modes(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn middle_mode_uses_the_center_or_upper_center() {
        assert_eq!(
            middle_reasoning_mode(&modes(&["low", "medium", "high", "xhigh", "max"])),
            Some("high".to_string())
        );
        assert_eq!(
            middle_reasoning_mode(&modes(&["low", "medium", "high", "xhigh"])),
            Some("high".to_string())
        );
        assert_eq!(
            middle_reasoning_mode(&modes(&["nonthink", "auto", "high", "max"])),
            Some("high".to_string())
        );
    }
}

fn title_source(content: &MessageContent) -> Option<String> {
    content.as_blocks().iter().find_map(|block| {
        let ContentBlock::Text { text, .. } = block else {
            return None;
        };
        let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
        (!normalized.is_empty()).then_some(normalized)
    })
}

fn title_source_excerpt(source: &str) -> String {
    const TITLE_SOURCE_CHARS: usize = 2_000;
    source.chars().take(TITLE_SOURCE_CHARS).collect()
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn clean_generated_session_title(raw: &str) -> Option<String> {
    const GENERATED_TITLE_CHARS: usize = 40;

    let first_line = raw.lines().find(|line| !line.trim().is_empty())?.trim();
    let title = first_line
        .strip_prefix("Title:")
        .or_else(|| first_line.strip_prefix("title:"))
        .or_else(|| first_line.strip_prefix("标题:"))
        .or_else(|| first_line.strip_prefix("标题："))
        .unwrap_or(first_line)
        .trim()
        .trim_matches(|character| matches!(character, '"' | '\'' | '`' | '“' | '”' | '‘' | '’'))
        .trim()
        .trim_end_matches(['.', '。'])
        .trim();
    if title.is_empty() {
        return None;
    }
    Some(title.chars().take(GENERATED_TITLE_CHARS).collect())
}

#[cfg(test)]
mod title_tests {
    use super::*;

    #[test]
    fn title_source_uses_first_non_empty_text_block() {
        let content = MessageContent::blocks(vec![
            ContentBlock::image("image/png", "aGVsbG8="),
            ContentBlock::text("  investigate\n flaky tests  "),
        ]);
        assert_eq!(
            title_source(&content).as_deref(),
            Some("investigate flaky tests")
        );
    }

    #[test]
    fn generated_title_is_unwrapped_and_bounded() {
        assert_eq!(
            clean_generated_session_title("Title: \"Investigate flaky tests.\"\nextra").as_deref(),
            Some("Investigate flaky tests")
        );
        assert_eq!(
            clean_generated_session_title(&"x".repeat(80))
                .unwrap()
                .chars()
                .count(),
            40
        );
    }
}
