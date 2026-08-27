use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::{broadcast, mpsc};

use crate::{EndpointId, MessageContent, MessageId, SessionId, SessionRecord, TurnId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePhase {
    Idle,
    Running,
    Compacting,
    WaitingPermission,
    Cancelling,
    Closing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub raw_input: serde_json::Value,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPermission {
    pub request_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsageSnapshot {
    pub used: u64,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub kind: String,
    pub moved_to: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Manual,
    Automatic,
    Recovery,
    Handoff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveStepSnapshot {
    pub turn_id: TurnId,
    pub step_id: u64,
    pub revision: u64,
    pub message_id: MessageId,
    pub thought_message_id: MessageId,
    pub reasoning: String,
    pub response: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    pub seq: u64,
    pub session_id: SessionId,
    pub payload: SessionEventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientTranscriptEvent {
    pub recorded_at_ms: u64,
    pub payload: SessionEventPayload,
}

impl ClientTranscriptEvent {
    pub fn new(payload: SessionEventPayload) -> Self {
        let recorded_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        Self {
            recorded_at_ms,
            payload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEventPayload {
    UserPromptSubmitted {
        message_id: MessageId,
        turn_id: TurnId,
        origin: EndpointId,
        content: MessageContent,
    },
    TurnStarted {
        turn_id: TurnId,
    },
    AssistantDelta {
        message_id: MessageId,
        turn_id: TurnId,
        step_id: u64,
        revision: u64,
        delta: String,
    },
    AssistantReasoningDelta {
        message_id: MessageId,
        turn_id: TurnId,
        step_id: u64,
        revision: u64,
        delta: String,
    },
    StepSnapshot {
        step: ActiveStepSnapshot,
    },
    AssistantCompleted {
        message_id: MessageId,
        thought_message_id: MessageId,
        turn_id: TurnId,
        content: String,
        reasoning: Option<String>,
        tool_calls: Vec<ActiveToolCall>,
    },
    AssistantInterrupted {
        message_id: MessageId,
        thought_message_id: MessageId,
        turn_id: TurnId,
        content: String,
        reasoning: String,
        error_kind: String,
    },
    Notification {
        message_id: MessageId,
        turn_id: Option<TurnId>,
        origin: Option<EndpointId>,
        category: String,
        level: NotificationLevel,
        text: String,
        #[serde(default)]
        data: serde_json::Value,
    },
    ToolStarted {
        turn_id: TurnId,
        call: ActiveToolCall,
    },
    ToolUpdated {
        turn_id: TurnId,
        call: ActiveToolCall,
    },
    ToolCompleted {
        turn_id: TurnId,
        result: dwo_tools::ToolResult,
    },
    TerminalOpened {
        turn_id: TurnId,
        tool_call_id: String,
        terminal_id: String,
        command: String,
        cwd: PathBuf,
    },
    TerminalOutput {
        turn_id: TurnId,
        terminal_id: String,
        data: Vec<u8>,
    },
    TerminalExited {
        turn_id: TurnId,
        terminal_id: String,
        exit_code: Option<i32>,
        status: String,
    },
    FileRead {
        turn_id: TurnId,
        tool_call_id: String,
        path: PathBuf,
    },
    FileChanged {
        turn_id: TurnId,
        tool_call_id: String,
        changes: Vec<FileChange>,
        patch: String,
    },
    PermissionRequested {
        turn_id: TurnId,
        permission: PendingPermission,
    },
    PermissionResolved {
        turn_id: TurnId,
        request_id: String,
        responder: EndpointId,
        allowed: bool,
        reason: Option<String>,
    },
    PlanUpdated {
        turn_id: TurnId,
        plan: crate::ExecutionPlan,
        cleared: bool,
    },
    TurnCompleted {
        turn_id: TurnId,
    },
    TurnCancelled {
        turn_id: TurnId,
    },
    TurnFailed {
        turn_id: TurnId,
        error: String,
    },
    ConfigChanged {
        config: crate::SessionConfig,
    },
    UsageChanged {
        used: u64,
        size: u64,
    },
    TitleChanged {
        title: String,
        updated_at_ms: u64,
    },
    WorkspaceChanged {
        old_cwd: PathBuf,
        cwd: PathBuf,
        old_worktree_id: Option<String>,
        worktree_id: Option<String>,
    },
    Closing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub record: SessionRecord,
    pub transcript: Vec<ClientTranscriptEvent>,
    pub checkpoint_cursor: usize,
    pub usage: SessionUsageSnapshot,
    pub phase: RuntimePhase,
    pub active_turn_id: Option<TurnId>,
    pub active_step: Option<ActiveStepSnapshot>,
    pub partial_message: String,
    pub active_tool_calls: Vec<ActiveToolCall>,
    pub pending_permission: Option<PendingPermission>,
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusSnapshot {
    pub record: SessionRecord,
    pub usage: SessionUsageSnapshot,
    pub phase: RuntimePhase,
    pub active_turn_id: Option<TurnId>,
    pub last_answer: Option<String>,
    pub last_turn_status: Option<TerminalTurnStatus>,
    pub last_turn_finished_at_ms: Option<u64>,
}

pub struct SessionNotification {
    pub origin: Option<EndpointId>,
    pub category: String,
    pub level: NotificationLevel,
    pub text: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalTurnStatus {
    Completed,
    Failed,
    Cancelled,
}

pub struct SessionSubscription {
    pub snapshot: SessionSnapshot,
    pub events: mpsc::Receiver<SessionEvent>,
}

pub(crate) fn subscription(
    snapshot: SessionSnapshot,
    mut source: broadcast::Receiver<SessionEvent>,
    session_id: SessionId,
) -> SessionSubscription {
    let watermark = snapshot.seq;
    let (events, receiver) = mpsc::channel(256);
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
                        session_id = %session_id,
                        skipped,
                        "disconnect lagging session subscription; client must resync"
                    );
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    SessionSubscription {
        snapshot,
        events: receiver,
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

fn try_send_step_snapshot(
    events: &mpsc::Sender<SessionEvent>,
    session_id: &SessionId,
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
        | SessionEventPayload::AssistantInterrupted { turn_id, .. }
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

pub(crate) fn project_tool_call(
    active: &mut Vec<ActiveToolCall>,
    turn_id: &TurnId,
    mut call: ActiveToolCall,
) -> Option<SessionEventPayload> {
    match active
        .iter_mut()
        .find(|current| current.tool_call_id == call.tool_call_id)
    {
        Some(current) => {
            if terminal_tool_status(&current.status) && !terminal_tool_status(&call.status) {
                call.status.clone_from(&current.status);
            }
            if *current == call {
                return None;
            }
            current.clone_from(&call);
            Some(SessionEventPayload::ToolUpdated {
                turn_id: turn_id.clone(),
                call,
            })
        }
        None => {
            active.push(call.clone());
            Some(SessionEventPayload::ToolStarted {
                turn_id: turn_id.clone(),
                call,
            })
        }
    }
}

fn terminal_tool_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled" | "canceled")
}
