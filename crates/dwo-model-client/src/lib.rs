//! Provider-configured model transport for streaming turns and compaction summaries.

mod base;
mod client;
mod config;
mod error;
mod message;
mod retry;
mod types;

pub use base::BaseClient;
pub use client::ConfiguredModelClient;
pub use config::{
    AgentModelConfig, AgentModelEntry, AgentProviderConfig, ModelCapabilities, ModelCatalog,
    ModelClientConfig, ModelConfig, ModelSpec, ProviderConfig, ProviderProtocol, ProviderSpec,
    RequestPolicy,
};
pub use error::ModelClientError;
pub use retry::{
    MAX_MODEL_RETRIES, RetryInfo, error_kind, request_with_retry, retry_info, wait_before_retry,
};
pub use types::{
    FinishReason, ModelClient, ModelLimits, ModelReply, ModelSelection, ModelStreamEvent,
    ModelUsage, StreamToolCall, SummaryReply,
};
