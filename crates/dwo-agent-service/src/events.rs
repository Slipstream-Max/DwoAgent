use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::mpsc;

use crate::{EndpointId, MessageContent, MessageId, SessionId, SessionRecord, TurnId};

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
    pub raw_input: serde_json::Value,
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
    ToolStarted {
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
    CompactionStarted {
        turn_id: TurnId,
        compaction_id: String,
        trigger: CompactionTrigger,
    },
    CompactionCompleted {
        turn_id: TurnId,
        compaction_id: String,
        summary: Option<String>,
    },
    CompactionFailed {
        turn_id: TurnId,
        compaction_id: String,
        error: String,
    },
    CompactionCancelled {
        turn_id: TurnId,
        compaction_id: String,
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
}

pub struct SessionSubscription {
    pub snapshot: SessionSnapshot,
    pub events: mpsc::Receiver<SessionEvent>,
}
