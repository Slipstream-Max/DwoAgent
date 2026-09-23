//! Standalone multi-session agent daemon core for the dwoagent rewrite.

pub mod atomic_file;
mod compaction;
mod error;
mod events;
mod permission;
mod profile;
mod repository;
mod session;
mod session_record;
mod session_service;
mod turn;

pub use dwo_context::{
    CompactionState, ContentAnnotations, ContentAudienceRole, ContentBlock, ContextManager,
    ContextMessage, EmbeddedResourceContents, EnvChange, ExternalRuleFile, MessageContent,
    MessageKind, MessageRole, SessionContext, SessionUsage, SystemPromptBlock, SystemPromptBuilder,
    TurnId,
};
pub use dwo_model_client::{
    AgentModelConfig, AgentModelEntry, AgentProviderConfig, ConfiguredModelClient,
    DefaultModelConfig, FinishReason, ModelCapabilities, ModelCatalog, ModelClient,
    ModelClientConfig, ModelClientError, ModelConfig, ModelFamilySpec, ModelLimits, ModelReply,
    ModelSelection, ModelSpec, ModelStreamEvent, ModelUsage, ProviderConfig, RequestPolicy,
    StreamToolCall, SummaryReply,
};
pub use dwo_tools::{ConfirmationDecision, ConfirmationRequest, SessionMode};
pub use error::SessionServiceError;
pub use events::{
    ActiveStepSnapshot, ActiveToolCall, ClientTranscriptEvent, CompactionTrigger, FileChange,
    NotificationLevel, PendingPermission, RuntimePhase, SessionEvent, SessionEventPayload,
    SessionNotification, SessionSnapshot, SessionStatusSnapshot, SessionSubscription,
    SessionUsageSnapshot, TerminalTurnStatus,
};
pub use profile::{
    AgentProfileConfig, LoadedAgentProfile, LogLevel, LoggingConfig, WebsocketConfig, load_profile,
};
pub use repository::{FsSessionRepository, MemorySessionRepository, SessionRepository};
pub use session::{CompactionAccepted, EndpointId, MessageId, PromptAccepted, SessionHandle};
pub use session_record::{
    DEFAULT_MAX_MODEL_STEPS, ExecutionPlan, SessionConfig, SessionConfigUpdate, SessionId,
    SessionInfo, SessionLlmSettings, SessionRecord, SessionUpdate, SessionWorkspace,
};
pub use session_service::{
    NewSession, SessionDeletionHook, SessionListItem, SessionListPage, SessionListQuery,
    SessionService, WorkspaceResolver,
};
