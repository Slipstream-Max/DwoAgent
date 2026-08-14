//! Standalone multi-session agent daemon core for the dwoagent rewrite.

mod agent_loop;
pub mod atomic_file;
mod error;
mod events;
mod permission;
mod profile;
mod record;
mod repository;
mod service;
mod session;

pub use dwo_context::{
    CompactionPlan, CompactionPlanner, CompactionState, CompactionView, ContentAnnotations,
    ContentAudienceRole, ContentBlock, ContextManager, ContextMessage, EmbeddedResourceContents,
    EnvChange, MessageContent, MessageKind, MessageRole, SessionContext, SessionUsage,
    SystemPromptBlock, SystemPromptBuilder, TurnId,
};
pub use dwo_model_client::{
    AgentModelConfig, AgentModelEntry, AgentProviderConfig, ConfiguredModelClient, FinishReason,
    ModelCapabilities, ModelCatalog, ModelClient, ModelClientConfig, ModelClientError, ModelConfig,
    ModelLimits, ModelReply, ModelSelection, ModelSpec, ModelStreamEvent, ModelUsage,
    ProviderConfig, ProviderProtocol, ProviderSpec, RequestPolicy, StreamToolCall, SummaryReply,
};
pub use dwo_tools::{ConfirmationDecision, ConfirmationRequest, SessionMode};
pub use error::AgentServiceError;
pub use events::{
    ActiveStepSnapshot, ActiveToolCall, ClientTranscriptEvent, CompactionTrigger, FileChange,
    NotificationLevel, PendingPermission, RuntimePhase, SessionEvent, SessionEventPayload,
    SessionSnapshot, SessionStatusSnapshot, SessionSubscription, SessionUsageSnapshot,
};
pub use profile::{AgentProfileConfig, LoadedAgentProfile, LogLevel, LoggingConfig, load_profile};
pub use record::{
    DEFAULT_MAX_MODEL_STEPS, SessionConfig, SessionConfigUpdate, SessionId, SessionInfo,
    SessionLlmSettings, SessionRecord,
};
pub use repository::{FsSessionRepository, MemorySessionRepository, SessionRepository};
pub use service::{AgentService, NewSession};
pub use session::{EndpointId, MessageId, PromptAccepted, SessionAgent};
