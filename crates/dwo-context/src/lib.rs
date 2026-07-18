//! Model context, prompt construction, environment updates, and compaction.

pub mod compaction;
pub mod env_watcher;
mod manager;
mod message;
pub mod prompt;

pub use compaction::{
    COMPACT_INSTRUCTION, CompactionPlan, CompactionPlanner, CompactionView, DEFAULT_RECENT_TURNS,
    DEFAULT_RECENT_USER_BYTES,
};
pub use env_watcher::{DynamicEnvironmentSnapshot, EnvChange, EnvWatcherState};
pub use manager::{CompactionState, ContextManager, SessionContext, SessionUsage};
pub use message::{
    ContentAnnotations, ContentAudienceRole, ContentBlock, ContextMessage,
    EmbeddedResourceContents, MessageContent, MessageKind, MessageRole, ToolResultRecord, TurnId,
};
pub use prompt::{
    AgentProfilePaths, ChannelCapabilitySnapshot, EnvironmentSnapshot, McpSnapshot,
    PromptBuildError, PromptSnapshot, RuleSnapshot, SkillSnapshot, SystemPromptBlock,
    SystemPromptBuilder,
};
