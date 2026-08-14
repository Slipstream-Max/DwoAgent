use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::ModelClientError;

pub const MAX_MODEL_RETRIES: u32 = 5;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryInfo {
    pub retry: u32,
    pub max_retries: u32,
    pub delay: Duration,
    pub error_kind: &'static str,
}

pub fn error_kind(error: &ModelClientError) -> &'static str {
    match error {
        ModelClientError::Config(_) => "config",
        ModelClientError::UnknownModel(_) => "unknown_model",
        ModelClientError::MissingApiKey(_) => "missing_api_key",
        ModelClientError::Cancelled => "cancelled",
        ModelClientError::StreamInterrupted { .. } => "stream_interrupted",
        ModelClientError::ContextLengthExceeded { .. } => "context_length_exceeded",
        ModelClientError::Authentication { .. } => "authentication",
        ModelClientError::RateLimited { .. } => "rate_limited",
        ModelClientError::InvalidRequest { .. } => "invalid_request",
        ModelClientError::ProviderStatus { .. } => "provider_status",
        ModelClientError::Http(_) => "http",
        ModelClientError::Protocol(_) => "protocol",
        ModelClientError::InvalidResponse(_) => "invalid_response",
    }
}

pub fn retry_info(error: &ModelClientError, retry: u32) -> Option<RetryInfo> {
    if retry == 0 || retry > MAX_MODEL_RETRIES || !is_retryable(error) {
        return None;
    }
    let base = Duration::from_secs(1_u64 << (retry - 1).min(4));
    let jitter_ceiling_ms = (base.as_millis() as u64) / 4;
    let jitter_ms = random_u64() % jitter_ceiling_ms.saturating_add(1);
    let configured = error.retry_after().unwrap_or_default();
    let delay = base
        .saturating_add(Duration::from_millis(jitter_ms))
        .max(configured)
        .min(MAX_RETRY_DELAY);
    Some(RetryInfo {
        retry,
        max_retries: MAX_MODEL_RETRIES,
        delay,
        error_kind: error_kind(error),
    })
}

pub async fn wait_before_retry(
    info: &RetryInfo,
    cancellation: &CancellationToken,
) -> Result<(), ModelClientError> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(ModelClientError::Cancelled),
        _ = tokio::time::sleep(info.delay) => Ok(()),
    }
}

pub async fn request_with_retry<T, F, Fut>(
    cancellation: &CancellationToken,
    mut request: F,
) -> Result<T, ModelClientError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ModelClientError>>,
{
    let mut retry = 0;
    loop {
        match request().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                retry += 1;
                let Some(info) = retry_info(&error, retry) else {
                    return Err(error);
                };
                wait_before_retry(&info, cancellation).await?;
            }
        }
    }
}

fn is_retryable(error: &ModelClientError) -> bool {
    match error {
        ModelClientError::StreamInterrupted { .. }
        | ModelClientError::RateLimited { .. }
        | ModelClientError::InvalidResponse(_) => true,
        ModelClientError::ProviderStatus { status, .. } => {
            matches!(*status, 408 | 409 | 425 | 429 | 500..=599)
        }
        ModelClientError::Http(error) => {
            error.is_connect() || error.is_timeout() || error.is_request() || error.is_body()
        }
        _ => false,
    }
}

fn random_u64() -> u64 {
    let mut bytes = [0_u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return 0;
    }
    u64::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_only_transient_errors() {
        assert!(retry_info(&ModelClientError::invalid_response("bad stream"), 1).is_some());
        assert!(
            retry_info(
                &ModelClientError::ProviderStatus {
                    status: 503,
                    body: "busy".to_string(),
                    retry_after_ms: None,
                },
                5,
            )
            .is_some()
        );
        assert!(retry_info(&ModelClientError::Cancelled, 1).is_none());
        assert!(retry_info(&ModelClientError::protocol("bad input"), 1).is_none());
        assert!(retry_info(&ModelClientError::invalid_response("bad stream"), 6).is_none());
    }
}
