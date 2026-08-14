use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ModelClientError {
    #[error("invalid model client configuration: {0}")]
    Config(String),
    #[error("unknown model alias: {0}")]
    UnknownModel(String),
    #[error("missing API key from environment variable: {0}")]
    MissingApiKey(String),
    #[error("model request was cancelled")]
    Cancelled,
    #[error(
        "model stream interrupted after {text_chars} chars of output (tool calls in flight: {has_tool_calls})"
    )]
    StreamInterrupted {
        text_chars: usize,
        has_tool_calls: bool,
    },
    #[error("model input exceeds the context window (HTTP {status}): {body}")]
    ContextLengthExceeded { status: u16, body: String },
    #[error("model provider authentication failed (HTTP {status}): {body}")]
    Authentication { status: u16, body: String },
    #[error("model provider rate limit exceeded: {body}")]
    RateLimited {
        body: String,
        retry_after_ms: Option<u64>,
    },
    #[error("model provider rejected the request (HTTP {status}): {body}")]
    InvalidRequest { status: u16, body: String },
    #[error("model provider returned HTTP {status}: {body}")]
    ProviderStatus {
        status: u16,
        body: String,
        retry_after_ms: Option<u64>,
    },
    #[error("model HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid model provider response: {0}")]
    Protocol(String),
    #[error("transient invalid model provider response: {0}")]
    InvalidResponse(String),
}

impl ModelClientError {
    pub(crate) fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }

    pub(crate) fn invalid_response(message: impl Into<String>) -> Self {
        Self::InvalidResponse(message.into())
    }

    pub fn is_context_length_exceeded(&self) -> bool {
        matches!(self, Self::ContextLengthExceeded { .. })
    }

    pub fn is_stream_interrupted(&self) -> bool {
        matches!(self, Self::StreamInterrupted { .. })
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after_ms, .. }
            | Self::ProviderStatus { retry_after_ms, .. } => {
                retry_after_ms.map(Duration::from_millis)
            }
            _ => None,
        }
    }
}
