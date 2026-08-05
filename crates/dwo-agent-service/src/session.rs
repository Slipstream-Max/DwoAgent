use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use dwo_context::{
    ContentBlock, ContextManager, ContextMessage, MessageContent, MessageKind,
    PendingContextMessage, PendingMessageBatch, SystemPromptBuilder,
};
use dwo_model_client::{ModelClient, ModelReply, ModelSelection};
use dwo_tools::{ConfirmationDecision, ToolManager};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::TurnId;
use crate::agent_loop::{self, RunTurn, TurnActorMessage, TurnEvent, TurnOutcome};
use crate::error::AgentServiceError;
use crate::events::{
    ActiveStepSnapshot, ActiveToolCall, ClientTranscriptEvent, FileChange, RuntimePhase,
    SessionEvent, SessionEventPayload, SessionSnapshot, SessionSubscription, SessionUsageSnapshot,
};
use crate::permission::{PermissionRequestEnvelope, PermissionRequester, PermissionState};
use crate::record::{SessionConfig, SessionConfigUpdate, SessionRecord, title_from_user_content};
use crate::repository::SessionRepository;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EndpointId(String);

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

pub struct SessionAgent {
    id: crate::SessionId,
    control: mpsc::Sender<Control>,
}

impl SessionAgent {
    pub(crate) fn spawn(
        record: SessionRecord,
        transcript: Vec<ClientTranscriptEvent>,
        repository: Arc<dyn SessionRepository>,
        model: Arc<dyn ModelClient>,
        tools: Arc<ToolManager>,
        prompt_builder: SystemPromptBuilder,
    ) -> Arc<Self> {
        let id = record.info.id.clone();
        let (control_tx, control_rx) = mpsc::channel(128);
        let (turn_tx, turn_rx) = mpsc::unbounded_channel();
        let (permission_tx, permission_rx) = mpsc::unbounded_channel();
        let (config_tx, _) = watch::channel(record.config());
        let (events, _) = broadcast::channel(1024);
        let actor = SessionActor {
            record,
            transcript,
            repository,
            model,
            tools,
            prompt_builder,
            controls: control_rx,
            turn_tx,
            turn_messages: turn_rx,
            permission_tx,
            permission_requests: permission_rx,
            config_tx,
            events,
            seq: 0,
            phase: RuntimePhase::Idle,
            active: None,
            permission: PermissionState::default(),
            pending_messages: VecDeque::new(),
            closing_response: None,
            title_cancellation: None,
        };
        tokio::spawn(actor.run());
        Arc::new(Self {
            id,
            control: control_tx,
        })
    }

    pub fn id(&self) -> &crate::SessionId {
        &self.id
    }

    pub async fn snapshot(&self) -> Result<SessionSnapshot, AgentServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(Control::Snapshot { response }).await?;
        wait.await
            .map_err(|_| AgentServiceError::SessionClosed(self.id.clone()))
    }

    pub async fn attach(
        &self,
        endpoint: EndpointId,
    ) -> Result<SessionSubscription, AgentServiceError> {
        self.attach_from(endpoint, None).await
    }

    pub async fn attach_from(
        &self,
        _endpoint: EndpointId,
        checkpoint_cursor: Option<usize>,
    ) -> Result<SessionSubscription, AgentServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(Control::Attach {
            checkpoint_cursor,
            response,
        })
        .await?;
        let (snapshot, mut source) = wait
            .await
            .map_err(|_| AgentServiceError::SessionClosed(self.id.clone()))?;
        let watermark = snapshot.seq;
        let (events, receiver) = mpsc::channel(256);
        let event_session_id = self.id.clone();
        let mut active_step = snapshot.active_step.clone();
        let mut step_seq = watermark;
        tokio::spawn(async move {
            let mut needs_step_snapshot = false;
            loop {
                match source.recv().await {
                    Ok(event) if event.seq > watermark => {
                        if apply_step_delta(&mut active_step, &event.payload) {
                            step_seq = event.seq;
                            if needs_step_snapshot {
                                match try_send_step_snapshot(
                                    &events,
                                    &event.session_id,
                                    step_seq,
                                    active_step.as_ref(),
                                ) {
                                    StepSnapshotSend::Sent => needs_step_snapshot = false,
                                    StepSnapshotSend::Full => continue,
                                    StepSnapshotSend::Closed => break,
                                }
                                continue;
                            }
                            match events.try_send(event) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    needs_step_snapshot = true;
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => break,
                            }
                            continue;
                        }
                        if needs_step_snapshot {
                            if let Some(step) = active_step.as_ref()
                                && events
                                    .send(SessionEvent {
                                        seq: step_seq,
                                        session_id: event.session_id.clone(),
                                        payload: SessionEventPayload::StepSnapshot {
                                            step: step.clone(),
                                        },
                                    })
                                    .await
                                    .is_err()
                            {
                                break;
                            }
                            needs_step_snapshot = false;
                        }
                        update_step_checkpoint(&mut active_step, &event.payload);
                        if events.send(event).await.is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            event = "session.subscription_lagged",
                            session_id = %event_session_id,
                            skipped,
                            "disconnect lagging session subscription; client must resync"
                        );
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Ok(SessionSubscription {
            snapshot,
            events: receiver,
        })
    }

    pub async fn prompt(
        &self,
        origin: EndpointId,
        content: impl Into<String>,
    ) -> Result<PromptAccepted, AgentServiceError> {
        self.prompt_content(origin, MessageContent::text(content))
            .await
    }

    pub async fn prompt_content(
        &self,
        origin: EndpointId,
        content: MessageContent,
    ) -> Result<PromptAccepted, AgentServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(Control::Prompt {
            origin,
            content,
            only_if_idle: false,
            response,
        })
        .await?;
        wait.await
            .map_err(|_| AgentServiceError::SessionClosed(self.id.clone()))?
    }

    pub async fn prompt_idle(
        &self,
        origin: EndpointId,
        content: impl Into<String>,
    ) -> Result<PromptAccepted, AgentServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(Control::Prompt {
            origin,
            content: MessageContent::text(content),
            only_if_idle: true,
            response,
        })
        .await?;
        wait.await
            .map_err(|_| AgentServiceError::SessionClosed(self.id.clone()))?
    }

    pub async fn compact(&self, origin: EndpointId) -> Result<PromptAccepted, AgentServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(Control::Compact { origin, response }).await?;
        wait.await
            .map_err(|_| AgentServiceError::SessionClosed(self.id.clone()))?
    }

    pub async fn resume(
        &self,
        origin: EndpointId,
    ) -> Result<Option<PromptAccepted>, AgentServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(Control::Resume { origin, response }).await?;
        wait.await
            .map_err(|_| AgentServiceError::SessionClosed(self.id.clone()))?
    }

    pub async fn append_internal(
        &self,
        turn_id: TurnId,
        content: impl Into<String>,
    ) -> Result<(), AgentServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(Control::AppendInternal {
            turn_id,
            content: MessageContent::text(content),
            response,
        })
        .await?;
        wait.await
            .map_err(|_| AgentServiceError::SessionClosed(self.id.clone()))?
    }

    pub async fn cancel(&self, expected_turn_id: Option<TurnId>) -> Result<(), AgentServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(Control::Cancel {
            expected_turn_id,
            response,
        })
        .await?;
        wait.await
            .map_err(|_| AgentServiceError::SessionClosed(self.id.clone()))?
    }

    pub async fn set_config(&self, update: SessionConfigUpdate) -> Result<(), AgentServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(Control::SetConfig { update, response }).await?;
        wait.await
            .map_err(|_| AgentServiceError::SessionClosed(self.id.clone()))?
    }

    pub async fn notify_internal(
        &self,
        content: impl Into<String>,
    ) -> Result<Option<TurnId>, AgentServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(Control::InternalMessage {
            content: MessageContent::text(content),
            wake: true,
            response,
        })
        .await?;
        wait.await
            .map_err(|_| AgentServiceError::SessionClosed(self.id.clone()))?
    }

    pub async fn respond_permission(
        &self,
        origin: EndpointId,
        request_id: String,
        decision: ConfirmationDecision,
    ) -> Result<(), AgentServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(Control::RespondPermission {
            origin,
            request_id,
            decision,
            response,
        })
        .await?;
        wait.await
            .map_err(|_| AgentServiceError::SessionClosed(self.id.clone()))?
    }

    pub async fn close(&self) -> Result<(), AgentServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(Control::Close { response }).await?;
        wait.await
            .map_err(|_| AgentServiceError::SessionClosed(self.id.clone()))?
    }

    async fn send(&self, control: Control) -> Result<(), AgentServiceError> {
        self.control
            .send(control)
            .await
            .map_err(|_| AgentServiceError::SessionClosed(self.id.clone()))
    }
}

enum Control {
    Attach {
        checkpoint_cursor: Option<usize>,
        response: oneshot::Sender<(SessionSnapshot, broadcast::Receiver<SessionEvent>)>,
    },
    Snapshot {
        response: oneshot::Sender<SessionSnapshot>,
    },
    Prompt {
        origin: EndpointId,
        content: MessageContent,
        only_if_idle: bool,
        response: oneshot::Sender<Result<PromptAccepted, AgentServiceError>>,
    },
    Compact {
        origin: EndpointId,
        response: oneshot::Sender<Result<PromptAccepted, AgentServiceError>>,
    },
    Resume {
        origin: EndpointId,
        response: oneshot::Sender<Result<Option<PromptAccepted>, AgentServiceError>>,
    },
    AppendInternal {
        turn_id: TurnId,
        content: MessageContent,
        response: oneshot::Sender<Result<(), AgentServiceError>>,
    },
    Cancel {
        expected_turn_id: Option<TurnId>,
        response: oneshot::Sender<Result<(), AgentServiceError>>,
    },
    SetConfig {
        update: SessionConfigUpdate,
        response: oneshot::Sender<Result<(), AgentServiceError>>,
    },
    InternalMessage {
        content: MessageContent,
        wake: bool,
        response: oneshot::Sender<Result<Option<TurnId>, AgentServiceError>>,
    },
    RespondPermission {
        origin: EndpointId,
        request_id: String,
        decision: ConfirmationDecision,
        response: oneshot::Sender<Result<(), AgentServiceError>>,
    },
    Close {
        response: oneshot::Sender<Result<(), AgentServiceError>>,
    },
}

struct ActiveTurn {
    id: TurnId,
    kind: ActiveTurnKind,
    cancellation: CancellationToken,
    partial_message: String,
    partial_reasoning: String,
    step_id: u64,
    step_revision: u64,
    message_id: MessageId,
    thought_message_id: MessageId,
    tools: Vec<ActiveToolCall>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveTurnKind {
    Agent,
    ManualCompaction,
}

struct SessionActor {
    record: SessionRecord,
    transcript: Vec<ClientTranscriptEvent>,
    repository: Arc<dyn SessionRepository>,
    model: Arc<dyn ModelClient>,
    tools: Arc<ToolManager>,
    prompt_builder: SystemPromptBuilder,
    controls: mpsc::Receiver<Control>,
    turn_tx: mpsc::UnboundedSender<TurnActorMessage>,
    turn_messages: mpsc::UnboundedReceiver<TurnActorMessage>,
    permission_tx: mpsc::UnboundedSender<PermissionRequestEnvelope>,
    permission_requests: mpsc::UnboundedReceiver<PermissionRequestEnvelope>,
    config_tx: watch::Sender<SessionConfig>,
    events: broadcast::Sender<SessionEvent>,
    seq: u64,
    phase: RuntimePhase,
    active: Option<ActiveTurn>,
    permission: PermissionState,
    pending_messages: VecDeque<PendingMessage>,
    closing_response: Option<oneshot::Sender<Result<(), AgentServiceError>>>,
    title_cancellation: Option<CancellationToken>,
}

enum PendingMessage {
    User {
        content: MessageContent,
    },
    Internal {
        content: MessageContent,
        wake: bool,
        response: oneshot::Sender<Result<Option<TurnId>, AgentServiceError>>,
    },
    StepInternal {
        content: MessageContent,
    },
}

impl SessionActor {
    async fn run(mut self) {
        loop {
            tokio::select! {
                biased;
                control = self.controls.recv() => {
                    let Some(control) = control else { break };
                    if self.handle_control(control).await {
                        break;
                    }
                }
                message = self.turn_messages.recv() => {
                    let Some(message) = message else { break };
                    if self.handle_turn_message(message).await {
                        break;
                    }
                }
                request = self.permission_requests.recv() => {
                    let Some(request) = request else { break };
                    self.handle_permission_request(request);
                }
            }
        }
        if let Some(active) = self.active.take() {
            active.cancellation.cancel();
        }
        if let Some(cancellation) = self.title_cancellation.take() {
            cancellation.cancel();
        }
        self.permission.reject("session closed");
        self.tools.shutdown().await;
    }

    async fn handle_control(&mut self, control: Control) -> bool {
        match control {
            Control::Attach {
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
            Control::Snapshot { response } => {
                let _ = response.send(self.snapshot());
            }
            Control::Prompt {
                origin,
                content,
                only_if_idle,
                response,
            } => {
                if self.phase == RuntimePhase::Closing {
                    let _ = response.send(Err(AgentServiceError::SessionClosed(
                        self.record.info.id.clone(),
                    )));
                    return false;
                }
                if self.phase == RuntimePhase::Cancelling {
                    let error = if only_if_idle {
                        AgentServiceError::SessionBusy(self.record.info.id.clone())
                    } else {
                        AgentServiceError::PromptCancelled(self.record.info.id.clone())
                    };
                    let _ = response.send(Err(error));
                    return false;
                }
                if self.active.is_none() {
                    let result = self.start_prompt(origin, content).await;
                    let _ = response.send(result);
                    return false;
                }
                if self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.kind == ActiveTurnKind::ManualCompaction)
                {
                    let _ = response.send(Err(AgentServiceError::SessionBusy(
                        self.record.info.id.clone(),
                    )));
                    return false;
                }
                if only_if_idle {
                    let _ = response.send(Err(AgentServiceError::SessionBusy(
                        self.record.info.id.clone(),
                    )));
                    return false;
                }
                if let Err(error) = self.validate_message_content(&content) {
                    let _ = response.send(Err(error));
                    return false;
                }
                let turn_id = self
                    .active
                    .as_ref()
                    .expect("active prompt has a turn")
                    .id
                    .clone();
                let message_id = MessageId::new();
                self.emit_client_event(SessionEventPayload::UserPromptSubmitted {
                    message_id: message_id.clone(),
                    turn_id: turn_id.clone(),
                    origin,
                    content: content.clone(),
                })
                .await;
                self.pending_messages
                    .push_back(PendingMessage::User { content });
                let _ = response.send(Ok(PromptAccepted {
                    message_id,
                    turn_id,
                }));
            }
            Control::Compact { origin, response } => {
                if self.phase == RuntimePhase::Closing {
                    let _ = response.send(Err(AgentServiceError::SessionClosed(
                        self.record.info.id.clone(),
                    )));
                    return false;
                }
                if self.phase == RuntimePhase::Cancelling {
                    let _ = response.send(Err(AgentServiceError::PromptCancelled(
                        self.record.info.id.clone(),
                    )));
                    return false;
                }
                if self.active.is_some() {
                    let _ = response.send(Err(AgentServiceError::SessionBusy(
                        self.record.info.id.clone(),
                    )));
                    return false;
                }
                let result = self.start_manual_compaction(origin).await;
                let _ = response.send(result);
            }
            Control::Resume { origin, response } => {
                if self.phase == RuntimePhase::Closing {
                    let _ = response.send(Err(AgentServiceError::SessionClosed(
                        self.record.info.id.clone(),
                    )));
                    return false;
                }
                if self.active.is_some() || self.phase != RuntimePhase::Idle {
                    let _ = response.send(Ok(None));
                    return false;
                }
                let result = self.start_resume(origin).await.map(Some);
                let _ = response.send(result);
            }
            Control::AppendInternal {
                turn_id,
                content,
                response,
            } => {
                if self
                    .active
                    .as_ref()
                    .is_none_or(|active| active.id != turn_id)
                {
                    let _ = response.send(Err(AgentServiceError::TurnNotActive(turn_id)));
                    return false;
                }
                self.pending_messages
                    .push_back(PendingMessage::StepInternal { content });
                let _ = response.send(Ok(()));
            }
            Control::Cancel {
                expected_turn_id,
                response,
            } => {
                let Some(active) = &self.active else {
                    let missing = expected_turn_id.unwrap_or_default();
                    let _ = response.send(Err(AgentServiceError::TurnNotActive(missing)));
                    return false;
                };
                if let Some(expected) = expected_turn_id
                    && expected != active.id
                {
                    let _ = response.send(Err(AgentServiceError::TurnNotActive(expected)));
                    return false;
                }
                active.cancellation.cancel();
                self.phase = RuntimePhase::Cancelling;
                self.permission.reject("turn cancelled");
                let tools = self.tools.clone();
                tokio::spawn(async move { tools.shutdown().await });
                let _ = response.send(Ok(()));
            }
            Control::SetConfig { update, response } => {
                if self.phase == RuntimePhase::Closing {
                    let _ = response.send(Err(AgentServiceError::SessionClosed(
                        self.record.info.id.clone(),
                    )));
                    return false;
                }
                let model_changed = matches!(&update, SessionConfigUpdate::Model(_));
                let previous_model = self.record.llm.model.clone();
                let new_model_reasoning = match &update {
                    SessionConfigUpdate::Model(model) => self
                        .model
                        .reasoning_modes(model)
                        .ok()
                        .and_then(|modes| middle_reasoning_mode(&modes)),
                    _ => None,
                };
                let mut updated = self.record.clone();
                let result = updated
                    .apply_config(update, new_model_reasoning)
                    .map_err(AgentServiceError::InvalidConfig);
                let result = match result {
                    Ok(()) => {
                        let mut selection = ModelSelection {
                            model: updated.llm.model.clone(),
                            reasoning: updated.llm.reasoning.clone(),
                        };
                        let mut remember_reasoning = true;
                        let validation =
                            if let Err(error) = self.model.validate_selection(&selection) {
                                if !model_changed || updated.llm.reasoning.is_none() {
                                    Err(AgentServiceError::InvalidConfig(error.to_string()))
                                } else {
                                    // A remembered mode may not exist on the target model.
                                    remember_reasoning = !updated
                                        .llm
                                        .reasoning_by_model
                                        .contains_key(&updated.llm.model);
                                    updated.llm.reasoning = None;
                                    selection.reasoning = None;
                                    self.model.validate_selection(&selection).map_err(|_| {
                                        AgentServiceError::InvalidConfig(error.to_string())
                                    })
                                }
                            } else {
                                Ok(())
                            };
                        match validation {
                            Ok(()) => {
                                let normalized = if model_changed {
                                    self.normalize_record_context_for_model(
                                        &mut updated,
                                        &previous_model,
                                    )
                                } else {
                                    Ok(())
                                };
                                match normalized {
                                    Ok(()) => {
                                        if remember_reasoning {
                                            updated.llm.remember_current_reasoning();
                                        }
                                        updated.touch();
                                        self.repository
                                            .save(&updated)
                                            .await
                                            .map_err(AgentServiceError::from)
                                    }
                                    Err(error) => Err(error),
                                }
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                };
                if result.is_ok() {
                    self.record = updated;
                    let config = self.record.config();
                    self.config_tx.send_replace(config.clone());
                    self.emit(SessionEventPayload::ConfigChanged { config });
                    if model_changed {
                        self.emit_usage_changed();
                    }
                }
                let _ = response.send(result);
            }
            Control::InternalMessage {
                content,
                wake,
                response,
            } => {
                if self.phase == RuntimePhase::Closing {
                    let _ = response.send(Err(AgentServiceError::SessionClosed(
                        self.record.info.id.clone(),
                    )));
                    return false;
                }
                if self.active.is_some() {
                    self.pending_messages.push_back(PendingMessage::Internal {
                        content,
                        wake,
                        response,
                    });
                } else if wake {
                    let result = self.start_internal(content).await.map(Some);
                    let _ = response.send(result);
                } else {
                    let result = self.append_internal_idle(content).await.map(|()| None);
                    let _ = response.send(result);
                }
            }
            Control::RespondPermission {
                origin: responder,
                request_id,
                decision,
                response,
            } => {
                let allowed = decision.allowed;
                let reason = decision.reason.clone();
                let result = self.permission.respond(&request_id, decision);
                let result = result.map(|resolved| {
                    self.phase = RuntimePhase::Running;
                    self.emit(SessionEventPayload::PermissionResolved {
                        turn_id: resolved.turn_id,
                        request_id: resolved.request_id,
                        responder,
                        allowed,
                        reason,
                    });
                });
                let _ = response.send(result);
            }
            Control::Close { response } => {
                if self.phase == RuntimePhase::Closing {
                    let _ = response.send(Err(AgentServiceError::SessionClosed(
                        self.record.info.id.clone(),
                    )));
                    return false;
                }
                self.phase = RuntimePhase::Closing;
                if let Some(cancellation) = self.title_cancellation.take() {
                    cancellation.cancel();
                }
                self.reject_pending_messages();
                self.emit(SessionEventPayload::Closing);
                if let Some(active) = &self.active {
                    active.cancellation.cancel();
                    self.permission.reject("session closed");
                    let tools = self.tools.clone();
                    tokio::spawn(async move { tools.shutdown().await });
                    self.closing_response = Some(response);
                    return false;
                }
                self.permission.reject("session closed");
                self.tools.shutdown().await;
                let result = self.repository.save(&self.record).await.map_err(Into::into);
                let _ = response.send(result);
                return true;
            }
        }
        false
    }

    async fn start_prompt(
        &mut self,
        origin: EndpointId,
        content: MessageContent,
    ) -> Result<PromptAccepted, AgentServiceError> {
        if self.phase == RuntimePhase::Closing {
            return Err(AgentServiceError::SessionClosed(
                self.record.info.id.clone(),
            ));
        }
        self.validate_message_content(&content)?;
        let previous_usage = self.usage_snapshot();
        let turn_id = TurnId::new();
        let message_id = MessageId::new();
        let event_content = content.clone();
        let repaired_title = self
            .record
            .info
            .title
            .trim()
            .is_empty()
            .then(|| title_from_user_content(&content))
            .flatten();
        if let Some(title) = &repaired_title {
            self.record.set_automatic_title(title.clone());
        }
        let title_generation = if repaired_title.is_none()
            && self.record.auto_title_pending()
            && self.title_cancellation.is_none()
        {
            title_source(&content).map(|source| (source, self.record.info.title.clone()))
        } else {
            None
        };
        let mut context = ContextManager::new(self.record.context.clone());
        context.append_user(content);
        context.refresh_usage(self.tools.schemas());
        self.record.context = context.into_context();
        self.record.touch();
        self.repository.save(&self.record).await?;
        if let Some((source, original_title)) = title_generation {
            self.start_title_generation(source, original_title);
        }
        self.emit_client_event(SessionEventPayload::UserPromptSubmitted {
            message_id: message_id.clone(),
            turn_id: turn_id.clone(),
            origin,
            content: event_content,
        })
        .await;
        if let Some(title) = repaired_title {
            self.emit(SessionEventPayload::TitleChanged {
                title,
                updated_at_ms: self.record.info.updated_at_ms,
            });
        }
        if self.usage_snapshot() != previous_usage {
            self.emit_usage_changed();
        }
        self.activate_turn(turn_id.clone());
        Ok(PromptAccepted {
            message_id,
            turn_id,
        })
    }

    async fn start_manual_compaction(
        &mut self,
        origin: EndpointId,
    ) -> Result<PromptAccepted, AgentServiceError> {
        let turn_id = TurnId::new();
        let message_id = MessageId::new();
        self.record.touch();
        self.repository.save(&self.record).await?;
        self.emit_client_event(SessionEventPayload::UserPromptSubmitted {
            message_id: message_id.clone(),
            turn_id: turn_id.clone(),
            origin,
            content: MessageContent::text("/compact"),
        })
        .await;
        let turn = self.prepare_turn(turn_id.clone(), ActiveTurnKind::ManualCompaction);
        tokio::spawn(agent_loop::run_manual_compaction(turn));
        Ok(PromptAccepted {
            message_id,
            turn_id,
        })
    }

    async fn start_resume(
        &mut self,
        origin: EndpointId,
    ) -> Result<PromptAccepted, AgentServiceError> {
        let previous_usage = self.usage_snapshot();
        let turn_id = TurnId::new();
        let message_id = MessageId::new();
        let mut context = ContextManager::new(self.record.context.clone());
        context.append_internal(
            MessageKind::Runtime,
            "<resume>Continue the previous task from the current session state.</resume>",
        );
        context.refresh_usage(self.tools.schemas());
        self.record.context = context.into_context();
        self.record.touch();
        self.repository.save(&self.record).await?;
        self.emit_client_event(SessionEventPayload::UserPromptSubmitted {
            message_id: message_id.clone(),
            turn_id: turn_id.clone(),
            origin,
            content: MessageContent::text("/resume"),
        })
        .await;
        if self.usage_snapshot() != previous_usage {
            self.emit_usage_changed();
        }
        self.activate_turn(turn_id.clone());
        Ok(PromptAccepted {
            message_id,
            turn_id,
        })
    }

    async fn start_internal(
        &mut self,
        content: MessageContent,
    ) -> Result<TurnId, AgentServiceError> {
        if self.phase == RuntimePhase::Closing {
            return Err(AgentServiceError::SessionClosed(
                self.record.info.id.clone(),
            ));
        }
        let previous_usage = self.usage_snapshot();
        let turn_id = TurnId::new();
        let mut context = ContextManager::new(self.record.context.clone());
        context.append_internal(MessageKind::Runtime, content);
        context.refresh_usage(self.tools.schemas());
        self.record.context = context.into_context();
        self.record.touch();
        self.repository.save(&self.record).await?;
        if self.usage_snapshot() != previous_usage {
            self.emit_usage_changed();
        }
        self.activate_turn(turn_id.clone());
        Ok(turn_id)
    }

    async fn append_internal_idle(
        &mut self,
        content: MessageContent,
    ) -> Result<(), AgentServiceError> {
        let previous_usage = self.usage_snapshot();
        let mut context = ContextManager::new(self.record.context.clone());
        context.append_internal(MessageKind::Runtime, content);
        context.refresh_usage(self.tools.schemas());
        self.record.context = context.into_context();
        self.record.touch();
        self.repository.save(&self.record).await?;
        if self.usage_snapshot() != previous_usage {
            self.emit_usage_changed();
        }
        Ok(())
    }

    async fn append_user_idle(&mut self, content: MessageContent) -> Result<(), AgentServiceError> {
        let previous_usage = self.usage_snapshot();
        let mut context = ContextManager::new(self.record.context.clone());
        context.append_user(content);
        context.refresh_usage(self.tools.schemas());
        self.record.context = context.into_context();
        self.record.touch();
        self.repository.save(&self.record).await?;
        if self.usage_snapshot() != previous_usage {
            self.emit_usage_changed();
        }
        Ok(())
    }

    fn activate_turn(&mut self, turn_id: TurnId) {
        let turn = self.prepare_turn(turn_id, ActiveTurnKind::Agent);
        tokio::spawn(agent_loop::run(turn));
    }

    fn prepare_turn(&mut self, turn_id: TurnId, kind: ActiveTurnKind) -> RunTurn {
        let cancellation = CancellationToken::new();
        self.active = Some(ActiveTurn {
            id: turn_id.clone(),
            kind,
            cancellation: cancellation.clone(),
            partial_message: String::new(),
            partial_reasoning: String::new(),
            step_id: 1,
            step_revision: 0,
            message_id: MessageId::new(),
            thought_message_id: MessageId::new(),
            tools: Vec::new(),
        });
        self.phase = RuntimePhase::Running;
        self.emit(SessionEventPayload::TurnStarted {
            turn_id: turn_id.clone(),
        });
        let permission = PermissionRequester::new(
            turn_id.clone(),
            cancellation.clone(),
            self.permission_tx.clone(),
        );
        RunTurn {
            session_id: self.record.info.id.clone(),
            turn_id,
            context: ContextManager::new(self.record.context.clone()),
            prompt_builder: self.prompt_builder.clone(),
            model: self.model.clone(),
            tools: self.tools.clone(),
            config: self.config_tx.subscribe(),
            permission,
            cancellation,
            actor: self.turn_tx.clone(),
        }
    }

    fn validate_message_content(&self, content: &MessageContent) -> Result<(), AgentServiceError> {
        if content.contains_images()
            && !self
                .model
                .supports_image_input(&self.record.llm.model)
                .map_err(|error| AgentServiceError::InvalidConfig(error.to_string()))?
        {
            return Err(AgentServiceError::InvalidConfig(format!(
                "model {} does not support image input",
                self.record.llm.model
            )));
        }
        Ok(())
    }

    fn normalize_record_context_for_model(
        &self,
        record: &mut SessionRecord,
        previous_model: &str,
    ) -> Result<(), AgentServiceError> {
        let provider = self
            .model
            .provider_id(&record.llm.model)
            .map_err(|error| AgentServiceError::InvalidConfig(error.to_string()))?;
        let previous_provider = match record.context.provider.clone() {
            Some(provider) => provider,
            None => self
                .model
                .provider_id(previous_model)
                .map_err(|error| AgentServiceError::InvalidConfig(error.to_string()))?,
        };
        let allow_image_input = self
            .model
            .supports_image_input(&record.llm.model)
            .map_err(|error| AgentServiceError::InvalidConfig(error.to_string()))?;
        let mut context = ContextManager::new(record.context.clone());
        context.normalize_for_selection(&provider, Some(&previous_provider), allow_image_input);
        context.refresh_usage(self.tools.schemas());
        record.context = context.into_context();
        Ok(())
    }

    async fn handle_turn_message(&mut self, message: TurnActorMessage) -> bool {
        match message {
            TurnActorMessage::Event(event) => self.handle_turn_event(event).await,
            TurnActorMessage::TitleGenerated {
                original_title,
                result,
            } => {
                self.finish_title_generation(&original_title, result).await;
                false
            }
            TurnActorMessage::PersistContext { context, completed } => {
                let previous_usage = self.usage_snapshot();
                self.record.context = *context;
                self.record.touch();
                let result = self.repository.save(&self.record).await;
                if result.is_ok() && self.usage_snapshot() != previous_usage {
                    self.emit_usage_changed();
                }
                let _ = completed.send(result);
                false
            }
            TurnActorMessage::TakePendingMessages { completed } => {
                let batch = self.take_pending_messages().await;
                let _ = completed.send(batch);
                false
            }
        }
    }

    async fn handle_turn_event(&mut self, event: TurnEvent) -> bool {
        match event {
            TurnEvent::AssistantDelta { turn_id, delta } => {
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
                    self.emit(SessionEventPayload::AssistantDelta {
                        message_id,
                        turn_id,
                        step_id,
                        revision,
                        delta,
                    });
                }
            }
            TurnEvent::AssistantReasoningDelta { turn_id, delta } => {
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
                    self.emit(SessionEventPayload::AssistantReasoningDelta {
                        message_id,
                        turn_id,
                        step_id,
                        revision,
                        delta,
                    });
                }
            }
            TurnEvent::AssistantCompleted {
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
                    self.emit_client_event(SessionEventPayload::AssistantCompleted {
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
            TurnEvent::ToolStarted { turn_id, call } => {
                let accepted = if let Some(active) =
                    self.active.as_mut().filter(|active| active.id == turn_id)
                {
                    active.tools.push(call.clone());
                    true
                } else {
                    false
                };
                if accepted {
                    self.emit_client_event(SessionEventPayload::ToolStarted { turn_id, call })
                        .await;
                }
            }
            TurnEvent::ToolCompleted { turn_id, result } => {
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
                    self.emit_client_event(SessionEventPayload::ToolCompleted { turn_id, result })
                        .await;
                }
            }
            TurnEvent::ToolTelemetry { turn_id, event } => match event {
                dwo_tools::ToolEvent::TerminalOpened {
                    tool_call_id,
                    terminal_id,
                    command,
                    cwd,
                } => {
                    self.emit_client_event(SessionEventPayload::TerminalOpened {
                        turn_id,
                        tool_call_id,
                        terminal_id,
                        command,
                        cwd,
                    })
                    .await;
                }
                dwo_tools::ToolEvent::TerminalOutput { terminal_id, data } => {
                    self.emit(SessionEventPayload::TerminalOutput {
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
                    self.emit_client_event(SessionEventPayload::TerminalExited {
                        turn_id,
                        terminal_id,
                        exit_code,
                        status,
                    })
                    .await;
                }
                dwo_tools::ToolEvent::FileRead { tool_call_id, path } => {
                    self.emit_client_event(SessionEventPayload::FileRead {
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
                    self.emit_client_event(SessionEventPayload::FileChanged {
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
            },
            TurnEvent::Finished { turn_id, outcome } => {
                if self
                    .active
                    .as_ref()
                    .is_none_or(|active| active.id != turn_id)
                {
                    return false;
                }
                self.active = None;
                self.permission.reject("turn finished");
                self.phase = RuntimePhase::Idle;
                let allow_pending_wake = !matches!(outcome, TurnOutcome::Cancelled);
                match outcome {
                    TurnOutcome::Completed => {
                        self.emit(SessionEventPayload::TurnCompleted { turn_id })
                    }
                    TurnOutcome::Cancelled => {
                        self.emit(SessionEventPayload::TurnCancelled { turn_id })
                    }
                    TurnOutcome::Failed(error) => {
                        self.emit(SessionEventPayload::TurnFailed { turn_id, error })
                    }
                }
                if let Some(response) = self.closing_response.take() {
                    self.phase = RuntimePhase::Closing;
                    self.reject_pending_messages();
                    self.tools.shutdown().await;
                    let result = self.repository.save(&self.record).await.map_err(Into::into);
                    let _ = response.send(result);
                    return true;
                }
                self.process_pending_idle(allow_pending_wake).await;
            }
        }
        false
    }

    async fn take_pending_messages(&mut self) -> PendingMessageBatch {
        debug_assert!(self.active.is_some());
        let mut messages = Vec::with_capacity(self.pending_messages.len());
        let mut should_continue = false;
        while let Some(pending) = self.pending_messages.pop_front() {
            match pending {
                PendingMessage::User { content } => {
                    messages.push(PendingContextMessage::User(content));
                    should_continue = true;
                }
                PendingMessage::Internal {
                    content,
                    wake,
                    response,
                } => {
                    messages.push(PendingContextMessage::Internal(content));
                    should_continue |= wake;
                    let _ = response.send(Ok(None));
                }
                PendingMessage::StepInternal { content } => {
                    messages.push(PendingContextMessage::Internal(content));
                    should_continue = true;
                }
            }
        }
        PendingMessageBatch {
            messages,
            should_continue,
        }
    }

    async fn process_pending_idle(&mut self, allow_wake: bool) {
        while let Some(pending) = self.pending_messages.pop_front() {
            match pending {
                PendingMessage::User { content } => {
                    if let Err(error) = self.append_user_idle(content).await {
                        tracing::error!(
                            event = "session.pending_user_persist_failed",
                            session_id = %self.record.info.id,
                            error = %format!("{error:#}"),
                            "persist accepted user message after turn finished"
                        );
                    }
                }
                PendingMessage::Internal {
                    content,
                    wake,
                    response,
                } => {
                    if wake && allow_wake {
                        let started = self.start_internal(content).await.map(Some);
                        let running = started.is_ok();
                        let _ = response.send(started);
                        if running {
                            return;
                        }
                    } else {
                        let result = self.append_internal_idle(content).await.map(|()| None);
                        let _ = response.send(result);
                    }
                }
                PendingMessage::StepInternal { .. } => {}
            }
        }
    }

    fn reject_pending_messages(&mut self) {
        while let Some(pending) = self.pending_messages.pop_front() {
            let error = || AgentServiceError::SessionClosed(self.record.info.id.clone());
            match pending {
                PendingMessage::User { .. } => {}
                PendingMessage::Internal { response, .. } => {
                    let _ = response.send(Err(error()));
                }
                PendingMessage::StepInternal { .. } => {}
            }
        }
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
        self.emit(SessionEventPayload::PermissionRequested {
            turn_id,
            permission,
        });
    }

    fn start_title_generation(&mut self, source: String, original_title: String) {
        let model = self.model.clone();
        let selection = ModelSelection {
            model: self.record.llm.model.clone(),
            reasoning: None,
        };
        let actor = self.turn_tx.clone();
        let cancellation = CancellationToken::new();
        self.title_cancellation = Some(cancellation.clone());
        tokio::spawn(async move {
            let source = title_source_excerpt(&source);
            let messages = vec![
                ContextMessage::system(
                    "Generate a concise conversation title. Return only the title, without quotes or explanation. Use the same language as the user. Keep it under 12 Chinese characters or 6 English words.",
                ),
                ContextMessage::user(format!("User message:\n{source}\n\nTitle:")),
            ];
            let result = model.complete(selection, messages, cancellation).await;
            let _ = actor.send(TurnActorMessage::TitleGenerated {
                original_title,
                result,
            });
        });
    }

    async fn finish_title_generation(
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
        self.emit(SessionEventPayload::TitleChanged {
            title,
            updated_at_ms,
        });
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
        self.emit(SessionEventPayload::UsageChanged {
            used: usage.used,
            size: usage.size,
        });
    }

    async fn emit_client_event(&mut self, payload: SessionEventPayload) {
        let event = ClientTranscriptEvent::new(payload.clone());
        match self
            .repository
            .append_transcript_event(&self.record.info.id, &event)
            .await
        {
            Ok(()) => self.transcript.push(event),
            Err(error) => tracing::error!(
                event = "session.transcript_persist_failed",
                session_id = %self.record.info.id,
                error = %format!("{error:#}"),
                "persist client transcript event failed"
            ),
        }
        self.emit(payload);
    }

    fn emit(&mut self, payload: SessionEventPayload) {
        self.seq = self.seq.saturating_add(1);
        let _ = self.events.send(SessionEvent {
            seq: self.seq,
            session_id: self.record.info.id.clone(),
            payload,
        });
    }
}

fn apply_step_delta(step: &mut Option<ActiveStepSnapshot>, payload: &SessionEventPayload) -> bool {
    let (message_id, turn_id, step_id, revision, delta, reasoning) = match payload {
        SessionEventPayload::AssistantDelta {
            message_id,
            turn_id,
            step_id,
            revision,
            delta,
        } => (message_id, turn_id, *step_id, *revision, delta, false),
        SessionEventPayload::AssistantReasoningDelta {
            message_id,
            turn_id,
            step_id,
            revision,
            delta,
        } => (message_id, turn_id, *step_id, *revision, delta, true),
        _ => return false,
    };
    if step
        .as_ref()
        .is_none_or(|current| current.turn_id != *turn_id || current.step_id != step_id)
    {
        *step = Some(ActiveStepSnapshot {
            turn_id: turn_id.clone(),
            step_id,
            revision: 0,
            message_id: MessageId::new(),
            thought_message_id: MessageId::new(),
            reasoning: String::new(),
            response: String::new(),
        });
    }
    let current = step.as_mut().expect("active step was initialized");
    current.revision = revision;
    if reasoning {
        current.thought_message_id = message_id.clone();
        current.reasoning.push_str(delta);
    } else {
        current.message_id = message_id.clone();
        current.response.push_str(delta);
    }
    true
}

enum StepSnapshotSend {
    Sent,
    Full,
    Closed,
}

fn middle_reasoning_mode(modes: &[String]) -> Option<String> {
    let mut modes = modes.to_vec();
    modes.sort_by_key(|mode| reasoning_mode_rank(mode));
    modes.get(modes.len() / 2).cloned()
}

fn reasoning_mode_rank(mode: &str) -> u8 {
    match mode.trim().to_ascii_lowercase().as_str() {
        "none" | "nonthink" => 0,
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

fn try_send_step_snapshot(
    events: &mpsc::Sender<SessionEvent>,
    session_id: &crate::SessionId,
    seq: u64,
    step: Option<&ActiveStepSnapshot>,
) -> StepSnapshotSend {
    let Some(step) = step else {
        return StepSnapshotSend::Sent;
    };
    match events.try_send(SessionEvent {
        seq,
        session_id: session_id.clone(),
        payload: SessionEventPayload::StepSnapshot { step: step.clone() },
    }) {
        Ok(()) => StepSnapshotSend::Sent,
        Err(mpsc::error::TrySendError::Full(_)) => StepSnapshotSend::Full,
        Err(mpsc::error::TrySendError::Closed(_)) => StepSnapshotSend::Closed,
    }
}

fn update_step_checkpoint(step: &mut Option<ActiveStepSnapshot>, payload: &SessionEventPayload) {
    match payload {
        SessionEventPayload::TurnStarted { turn_id } => {
            *step = Some(ActiveStepSnapshot {
                turn_id: turn_id.clone(),
                step_id: 1,
                revision: 0,
                message_id: MessageId::new(),
                thought_message_id: MessageId::new(),
                reasoning: String::new(),
                response: String::new(),
            });
        }
        SessionEventPayload::AssistantCompleted { turn_id, .. }
            if step
                .as_ref()
                .is_some_and(|current| current.turn_id == *turn_id) =>
        {
            let current = step.as_mut().expect("matching active step exists");
            current.step_id = current.step_id.saturating_add(1);
            current.revision = 0;
            current.message_id = MessageId::new();
            current.thought_message_id = MessageId::new();
            current.reasoning.clear();
            current.response.clear();
        }
        SessionEventPayload::TurnCompleted { turn_id }
        | SessionEventPayload::TurnCancelled { turn_id }
        | SessionEventPayload::TurnFailed { turn_id, .. }
            if step
                .as_ref()
                .is_some_and(|current| current.turn_id == *turn_id) =>
        {
            *step = None;
        }
        _ => {}
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
