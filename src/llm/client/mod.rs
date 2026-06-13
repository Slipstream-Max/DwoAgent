//! LLM client factory and shared exports.

pub mod base;
pub mod deepseek;

use anyhow::{Result, bail};

pub use base::{
    BaseLlmClient, LlmCancelToken, LlmRequestCancelled, LlmRequestOptions, LlmResponse,
    LlmRetryCallback, LlmRetryEvent, LlmRetryKind, LlmRetryPolicy, PassthroughReasoning,
    ReasoningShaper, StreamChunkCallback, TOOL_ARG_PARSE_ERROR_FIELD, Usage,
};
pub use deepseek::new_deepseek_client;

use crate::config::models::{ModelCapabilities, ModelConfig};

/// Construct a [`BaseLlmClient`] for a concrete provider, mirroring
/// Python's `create_model_client` dispatch.
pub fn create_model_client(
    config: ModelConfig,
    capabilities: ModelCapabilities,
    default_reasoning_mode: &str,
) -> Result<BaseLlmClient> {
    let provider = config.provider.trim().to_string();
    match provider.as_str() {
        "deepseek" => new_deepseek_client(config, capabilities, default_reasoning_mode),
        other => bail!("Unsupported model provider: {other}"),
    }
}
