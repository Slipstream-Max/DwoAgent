use std::fmt;
use std::sync::Arc;

use dwo_context::{ContextManager, MessageContent, SystemPromptBuilder};
use dwo_model_client::ModelClient;
use dwo_model_client::ModelSelection;
use dwo_tools::{ConfirmationDecision, ToolManager};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::TurnId;
use crate::agent_loop::{self, RunTurn, TurnActorMessage, TurnEvent, TurnOutcome};
use crate::error::AgentServiceError;
use crate::events::{
    ActiveToolCall, RuntimePhase, SessionEvent, SessionEventPayload, SessionSnapshot,
    SessionSubscription,
};
use crate::permission::{PermissionRequestEnvelope, PermissionRequester, PermissionState};
use crate::record::{SessionConfig, SessionConfigUpdate, SessionRecord};
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

pub struct SessionAgent {
    id: crate::SessionId,
    control: mpsc::Sender<Control>,
}

impl SessionAgent {
    pub(crate) fn spawn(
        record: SessionRecord,
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
            pending_prompt: None,
            closing_response: None,
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

    pub async fn attach(
        &self,
        endpoint: EndpointId,
    ) -> Result<SessionSubscription, AgentServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(Control::Attach { response }).await?;
        let (snapshot, mut source) = wait
            .await
            .map_err(|_| AgentServiceError::SessionClosed(self.id.clone()))?;
        let watermark = snapshot.seq;
        let (events, receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                match source.recv().await {
                    Ok(event) if event.seq > watermark => {
                        if matches!(
                            &event.payload,
                            SessionEventPayload::UserPromptSubmitted { origin, .. }
                                if origin == &endpoint
                        ) {
                            continue;
                        }
                        if events.send(event).is_err() {
                            break;
                        }
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
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
    ) -> Result<TurnId, AgentServiceError> {
        self.prompt_content(origin, MessageContent::text(content))
            .await
    }

    pub async fn prompt_content(
        &self,
        origin: EndpointId,
        content: MessageContent,
    ) -> Result<TurnId, AgentServiceError> {
        let (response, wait) = oneshot::channel();
        self.send(Control::Prompt {
            origin,
            content,
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
        response: oneshot::Sender<(SessionSnapshot, broadcast::Receiver<SessionEvent>)>,
    },
    Prompt {
        origin: EndpointId,
        content: MessageContent,
        response: oneshot::Sender<Result<TurnId, AgentServiceError>>,
    },
    Cancel {
        expected_turn_id: Option<TurnId>,
        response: oneshot::Sender<Result<(), AgentServiceError>>,
    },
    SetConfig {
        update: SessionConfigUpdate,
        response: oneshot::Sender<Result<(), AgentServiceError>>,
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
    cancellation: CancellationToken,
    partial_message: String,
    tools: Vec<ActiveToolCall>,
}

struct SessionActor {
    record: SessionRecord,
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
    pending_prompt: Option<PendingPrompt>,
    closing_response: Option<oneshot::Sender<Result<(), AgentServiceError>>>,
}

struct PendingPrompt {
    origin: EndpointId,
    content: MessageContent,
    response: oneshot::Sender<Result<TurnId, AgentServiceError>>,
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
        self.permission.reject("session closed");
        self.tools.shutdown().await;
    }

    async fn handle_control(&mut self, control: Control) -> bool {
        match control {
            Control::Attach { response } => {
                let receiver = self.events.subscribe();
                let _ = response.send((self.snapshot(), receiver));
            }
            Control::Prompt {
                origin,
                content,
                response,
            } => {
                if self.phase == RuntimePhase::Closing {
                    let _ = response.send(Err(AgentServiceError::SessionClosed(
                        self.record.info.id.clone(),
                    )));
                    return false;
                }
                let Some(active) = &self.active else {
                    let result = self.start_prompt(origin, content).await;
                    let _ = response.send(result);
                    return false;
                };
                if self.pending_prompt.is_some() {
                    let _ = response.send(Err(AgentServiceError::SessionBusy(
                        self.record.info.id.clone(),
                    )));
                    return false;
                }
                let cancellation = active.cancellation.clone();
                self.pending_prompt = Some(PendingPrompt {
                    origin,
                    content,
                    response,
                });
                cancellation.cancel();
                self.phase = RuntimePhase::Cancelling;
                self.permission.reject("turn interrupted by a new prompt");
                let tools = self.tools.clone();
                tokio::spawn(async move { tools.shutdown().await });
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
                let mut updated = self.record.clone();
                let result = updated
                    .apply_config(update)
                    .map_err(AgentServiceError::InvalidConfig);
                let result = match result {
                    Ok(()) => {
                        let selection = ModelSelection {
                            model: updated.llm.model.clone(),
                            reasoning: updated.llm.reasoning.clone(),
                        };
                        match self.model.validate_selection(&selection) {
                            Ok(()) => {
                                updated.touch();
                                self.repository
                                    .save(&updated)
                                    .await
                                    .map_err(AgentServiceError::from)
                            }
                            Err(error) => Err(AgentServiceError::InvalidConfig(error.to_string())),
                        }
                    }
                    Err(error) => Err(error),
                };
                if result.is_ok() {
                    self.record = updated;
                    let config = self.record.config();
                    self.config_tx.send_replace(config.clone());
                    self.emit(SessionEventPayload::ConfigChanged { config });
                }
                let _ = response.send(result);
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
                if let Some(pending) = self.pending_prompt.take() {
                    let _ = pending.response.send(Err(AgentServiceError::SessionClosed(
                        self.record.info.id.clone(),
                    )));
                }
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
    ) -> Result<TurnId, AgentServiceError> {
        if self.phase == RuntimePhase::Closing {
            return Err(AgentServiceError::SessionClosed(
                self.record.info.id.clone(),
            ));
        }
        let turn_id = TurnId::new();
        let event_content = content.clone();
        let mut context = ContextManager::new(self.record.context.clone());
        context.append_user(turn_id.clone(), content);
        self.record.context = context.into_context();
        self.record.touch();
        self.repository.save(&self.record).await?;
        let cancellation = CancellationToken::new();
        self.active = Some(ActiveTurn {
            id: turn_id.clone(),
            cancellation: cancellation.clone(),
            partial_message: String::new(),
            tools: Vec::new(),
        });
        self.phase = RuntimePhase::Running;
        self.emit(SessionEventPayload::UserPromptSubmitted {
            turn_id: turn_id.clone(),
            origin,
            content: event_content,
        });
        self.emit(SessionEventPayload::TurnStarted {
            turn_id: turn_id.clone(),
        });
        let permission = PermissionRequester::new(
            turn_id.clone(),
            cancellation.clone(),
            self.permission_tx.clone(),
        );
        tokio::spawn(agent_loop::run(RunTurn {
            turn_id: turn_id.clone(),
            context: ContextManager::new(self.record.context.clone()),
            prompt_builder: self.prompt_builder.clone(),
            model: self.model.clone(),
            tools: self.tools.clone(),
            config: self.config_tx.subscribe(),
            permission,
            cancellation,
            actor: self.turn_tx.clone(),
        }));
        Ok(turn_id)
    }

    async fn handle_turn_message(&mut self, message: TurnActorMessage) -> bool {
        match message {
            TurnActorMessage::Event(event) => self.handle_turn_event(event).await,
            TurnActorMessage::PersistContext { context, completed } => {
                self.record.context = *context;
                self.record.touch();
                let result = self.repository.save(&self.record).await;
                let _ = completed.send(result);
                false
            }
        }
    }

    async fn handle_turn_event(&mut self, event: TurnEvent) -> bool {
        match event {
            TurnEvent::AssistantDelta { turn_id, delta } => {
                if let Some(active) = self.active.as_mut().filter(|active| active.id == turn_id) {
                    active.partial_message.push_str(&delta);
                    self.emit(SessionEventPayload::AssistantDelta { turn_id, delta });
                }
            }
            TurnEvent::AssistantCompleted { turn_id, content } => {
                if let Some(active) = self.active.as_mut().filter(|active| active.id == turn_id) {
                    active.partial_message.clear();
                    self.emit(SessionEventPayload::AssistantCompleted { turn_id, content });
                }
            }
            TurnEvent::ToolStarted { turn_id, call } => {
                if let Some(active) = self.active.as_mut().filter(|active| active.id == turn_id) {
                    active.tools.push(call.clone());
                    self.emit(SessionEventPayload::ToolStarted { turn_id, call });
                }
            }
            TurnEvent::ToolCompleted { turn_id, result } => {
                if let Some(active) = self.active.as_mut().filter(|active| active.id == turn_id) {
                    active
                        .tools
                        .retain(|call| call.tool_call_id != result.tool_call_id);
                    self.emit(SessionEventPayload::ToolCompleted { turn_id, result });
                }
            }
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
                if let Some(pending) = self.pending_prompt.take() {
                    let result = self.start_prompt(pending.origin, pending.content).await;
                    let _ = pending.response.send(result);
                    return false;
                }
                if let Some(response) = self.closing_response.take() {
                    self.phase = RuntimePhase::Closing;
                    self.tools.shutdown().await;
                    let result = self.repository.save(&self.record).await.map_err(Into::into);
                    let _ = response.send(result);
                    return true;
                }
            }
        }
        false
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

    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            record: self.record.clone(),
            phase: self.phase,
            active_turn_id: self.active.as_ref().map(|active| active.id.clone()),
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

    fn emit(&mut self, payload: SessionEventPayload) {
        self.seq = self.seq.saturating_add(1);
        let _ = self.events.send(SessionEvent {
            seq: self.seq,
            session_id: self.record.info.id.clone(),
            payload,
        });
    }
}
