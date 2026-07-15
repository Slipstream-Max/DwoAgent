use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::{EndpointId, MessageContent, SessionId, SessionRecord, TurnId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePhase {
    Idle,
    Running,
    WaitingPermission,
    Cancelling,
    Closing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPermission {
    pub request_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    pub seq: u64,
    pub session_id: SessionId,
    pub payload: SessionEventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEventPayload {
    UserPromptSubmitted {
        turn_id: TurnId,
        origin: EndpointId,
        content: MessageContent,
    },
    TurnStarted {
        turn_id: TurnId,
    },
    AssistantDelta {
        turn_id: TurnId,
        delta: String,
    },
    AssistantCompleted {
        turn_id: TurnId,
        content: String,
    },
    ToolStarted {
        turn_id: TurnId,
        call: ActiveToolCall,
    },
    ToolCompleted {
        turn_id: TurnId,
        result: dwo_tools::ToolResult,
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
    Closing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub record: SessionRecord,
    pub phase: RuntimePhase,
    pub active_turn_id: Option<TurnId>,
    pub partial_message: String,
    pub active_tool_calls: Vec<ActiveToolCall>,
    pub pending_permission: Option<PendingPermission>,
    pub seq: u64,
}

pub struct SessionSubscription {
    pub snapshot: SessionSnapshot,
    pub events: mpsc::UnboundedReceiver<SessionEvent>,
}
