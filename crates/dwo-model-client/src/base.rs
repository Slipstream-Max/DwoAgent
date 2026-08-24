use std::collections::HashSet;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::header::{
    AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER,
};
use reqwest::{Client, Response, StatusCode, Url};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::{ModelConfig, ProviderConfig};
use crate::message::{
    StreamAccumulator, parse_response, provider_input, provider_tool_event_state, provider_tools,
    stream_tool_call,
};
use crate::{ModelClientError, ModelReply, ModelStreamEvent};

pub struct BaseClient {
    provider: ProviderConfig,
    endpoint: Url,
    http: Client,
    headers: HeaderMap,
}

impl BaseClient {
    pub fn new(provider: ProviderConfig) -> Result<Self, ModelClientError> {
        let endpoint = provider.responses_endpoint()?;
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
        let input = provider_input(messages, model.capabilities.image_input)?;
        let tools = provider_tools(tools, &model.hosted_tools)?;
        let body = self.request_body(model, input, &tools, reasoning, true)?;
        let response = self.send(&body, cancellation).await?;
        self.read_stream(response, &model.hosted_tools, events, cancellation)
            .await
    }

    pub async fn complete(
        &self,
        model: &ModelConfig,
        messages: &[dwo_context::ContextMessage],
        reasoning: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<ModelReply, ModelClientError> {
        let input = provider_input(messages, model.capabilities.image_input)?;
        let body = self.request_body(model, input, &[], reasoning, false)?;
        let response = self.send(&body, cancellation).await?;
        let payload: Value = tokio::select! {
            _ = cancellation.cancelled() => return Err(ModelClientError::Cancelled),
            payload = response.json() => payload?,
        };
        parse_response(&payload)
    }

    fn request_body(
        &self,
        model: &ModelConfig,
        input: Vec<Value>,
        tools: &[Value],
        reasoning: Option<&str>,
        stream: bool,
    ) -> Result<Value, ModelClientError> {
        let mut body = self.provider.extra_body.clone();
        merge_map(&mut body, &model.extra_body);
        if let Some(temperature) = model.temperature {
            body.insert("temperature".to_string(), json!(temperature));
        }
        if let Some(top_p) = model.top_p {
            body.insert("top_p".to_string(), json!(top_p));
        }
        body.insert(
            "max_output_tokens".to_string(),
            json!(model.max_output_tokens),
        );
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
        body.insert("input".to_string(), Value::Array(input));
        if !tools.is_empty() {
            body.insert("tools".to_string(), Value::Array(tools.to_vec()));
        }
        body.insert("stream".to_string(), Value::Bool(stream));
        Ok(Value::Object(body))
    }

    async fn send(
        &self,
        body: &Value,
        cancellation: &CancellationToken,
    ) -> Result<Response, ModelClientError> {
        let request = self
            .http
            .post(self.endpoint.clone())
            .headers(self.headers.clone())
            .json(body)
            .send();
        let response = tokio::select! {
            _ = cancellation.cancelled() => return Err(ModelClientError::Cancelled),
            response = request => response?,
        };
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let retry_after_ms = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(|seconds| seconds.saturating_mul(1_000));
        let body = response.text().await.unwrap_or_default();
        Err(classify_http_error(status, &body, retry_after_ms))
    }

    async fn read_stream(
        &self,
        response: Response,
        hosted_tools: &[Value],
        events: &mpsc::UnboundedSender<ModelStreamEvent>,
        cancellation: &CancellationToken,
    ) -> Result<ModelReply, ModelClientError> {
        let mut stream = response.bytes_stream().eventsource();
        let mut accumulated = StreamAccumulator::default();
        let mut emitted_tool_calls = HashSet::new();
        let mut reasoning_summary_part = None;
        let mut reasoning_content_part = None;
        loop {
            let next = tokio::select! {
                _ = cancellation.cancelled() => return Err(ModelClientError::Cancelled),
                next = tokio::time::timeout(self.provider.request.stream_idle_timeout(), stream.next()) => {
                    next.map_err(|_| ModelClientError::StreamInterrupted {
                        text_chars: accumulated.content.chars().count(),
                        has_tool_calls: !accumulated.output_items.is_empty(),
                    })?
                }
            };
            let Some(event) = next else {
                if accumulated.response.is_some() {
                    break;
                }
                return Err(ModelClientError::StreamInterrupted {
                    text_chars: accumulated.content.chars().count(),
                    has_tool_calls: !accumulated.output_items.is_empty(),
                });
            };
            let event = event.map_err(|_| ModelClientError::StreamInterrupted {
                text_chars: accumulated.content.chars().count(),
                has_tool_calls: !accumulated.output_items.is_empty(),
            })?;
            if event.data == "[DONE]" {
                break;
            }
            let payload: Value = serde_json::from_str(&event.data).map_err(|error| {
                ModelClientError::invalid_response(format!("invalid SSE JSON: {error}"))
            })?;
            match payload.get("type").and_then(Value::as_str) {
                Some("response.output_text.delta") => {
                    if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                        accumulated.content.push_str(delta);
                        let _ = events.send(ModelStreamEvent::TextDelta(delta.to_string()));
                    }
                }
                Some(
                    event_type @ ("response.reasoning_summary_text.delta"
                    | "response.reasoning_text.delta"),
                ) => {
                    if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                        let part = reasoning_part_key(&payload, event_type);
                        let (reasoning, reasoning_part) =
                            if event_type == "response.reasoning_summary_text.delta" {
                                (
                                    &mut accumulated.reasoning_summary,
                                    &mut reasoning_summary_part,
                                )
                            } else {
                                (
                                    &mut accumulated.reasoning_content,
                                    &mut reasoning_content_part,
                                )
                            };
                        let starts_new_part =
                            reasoning_part.is_some() && part.is_some() && *reasoning_part != part;
                        if starts_new_part {
                            reasoning.push_str("\n\n");
                            let _ =
                                events.send(ModelStreamEvent::ReasoningDelta("\n\n".to_string()));
                        }
                        if part.is_some() {
                            *reasoning_part = part;
                        }
                        reasoning.push_str(delta);
                        let _ = events.send(ModelStreamEvent::ReasoningDelta(delta.to_string()));
                    }
                }
                Some(event_type @ ("response.output_item.added" | "response.output_item.done")) => {
                    if let (Some(index), Some(item)) = (
                        payload.get("output_index").and_then(Value::as_u64),
                        payload.get("item"),
                    ) {
                        accumulated.add_output_item(index, item.clone());
                        if event_type == "response.output_item.done"
                            && !emitted_tool_calls.contains(&index)
                            && let Some(call) = stream_tool_call(item, hosted_tools)
                        {
                            emitted_tool_calls.insert(index);
                            let _ = events.send(ModelStreamEvent::ToolCall(call));
                        }
                    }
                }
                Some("response.function_call_arguments.delta") => {
                    if let (Some(index), Some(delta)) = (
                        payload.get("output_index").and_then(Value::as_u64),
                        payload.get("delta").and_then(Value::as_str),
                    ) {
                        let _ = accumulated.update_function_arguments(index, delta, true);
                    }
                }
                Some("response.function_call_arguments.done") => {
                    if let (Some(index), Some(arguments)) = (
                        payload.get("output_index").and_then(Value::as_u64),
                        payload.get("arguments").and_then(Value::as_str),
                    ) {
                        let call = accumulated
                            .update_function_arguments(index, arguments, false)
                            .and_then(|item| stream_tool_call(item, hosted_tools));
                        if !emitted_tool_calls.contains(&index)
                            && let Some(call) = call
                        {
                            emitted_tool_calls.insert(index);
                            let _ = events.send(ModelStreamEvent::ToolCall(call));
                        }
                    }
                }
                Some("response.completed" | "response.incomplete") => {
                    accumulated.response = payload.get("response").cloned();
                    break;
                }
                Some("response.failed") => {
                    return Err(ModelClientError::invalid_response(format!(
                        "response failed: {}",
                        payload.get("response").unwrap_or(&payload)
                    )));
                }
                Some(event_type) => {
                    let Some(status) = provider_tool_event_state(event_type) else {
                        continue;
                    };
                    let _ = accumulated.set_output_item_status(
                        payload.get("output_index").and_then(Value::as_u64),
                        payload.get("item_id").and_then(Value::as_str),
                        status,
                    );
                }
                None => {}
            }
        }
        accumulated.finish()
    }
}

fn reasoning_part_key(payload: &Value, event_type: &str) -> Option<(String, u64, &'static str)> {
    let item_id = payload
        .get("item_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            payload
                .get("output_index")
                .and_then(Value::as_u64)
                .map(|index| index.to_string())
        })?;
    if event_type == "response.reasoning_summary_text.delta" {
        payload
            .get("summary_index")
            .and_then(Value::as_u64)
            .map(|index| (item_id, index, "summary"))
    } else {
        payload
            .get("content_index")
            .and_then(Value::as_u64)
            .map(|index| (item_id, index, "content"))
    }
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

fn classify_http_error(
    status: StatusCode,
    body: &str,
    retry_after_ms: Option<u64>,
) -> ModelClientError {
    let body = cap_error_body(body);
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return ModelClientError::Authentication {
            status: status.as_u16(),
            body,
        };
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return ModelClientError::RateLimited {
            body,
            retry_after_ms,
        };
    }
    if is_context_length_error(&body) {
        return ModelClientError::ContextLengthExceeded {
            status: status.as_u16(),
            body,
        };
    }
    if status.is_client_error() && !matches!(status.as_u16(), 408 | 409 | 425) {
        return ModelClientError::InvalidRequest {
            status: status.as_u16(),
            body,
        };
    }
    ModelClientError::ProviderStatus {
        status: status.as_u16(),
        body,
        retry_after_ms,
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
                r#"{"error":{"code":"context_length_exceeded"}}"#,
                None,
            ),
            ModelClientError::ContextLengthExceeded { .. }
        ));
        assert!(matches!(
            classify_http_error(
                StatusCode::BAD_REQUEST,
                "unsupported thinking parameter",
                None,
            ),
            ModelClientError::InvalidRequest { .. }
        ));
        assert!(matches!(
            classify_http_error(StatusCode::UNAUTHORIZED, "bad key", None),
            ModelClientError::Authentication { .. }
        ));
    }
}
