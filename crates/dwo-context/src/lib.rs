//! Model context, prompt construction, environment updates, and compaction.

pub mod compaction;
pub mod env_watcher;
mod manager;
mod message;
pub mod prompt;
mod token;

pub use compaction::{
    COMPACT_INSTRUCTION, CompactionPlan, CompactionPlanner, CompactionView,
    DEFAULT_RECENT_CONTEXT_TOKENS, DEFAULT_RECENT_USER_TOKENS,
};
pub use env_watcher::{DynamicEnvironmentSnapshot, EnvChange, EnvWatcherState};
pub use manager::{
    CompactionState, ContextManager, PendingContextMessage, PendingMessageBatch, SessionContext,
    SessionUsage,
};
pub use message::{
    ContentAnnotations, ContentAudienceRole, ContentBlock, ContextMessage,
    EmbeddedResourceContents, MessageContent, MessageKind, MessageRole, ToolResultRecord, TurnId,
};
pub use prompt::{
    AgentProfilePaths, ChannelCapabilitySnapshot, EnvironmentSnapshot, McpSnapshot,
    PromptBuildError, PromptSnapshot, RuleSnapshot, RuleSource, SkillSnapshot, SystemPromptBlock,
    SystemPromptBuilder,
};
pub use token::{
    estimate_content_tokens, estimate_context_tokens, estimate_message_tokens,
    estimate_text_tokens, estimate_tool_tokens,
};
