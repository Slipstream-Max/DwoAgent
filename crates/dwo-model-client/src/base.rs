use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Response, StatusCode, Url};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::{ModelConfig, ProviderConfig, ProviderProtocol};
use crate::message::{StreamAccumulator, normalize_response_tool_calls, provider_messages, usage};
use crate::{FinishReason, ModelClientError, ModelReply, ModelStreamEvent, RequestPolicy};

pub struct BaseClient {
    provider: ProviderConfig,
    endpoint: Url,
    http: Client,
    headers: HeaderMap,
}

impl BaseClient {
    pub fn new(provider: ProviderConfig) -> Result<Self, ModelClientError> {
        if provider.protocol != ProviderProtocol::OpenAiChatCompletions {
            return Err(ModelClientError::config("unsupported provider protocol"));
        }
        let endpoint = provider.endpoint()?;
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        for (name, value) in &provider.headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                ModelClientError::config(format!("invalid provider header {name}: {error}"))
            })?;
            let value = HeaderValue::from_str(value).map_err(|error| {
                ModelClientError::config(format!("invalid provider header value: {error}"))
            })?;
            headers.insert(name, value);
        }
        if let Some(api_key) = provider.resolve_api_key()? {
            let value = HeaderValue::from_str(&format!("Bearer {api_key}"))
                .map_err(|error| ModelClientError::config(error.to_string()))?;
            headers.insert(AUTHORIZATION, value);
        }
        let http = Client::builder()
            .timeout(provider.request.request_timeout())
            .build()?;
        Ok(Self {
            provider,
            endpoint,
            http,
            headers,
        })
    }

    pub async fn stream(
        &self,
        model: &ModelConfig,
        messages: &[dwo_context::ContextMessage],
        tools: &[Value],
        reasoning: Option<&str>,
        events: &mpsc::UnboundedSender<ModelStreamEvent>,
        cancellation: &CancellationToken,
    ) -> Result<ModelReply, ModelClientError> {
        if !tools.is_empty() && !model.capabilities.tool_calls {
            return Err(ModelClientError::protocol(format!(
                "model {} does not support tool calls",
                model.model_id
            )));
        }
        let messages = provider_messages(messages, model.capabilities.image_input)?;
        let body = self.request_body(model, messages, tools, reasoning, true)?;
        let response = self.send_with_retries(&body, cancellation).await?;
        self.read_stream(response, events, cancellation).await
    }

    pub async fn complete(
        &self,
        model: &ModelConfig,
        messages: &[dwo_context::ContextMessage],
        reasoning: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<ModelReply, ModelClientError> {
        let messages = provider_messages(messages, model.capabilities.image_input)?;
        let body = self.request_body(model, messages, &[], reasoning, false)?;
        let response = self.send_with_retries(&body, cancellation).await?;
        let payload: Value = tokio::select! {
            _ = cancellation.cancelled() => return Err(ModelClientError::Cancelled),
            payload = response.json() => payload?,
        };
        parse_completion(&payload)
    }

    fn request_body(
        &self,
        model: &ModelConfig,
        messages: Vec<Value>,
        tools: &[Value],
        reasoning: Option<&str>,
        stream: bool,
    ) -> Result<Value, ModelClientError> {
        let mut body = self.provider.body.clone();
        merge_map(&mut body, &model.body);
        if let Some(temperature) = model.temperature {
            body.insert("temperature".to_string(), json!(temperature));
        }
        if let Some(top_p) = model.top_p {
            body.insert("top_p".to_string(), json!(top_p));
        }
        body.insert("max_tokens".to_string(), json!(model.max_output_tokens));
        let mode = reasoning.unwrap_or(&model.default_reasoning_mode);
        if let Some(override_body) = model.reasoning.get(mode) {
            merge_map(&mut body, override_body);
        } else if mode != "auto" {
            return Err(ModelClientError::config(format!(
                "model {} does not configure reasoning mode {mode}",
                model.model_id
            )));
        }
        body.insert("model".to_string(), Value::String(model.model_id.clone()));
        body.insert("messages".to_string(), Value::Array(messages));
        if !tools.is_empty() {
            body.insert("tools".to_string(), Value::Array(tools.to_vec()));
        }
        body.insert("stream".to_string(), Value::Bool(stream));
        Ok(Value::Object(body))
    }

    async fn send_with_retries(
        &self,
        body: &Value,
        cancellation: &CancellationToken,
    ) -> Result<Response, ModelClientError> {
        let policy = self.provider.request;
        for attempt in 0..=policy.max_retries {
            if cancellation.is_cancelled() {
                return Err(ModelClientError::Cancelled);
            }
            let request = self
                .http
                .post(self.endpoint.clone())
                .headers(self.headers.clone())
                .json(body)
                .send();
            let response = tokio::select! {
                _ = cancellation.cancelled() => return Err(ModelClientError::Cancelled),
                response = request => response,
            };
            match response {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    if is_retryable_status(status) && attempt < policy.max_retries {
                        sleep_before_retry(policy, attempt + 1, cancellation).await?;
                        continue;
                    }
                    return Err(classify_http_error(status, &body));
                }
                Err(error) if is_retryable_error(&error) && attempt < policy.max_retries => {
                    sleep_before_retry(policy, attempt + 1, cancellation).await?;
                }
                Err(error) => return Err(ModelClientError::Http(error)),
            }
        }
        unreachable!("retry loop always returns")
    }

    async fn read_stream(
        &self,
        response: Response,
        events: &mpsc::UnboundedSender<ModelStreamEvent>,
        cancellation: &CancellationToken,
    ) -> Result<ModelReply, ModelClientError> {
        let mut stream = response.bytes_stream().eventsource();
        let mut accumulated = StreamAccumulator::default();
        loop {
            let next = tokio::select! {
                _ = cancellation.cancelled() => return Err(ModelClientError::Cancelled),
                next = tokio::time::timeout(self.provider.request.stream_idle_timeout(), stream.next()) => {
                    next.map_err(|_| ModelClientError::StreamIdleTimeout)?
                }
            };
            let Some(event) = next else {
                return Err(ModelClientError::protocol("stream closed before [DONE]"));
            };
            let event = event.map_err(|error| ModelClientError::protocol(error.to_string()))?;
            if event.data == "[DONE]" {
                break;
            }
            let payload: Value = serde_json::from_str(&event.data).map_err(|error| {
                ModelClientError::protocol(format!("invalid SSE JSON: {error}"))
            })?;
            if let Some(value) = payload.get("usage").filter(|value| !value.is_null()) {
                accumulated.usage = Some(usage(Some(value)));
            }
            let Some(choice) = payload
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|choices| choices.first())
            else {
                continue;
            };
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                accumulated.finish_reason = Some(reason.to_string());
            }
            let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
                continue;
            };
            if let Some(text) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                accumulated.content.push_str(text);
                let _ = events.send(ModelStreamEvent::TextDelta(text.to_string()));
            }
            if let Some(reasoning) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                accumulated.reasoning.push_str(reasoning);
                let _ = events.send(ModelStreamEvent::ReasoningDelta(reasoning.to_string()));
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                accumulated.merge_tool_calls(tool_calls);
            }
        }
        let StreamAccumulator {
            content,
            reasoning,
            tool_calls,
            finish_reason,
            usage,
            ..
        } = accumulated;
        let tool_calls = StreamAccumulator {
            tool_calls,
            ..StreamAccumulator::default()
        }
        .normalized_tool_calls();
        let usage = usage.unwrap_or_default();
        Ok(ModelReply {
            content,
            reasoning: (!reasoning.is_empty()).then_some(reasoning),
            finish_reason: FinishReason::from_provider(
                finish_reason.as_deref(),
                !tool_calls.is_empty(),
            ),
            tool_calls,
            usage,
        })
    }
}

fn parse_completion(payload: &Value) -> Result<ModelReply, ModelClientError> {
    let choice = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| ModelClientError::protocol("missing choices[0]"))?;
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| ModelClientError::protocol("missing choices[0].message"))?;
    let content = match message.get("content") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(content)) => content.clone(),
        Some(_) => {
            return Err(ModelClientError::protocol(
                "assistant content must be a string",
            ));
        }
    };
    let reasoning = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let tool_calls = normalize_response_tool_calls(message.get("tool_calls"));
    Ok(ModelReply {
        content,
        reasoning,
        finish_reason: FinishReason::from_provider(
            choice.get("finish_reason").and_then(Value::as_str),
            !tool_calls.is_empty(),
        ),
        tool_calls,
        usage: usage(payload.get("usage")),
    })
}

fn merge_map(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    for (key, value) in source {
        match (target.get_mut(key), value) {
            (Some(Value::Object(target)), Value::Object(source)) => merge_map(target, source),
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 409 | 425 | 429) || status.is_server_error()
}

fn is_retryable_error(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout() || error.is_request()
}

async fn sleep_before_retry(
    policy: RequestPolicy,
    attempt: u32,
    cancellation: &CancellationToken,
) -> Result<(), ModelClientError> {
    let multiplier = 1_u32
        .checked_shl(attempt.saturating_sub(1).min(8))
        .unwrap_or(256);
    let delay = policy
        .retry_base_delay()
        .checked_mul(multiplier)
        .unwrap_or(Duration::from_secs(30))
        .min(Duration::from_secs(30));
    tokio::select! {
        _ = cancellation.cancelled() => Err(ModelClientError::Cancelled),
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}

fn cap_error_body(body: &str) -> String {
    const LIMIT: usize = 8_000;
    if body.len() <= LIMIT {
        return body.to_string();
    }
    let mut end = LIMIT;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &body[..end])
}

fn classify_http_error(status: StatusCode, body: &str) -> ModelClientError {
    let body = cap_error_body(body);
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return ModelClientError::Authentication {
            status: status.as_u16(),
            body,
        };
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return ModelClientError::RateLimited { body };
    }
    if is_context_length_error(&body) {
        return ModelClientError::ContextLengthExceeded {
            status: status.as_u16(),
            body,
        };
    }
    if status.is_client_error() {
        return ModelClientError::InvalidRequest {
            status: status.as_u16(),
            body,
        };
    }
    ModelClientError::ProviderStatus {
        status: status.as_u16(),
        body,
    }
}

fn is_context_length_error(body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    body.contains("context_length_exceeded")
        || body.contains("maximum context length")
        || (body.contains("context length") && body.contains("exceed"))
        || (body.contains("context window") && body.contains("token"))
        || (body.contains("input token")
            && (body.contains("too long") || body.contains("maximum") || body.contains("exceed")))
}

#[cfg(test)]
mod http_error_tests {
    use super::*;

    #[test]
    fn classifies_only_explicit_context_errors_for_compaction_recovery() {
        assert!(matches!(
            classify_http_error(
                StatusCode::BAD_REQUEST,
                r#"{"error":{"code":"context_length_exceeded"}}"#
            ),
            ModelClientError::ContextLengthExceeded { .. }
        ));
        assert!(matches!(
            classify_http_error(StatusCode::BAD_REQUEST, "unsupported thinking parameter"),
            ModelClientError::InvalidRequest { .. }
        ));
        assert!(matches!(
            classify_http_error(StatusCode::UNAUTHORIZED, "bad key"),
            ModelClientError::Authentication { .. }
        ));
    }
}
