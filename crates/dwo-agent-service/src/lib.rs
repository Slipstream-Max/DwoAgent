//! Standalone multi-session agent daemon core for the dwoagent rewrite.

mod agent_loop;
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
    SystemPromptBlock, SystemPromptBuilder, TranscriptItem, TurnId,
};
pub use dwo_model_client::{
    AgentModelConfig, AgentModelEntry, AgentProviderConfig, ConfiguredModelClient, FinishReason,
    ModelCapabilities, ModelCatalog, ModelClient, ModelClientConfig, ModelClientError, ModelConfig,
    ModelLimits, ModelReply, ModelSelection, ModelSpec, ModelStreamEvent, ModelUsage,
    ProviderConfig, ProviderProtocol, ProviderSpec, RequestPolicy, SummaryReply,
    TOOL_RESULT_HEADROOM_TOKENS,
};
pub use dwo_tools::{ConfirmationDecision, ConfirmationRequest, SessionMode};
pub use error::AgentServiceError;
pub use events::{
    ActiveToolCall, PendingPermission, RuntimePhase, SessionEvent, SessionEventPayload,
    SessionSnapshot, SessionSubscription,
};
pub use profile::{AgentProfileConfig, LoadedAgentProfile, load_profile};
pub use record::{
    SessionConfig, SessionConfigUpdate, SessionId, SessionInfo, SessionLlmSettings, SessionRecord,
};
pub use repository::{FsSessionRepository, MemorySessionRepository, SessionRepository};
pub use service::{AgentService, NewSession};
pub use session::{EndpointId, SessionAgent};
