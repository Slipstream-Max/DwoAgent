//! Base OpenAI-compatible LLM client.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use eventsource_stream::{Event, EventStreamError, Eventsource};
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Map, Value, json};
use tokio::time::{sleep, timeout};

use crate::perf::{messages_size, perf_log};
use crate::{ModelCapabilities, ModelConfig};

pub const TOOL_ARG_PARSE_ERROR_FIELD: &str = "_dwo_tool_arg_parse_error";

/// Async callback invoked for each streaming chunk.
pub type StreamChunkCallback =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>;

pub type LlmRetryCallback =
    Arc<dyn Fn(LlmRetryEvent) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>;

const DEFAULT_REQUEST_MAX_RETRIES: u32 = 4;
const DEFAULT_STREAM_MAX_RETRIES: u32 = 5;
const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_RETRY_BASE_DELAY: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy)]
pub struct LlmRetryPolicy {
    pub request_max_retries: u32,
    pub stream_max_retries: u32,
    pub stream_idle_timeout: Duration,
    pub base_delay: Duration,
}

impl Default for LlmRetryPolicy {
    fn default() -> Self {
        Self {
            request_max_retries: DEFAULT_REQUEST_MAX_RETRIES,
            stream_max_retries: DEFAULT_STREAM_MAX_RETRIES,
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
            base_delay: DEFAULT_RETRY_BASE_DELAY,
        }
    }
}

#[derive(Clone)]
pub struct LlmCancelToken {
    is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
    wait_cancelled: Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>,
}

impl LlmCancelToken {
    pub fn new<IsCancelled, WaitCancelled, WaitFuture>(
        is_cancelled: IsCancelled,
        wait_cancelled: WaitCancelled,
    ) -> Self
    where
        IsCancelled: Fn() -> bool + Send + Sync + 'static,
        WaitCancelled: Fn() -> WaitFuture + Send + Sync + 'static,
        WaitFuture: Future<Output = ()> + Send + 'static,
    {
        Self {
            is_cancelled: Arc::new(is_cancelled),
            wait_cancelled: Arc::new(move || Box::pin(wait_cancelled())),
        }
    }

    fn is_cancelled(&self) -> bool {
        (self.is_cancelled)()
    }

    async fn wait_cancelled(&self) {
        (self.wait_cancelled)().await;
    }
}

#[derive(Clone)]
pub struct LlmRequestOptions {
    pub retry: LlmRetryPolicy,
    pub cancel: Option<LlmCancelToken>,
    pub on_retry: Option<LlmRetryCallback>,
}

impl Default for LlmRequestOptions {
    fn default() -> Self {
        Self {
            retry: LlmRetryPolicy::default(),
            cancel: None,
            on_retry: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum LlmRetryKind {
    Request,
    Stream,
}

#[derive(Debug, Clone)]
pub struct LlmRetryEvent {
    pub kind: LlmRetryKind,
    pub attempt: u32,
    pub max_retries: u32,
    pub delay: Duration,
    pub error: String,
}

#[derive(Debug)]
pub struct LlmRequestCancelled;

impl std::fmt::Display for LlmRequestCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LLM request was cancelled.")
    }
}

impl std::error::Error for LlmRequestCancelled {}

/// Normalized usage block returned to callers.
#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

impl Usage {
    fn from_value(value: &Value) -> Result<Self> {
        let obj = value
            .as_object()
            .ok_or_else(|| anyhow!("Missing usage in model response"))?;
        let prompt_tokens = obj
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("usage.prompt_tokens missing"))?;
        let completion_tokens = obj
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("usage.completion_tokens missing"))?;
        let total_tokens = obj
            .get("total_tokens")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("usage.total_tokens missing"))?;
        Ok(Self {
            prompt_tokens,
            completion_tokens,
            total_tokens,
        })
    }
}

/// Response envelope matching Python's `{message, usage, total_tokens}`.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub message: Map<String, Value>,
    pub usage: Usage,
    pub total_tokens: u64,
    pub finish_reason: Option<String>,
}

/// OpenAI-compatible base client. Equivalent to Python's `BaseLLMClient`.
///
/// Concrete providers (e.g. DeepSeek) extend via the `ReasoningShaper` trait
/// to supply provider-specific request tweaks.
pub struct BaseLlmClient {
    pub config: ModelConfig,
    pub capabilities: ModelCapabilities,
    pub default_reasoning_mode: String,
    http: reqwest::Client,
    api_base: String,
    api_key: Option<String>,
    reasoning_shaper: Box<dyn ReasoningShaper>,
}

/// Strategy hook used by provider clients to shape provider-specific
/// reasoning-mode request kwargs. Mirror of Python's `_reasoning_kwargs`.
pub trait ReasoningShaper: Send + Sync {
    fn reasoning_kwargs(&self, reasoning_mode: &str) -> Result<Map<String, Value>>;
}

/// Default no-op shaper (matches the `BaseLLMClient` base behaviour).
pub struct PassthroughReasoning;

impl ReasoningShaper for PassthroughReasoning {
    fn reasoning_kwargs(&self, _reasoning_mode: &str) -> Result<Map<String, Value>> {
        Ok(Map::new())
    }
}

struct StreamAttemptResponse {
    message: Map<String, Value>,
    usage: Usage,
    content_chars: usize,
    reasoning_chars: usize,
    finish_reason: Option<String>,
}

enum StreamAttemptError {
    Retryable(anyhow::Error),
    Fatal(anyhow::Error),
}

impl BaseLlmClient {
    pub fn new(
        config: ModelConfig,
        capabilities: ModelCapabilities,
        default_reasoning_mode: impl Into<String>,
        reasoning_shaper: Box<dyn ReasoningShaper>,
    ) -> Result<Self> {
        let mut api_key = config.api_key.clone();
        if api_key.is_none() {
            if let Some(env_name) = config.api_key_env.as_deref() {
                api_key = std::env::var(env_name).ok().filter(|v| !v.is_empty());
                if api_key.is_none() {
                    bail!("Missing API key: {env_name}");
                }
            }
        }

        let mut builder = reqwest::Client::builder();
        if let Some(timeout) = config.timeout_seconds {
            if timeout.is_finite() && timeout > 0.0 {
                builder = builder.timeout(Duration::from_secs_f64(timeout));
            }
        }
        let http = builder.build().context("build reqwest client")?;

        let api_base = config
            .api_base
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

        Ok(Self {
            config,
            capabilities,
            default_reasoning_mode: default_reasoning_mode.into(),
            http,
            api_base,
            api_key,
            reasoning_shaper,
        })
    }

    /// Non-streaming request returning `{message, usage, total_tokens}`.
    pub async fn request_with_usage(
        &self,
        messages: &[Value],
        model_name: Option<&str>,
        tools: Option<&[Value]>,
        reasoning_mode: Option<&str>,
        options: LlmRequestOptions,
    ) -> Result<LlmResponse> {
        let started = Instant::now();
        let resolved_tools = self.tools_for_request(tools);
        let effective_model = model_name.unwrap_or(&self.config.model_id);
        let effective_reasoning = reasoning_mode.unwrap_or(&self.default_reasoning_mode);

        perf_log(
            "llm_request_start",
            &json!({
                "stream": false,
                "model": effective_model,
                "messages": messages.len(),
                "message_chars": messages_size(messages),
                "tools": resolved_tools.as_ref().map(Vec::len).unwrap_or(0),
                "reasoning_mode": effective_reasoning,
            }),
        );

        let body = self.build_request_body(
            messages,
            model_name,
            resolved_tools.as_deref(),
            false,
            reasoning_mode,
        )?;
        let response: Value = self
            .send_chat_completion_with_retries(&body, false, &options)
            .await?
            .json()
            .await
            .context("llm response decode")?;

        let choices = response
            .get("choices")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("Missing choices in non-stream response"))?;
        let first = choices
            .first()
            .ok_or_else(|| anyhow!("Empty choices in non-stream response"))?;
        let raw_message = first
            .get("message")
            .cloned()
            .ok_or_else(|| anyhow!("Missing message in non-stream response"))?;
        let finish_reason = first
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::to_string);
        let message_map = match raw_message {
            Value::Object(map) => prune_nulls(map),
            _ => bail!("choices[0].message must be object"),
        };

        let usage = Usage::from_value(
            response
                .get("usage")
                .ok_or_else(|| anyhow!("Missing usage in model response"))?,
        )?;
        let total_tokens = usage.total_tokens;

        perf_log(
            "llm_request_done",
            &json!({
                "stream": false,
                "model": effective_model,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "total_tokens": total_tokens,
                "finish_reason": finish_reason.clone(),
            }),
        );

        Ok(LlmResponse {
            message: message_map,
            usage,
            total_tokens,
            finish_reason,
        })
    }

    /// Streaming request with optional text / reasoning chunk callbacks.
    pub async fn request_stream_with_usage(
        &self,
        messages: &[Value],
        model_name: Option<&str>,
        tools: Option<&[Value]>,
        on_text_chunk: Option<StreamChunkCallback>,
        on_reasoning_chunk: Option<StreamChunkCallback>,
        reasoning_mode: Option<&str>,
        options: LlmRequestOptions,
    ) -> Result<LlmResponse> {
        let started = Instant::now();
        let mut first_chunk_at: Option<Instant> = None;
        let resolved_tools = self.tools_for_request(tools);
        let effective_model = model_name.unwrap_or(&self.config.model_id);
        let effective_reasoning = reasoning_mode.unwrap_or(&self.default_reasoning_mode);

        perf_log(
            "llm_request_start",
            &json!({
                "stream": true,
                "model": effective_model,
                "messages": messages.len(),
                "message_chars": messages_size(messages),
                "tools": resolved_tools.as_ref().map(Vec::len).unwrap_or(0),
                "reasoning_mode": effective_reasoning,
            }),
        );

        let body = self.build_request_body(
            messages,
            model_name,
            resolved_tools.as_deref(),
            true,
            reasoning_mode,
        )?;
        let mut stream_retry_count = 0_u32;
        let response = loop {
            raise_if_llm_cancelled(options.cancel.as_ref())?;
            match self
                .request_stream_once(
                    &body,
                    on_text_chunk.as_ref(),
                    on_reasoning_chunk.as_ref(),
                    &options,
                    &mut first_chunk_at,
                )
                .await
            {
                Ok(response) => break response,
                Err(StreamAttemptError::Fatal(err)) => return Err(err),
                Err(StreamAttemptError::Retryable(err))
                    if stream_retry_count < options.retry.stream_max_retries =>
                {
                    stream_retry_count += 1;
                    let delay = retry_delay(options.retry.base_delay, stream_retry_count);
                    perf_log(
                        "llm_stream_retry",
                        &json!({
                            "stream": true,
                            "attempt": stream_retry_count,
                            "max_retries": options.retry.stream_max_retries,
                            "delay_ms": delay.as_millis() as u64,
                            "error": err.to_string(),
                        }),
                    );
                    report_retry(
                        options.on_retry.as_ref(),
                        LlmRetryEvent {
                            kind: LlmRetryKind::Stream,
                            attempt: stream_retry_count,
                            max_retries: options.retry.stream_max_retries,
                            delay,
                            error: err.to_string(),
                        },
                    )
                    .await;
                    sleep_with_cancel(delay, options.cancel.as_ref()).await?;
                }
                Err(StreamAttemptError::Retryable(err)) => {
                    return Err(err).context("llm stream response");
                }
            }
        };

        let content_chars = response.content_chars;
        let reasoning_chars = response.reasoning_chars;
        let tool_count = response
            .message
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);

        perf_log(
            "llm_request_done",
            &json!({
                "stream": true,
                "model": effective_model,
                "first_chunk_ms": first_chunk_at
                    .map(|t| t.duration_since(started).as_millis() as u64),
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "total_tokens": response.usage.total_tokens,
                "content_chars": content_chars,
                "reasoning_chars": reasoning_chars,
                "tool_calls": tool_count,
                "stream_retries": stream_retry_count,
                "finish_reason": response.finish_reason.clone(),
            }),
        );

        Ok(LlmResponse {
            message: response.message,
            usage: response.usage,
            total_tokens: response.usage.total_tokens,
            finish_reason: response.finish_reason,
        })
    }

    async fn request_stream_once(
        &self,
        body: &Value,
        on_text_chunk: Option<&StreamChunkCallback>,
        on_reasoning_chunk: Option<&StreamChunkCallback>,
        options: &LlmRequestOptions,
        first_chunk_at: &mut Option<Instant>,
    ) -> std::result::Result<StreamAttemptResponse, StreamAttemptError> {
        let http_response = self
            .send_chat_completion_with_retries(body, true, options)
            .await
            .map_err(StreamAttemptError::Fatal)?;

        let mut event_stream = http_response.bytes_stream().eventsource();

        let mut role = "assistant".to_string();
        let mut content_parts: Vec<String> = Vec::new();
        let mut reasoning_parts: Vec<String> = Vec::new();
        let mut tool_call_map: BTreeMap<u64, Map<String, Value>> = BTreeMap::new();
        let mut usage: Option<Usage> = None;
        let mut finish_reason: Option<String> = None;

        loop {
            raise_if_llm_cancelled(options.cancel.as_ref()).map_err(StreamAttemptError::Fatal)?;
            let next_event = next_stream_event(
                &mut event_stream,
                options.retry.stream_idle_timeout,
                options.cancel.as_ref(),
            )
            .await?;
            let Some(event) = next_event else {
                return Err(StreamAttemptError::Retryable(anyhow!(
                    "llm stream closed before [DONE]"
                )));
            };
            if first_chunk_at.is_none() {
                *first_chunk_at = Some(Instant::now());
            }
            let data = event.data;
            if data == "[DONE]" {
                break;
            }
            let chunk: Value = serde_json::from_str(&data).map_err(|err| {
                StreamAttemptError::Retryable(anyhow!(err).context("llm stream chunk parse"))
            })?;

            if let Some(u) = chunk.get("usage").and_then(|v| (!v.is_null()).then_some(v)) {
                usage = Some(Usage::from_value(u).map_err(StreamAttemptError::Retryable)?);
            }

            let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
                continue;
            };
            let Some(first) = choices.first() else {
                continue;
            };
            if let Some(reason) = first.get("finish_reason").and_then(Value::as_str)
                && !reason.is_empty()
            {
                finish_reason = Some(reason.to_string());
            }
            let Some(delta) = first.get("delta") else {
                continue;
            };

            if let Some(r) = delta.get("role").and_then(Value::as_str) {
                role = r.to_string();
            }
            if let Some(text) = delta.get("content").and_then(Value::as_str)
                && !text.is_empty()
            {
                content_parts.push(text.to_string());
                if let Some(cb) = on_text_chunk {
                    cb(text.to_string()).await.map_err(|err| {
                        if options
                            .cancel
                            .as_ref()
                            .is_some_and(LlmCancelToken::is_cancelled)
                        {
                            StreamAttemptError::Fatal(anyhow::Error::new(LlmRequestCancelled))
                        } else {
                            StreamAttemptError::Fatal(err.context("llm stream text chunk callback"))
                        }
                    })?;
                }
            }
            if let Some(reasoning_text) = delta.get("reasoning_content").and_then(Value::as_str)
                && !reasoning_text.is_empty()
            {
                reasoning_parts.push(reasoning_text.to_string());
                if let Some(cb) = on_reasoning_chunk {
                    cb(reasoning_text.to_string()).await.map_err(|err| {
                        if options
                            .cancel
                            .as_ref()
                            .is_some_and(LlmCancelToken::is_cancelled)
                        {
                            StreamAttemptError::Fatal(anyhow::Error::new(LlmRequestCancelled))
                        } else {
                            StreamAttemptError::Fatal(
                                err.context("llm stream reasoning chunk callback"),
                            )
                        }
                    })?;
                }
            }
            if let Some(tc_deltas) = delta.get("tool_calls").and_then(Value::as_array) {
                merge_tool_call_deltas(&mut tool_call_map, tc_deltas);
            }
        }

        let usage = usage.ok_or_else(|| {
            StreamAttemptError::Retryable(anyhow!("Missing usage in stream response"))
        })?;

        let mut assistant_message = Map::new();
        assistant_message.insert("role".to_string(), Value::String(role));
        assistant_message.insert(
            "content".to_string(),
            if content_parts.is_empty() {
                Value::Null
            } else {
                Value::String(content_parts.concat())
            },
        );
        if !reasoning_parts.is_empty() {
            assistant_message.insert(
                "reasoning_content".to_string(),
                Value::String(reasoning_parts.concat()),
            );
        }
        if !tool_call_map.is_empty() {
            let finalized: Vec<Value> = tool_call_map.into_values().map(Value::Object).collect();
            assistant_message.insert("tool_calls".to_string(), Value::Array(finalized));
        }

        let content_chars: usize = content_parts.iter().map(|s| s.chars().count()).sum();
        let reasoning_chars: usize = reasoning_parts.iter().map(|s| s.chars().count()).sum();
        Ok(StreamAttemptResponse {
            message: assistant_message,
            usage,
            content_chars,
            reasoning_chars,
            finish_reason,
        })
    }

    /// Parse assistant tool_calls into `[{id,name,arguments}]` list. Mirror of
    /// `BaseLLMClient.parse_tool_calls`.
    pub fn parse_tool_calls(&self, assistant_message: &Map<String, Value>) -> Result<Vec<Value>> {
        let raw_tool_calls = assistant_message
            .get("tool_calls")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let items = match raw_tool_calls {
            Value::Array(items) => items,
            Value::Null => Vec::new(),
            _ => bail!("Invalid tool_calls: must be array"),
        };

        let mut parsed: Vec<Value> = Vec::with_capacity(items.len());
        for (index, item) in items.into_iter().enumerate() {
            let obj = item
                .as_object()
                .ok_or_else(|| anyhow!("Invalid tool_call at index {index}: must be object"))?;
            let function = obj
                .get("function")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                bail!("Invalid tool_call at index {index}: missing function.name");
            }

            let mut arg_parse_error: Option<String> = None;
            let parsed_args = match function.get("arguments") {
                Some(Value::Object(map)) => Value::Object(map.clone()),
                Some(Value::String(raw)) => {
                    let trimmed = raw.trim();
                    if trimmed.is_empty() {
                        Value::Object(Map::new())
                    } else {
                        match serde_json::from_str::<Value>(trimmed) {
                            Ok(value) => value,
                            Err(_err) => {
                                arg_parse_error = Some(
                                    "Tool arguments were invalid JSON and could not be parsed."
                                        .to_string(),
                                );
                                Value::Object(Map::new())
                            }
                        }
                    }
                }
                Some(Value::Null) | None => Value::Object(Map::new()),
                Some(_) => {
                    arg_parse_error = Some(
                        "Tool arguments must be a JSON object and could not be parsed.".to_string(),
                    );
                    Value::Object(Map::new())
                }
            };
            let parsed_args = if parsed_args.is_object() {
                parsed_args
            } else {
                arg_parse_error = Some(
                    "Tool arguments must be a JSON object and could not be parsed.".to_string(),
                );
                Value::Object(Map::new())
            };

            let id = obj
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("tool_call_{index}"));

            let mut parsed_call = json!({
                "id": id,
                "name": name,
                "arguments": parsed_args,
            });
            if let Some(error) = arg_parse_error
                && let Value::Object(map) = &mut parsed_call
            {
                map.insert(TOOL_ARG_PARSE_ERROR_FIELD.to_string(), Value::String(error));
            }
            parsed.push(parsed_call);
        }
        Ok(parsed)
    }

    // ── Internals ────────────────────────────────────────────────────────

    fn tools_for_request(&self, tools: Option<&[Value]>) -> Option<Vec<Value>> {
        if !self.capabilities.tool_use {
            return None;
        }
        tools.map(|explicit| explicit.to_vec())
    }

    fn build_request_body(
        &self,
        messages: &[Value],
        model_name: Option<&str>,
        tools: Option<&[Value]>,
        stream: bool,
        reasoning_mode: Option<&str>,
    ) -> Result<Value> {
        let mut kwargs: Map<String, Value> = Map::new();
        kwargs.insert(
            "model".to_string(),
            Value::String(
                model_name
                    .map(str::to_string)
                    .unwrap_or_else(|| self.config.model_id.clone()),
            ),
        );
        kwargs.insert(
            "messages".to_string(),
            Value::Array(prepare_messages(messages)),
        );
        if let Some(tools) = tools {
            kwargs.insert("tools".to_string(), Value::Array(tools.to_vec()));
        } else {
            kwargs.insert("tools".to_string(), Value::Null);
        }
        kwargs.insert("stream".to_string(), Value::Bool(stream));
        if stream {
            kwargs.insert("stream_options".to_string(), json!({"include_usage": true}));
        }
        if let Some(t) = self.config.temperature {
            kwargs.insert("temperature".to_string(), json!(t));
        }
        if let Some(t) = self.config.top_p {
            kwargs.insert("top_p".to_string(), json!(t));
        }
        if let Some(t) = self.config.max_tokens {
            kwargs.insert("max_tokens".to_string(), json!(t));
        }
        let effective_reasoning = reasoning_mode.unwrap_or(&self.default_reasoning_mode);
        let reasoning_kwargs = self
            .reasoning_shaper
            .reasoning_kwargs(effective_reasoning)?;
        for (k, v) in reasoning_kwargs {
            kwargs.insert(k, v);
        }

        // Drop nulls, matching `{key: value for key, value in kwargs.items() if value is not None}`.
        let filtered: Map<String, Value> =
            kwargs.into_iter().filter(|(_, v)| !v.is_null()).collect();
        Ok(Value::Object(filtered))
    }

    fn build_headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(api_key) = &self.api_key {
            let header_value = HeaderValue::from_str(&format!("Bearer {api_key}"))
                .context("build Authorization header")?;
            headers.insert(AUTHORIZATION, header_value);
        }
        Ok(headers)
    }

    fn chat_completions_url(&self) -> String {
        let base = self.api_base.trim_end_matches('/');
        format!("{base}/chat/completions")
    }

    async fn send_chat_completion_with_retries(
        &self,
        body: &Value,
        stream: bool,
        options: &LlmRequestOptions,
    ) -> Result<reqwest::Response> {
        let url = self.chat_completions_url();
        let headers = self.build_headers()?;

        for attempt in 0..=options.retry.request_max_retries {
            raise_if_llm_cancelled(options.cancel.as_ref())?;
            let request = self
                .http
                .post(&url)
                .headers(headers.clone())
                .json(body)
                .send();
            let result = match options.cancel.as_ref() {
                Some(cancel) => {
                    tokio::select! {
                        _ = cancel.wait_cancelled() => {
                            return Err(anyhow::Error::new(LlmRequestCancelled));
                        }
                        result = request => result
                    }
                }
                None => request.await,
            }
            .and_then(reqwest::Response::error_for_status);

            match result {
                Ok(response) => return Ok(response),
                Err(err)
                    if attempt < options.retry.request_max_retries
                        && is_retryable_llm_error(&err) =>
                {
                    let retry_count = attempt + 1;
                    let delay = retry_delay(options.retry.base_delay, retry_count);
                    perf_log(
                        "llm_request_retry",
                        &json!({
                            "stream": stream,
                            "attempt": retry_count,
                            "max_retries": options.retry.request_max_retries,
                            "delay_ms": delay.as_millis() as u64,
                            "error": err.to_string(),
                        }),
                    );
                    report_retry(
                        options.on_retry.as_ref(),
                        LlmRetryEvent {
                            kind: LlmRetryKind::Request,
                            attempt: retry_count,
                            max_retries: options.retry.request_max_retries,
                            delay,
                            error: err.to_string(),
                        },
                    )
                    .await;
                    sleep_with_cancel(delay, options.cancel.as_ref()).await?;
                }
                Err(err) => {
                    let label = if stream {
                        "llm stream request"
                    } else {
                        "llm request"
                    };
                    return Err(err).with_context(|| label);
                }
            }
        }

        Err(anyhow!("llm retry loop exited unexpectedly"))
    }
}

fn is_retryable_llm_error(err: &reqwest::Error) -> bool {
    if err.is_timeout() || err.is_connect() {
        return true;
    }
    match err.status() {
        Some(status) => is_retryable_status(status),
        None => false,
    }
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::CONFLICT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

async fn next_stream_event<S, E>(
    event_stream: &mut S,
    idle_timeout: Duration,
    cancel: Option<&LlmCancelToken>,
) -> std::result::Result<Option<Event>, StreamAttemptError>
where
    S: futures::Stream<Item = std::result::Result<Event, EventStreamError<E>>> + Unpin,
    E: std::fmt::Display + std::fmt::Debug + Send + Sync + 'static,
{
    let wait_next = timeout(idle_timeout, event_stream.next());
    let response = match cancel {
        Some(cancel) => {
            tokio::select! {
                _ = cancel.wait_cancelled() => {
                    return Err(StreamAttemptError::Fatal(anyhow::Error::new(LlmRequestCancelled)));
                }
                response = wait_next => response
            }
        }
        None => wait_next.await,
    };

    match response {
        Ok(Some(Ok(event))) => Ok(Some(event)),
        Ok(Some(Err(err))) => Err(StreamAttemptError::Retryable(
            anyhow!(err).context("llm stream event"),
        )),
        Ok(None) => Ok(None),
        Err(_) => Err(StreamAttemptError::Retryable(anyhow!(
            "llm stream idle timeout after {}ms",
            idle_timeout.as_millis()
        ))),
    }
}

async fn report_retry(callback: Option<&LlmRetryCallback>, event: LlmRetryEvent) {
    if let Some(callback) = callback {
        let _ = callback(event).await;
    }
}

fn raise_if_llm_cancelled(cancel: Option<&LlmCancelToken>) -> Result<()> {
    if cancel.is_some_and(LlmCancelToken::is_cancelled) {
        return Err(anyhow::Error::new(LlmRequestCancelled));
    }
    Ok(())
}

async fn sleep_with_cancel(duration: Duration, cancel: Option<&LlmCancelToken>) -> Result<()> {
    match cancel {
        Some(cancel) => {
            tokio::select! {
                _ = cancel.wait_cancelled() => Err(anyhow::Error::new(LlmRequestCancelled)),
                _ = sleep(duration) => Ok(()),
            }
        }
        None => {
            sleep(duration).await;
            Ok(())
        }
    }
}

fn retry_delay(base_delay: Duration, attempt: u32) -> Duration {
    let raw = raw_retry_delay(base_delay, attempt);
    apply_jitter(raw)
}

fn raw_retry_delay(base_delay: Duration, attempt: u32) -> Duration {
    base_delay * 2u32.saturating_pow(attempt.saturating_sub(1))
}

fn apply_jitter(duration: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let jitter_basis = 9000_u128 + u128::from(nanos % 2001);
    let millis = duration.as_millis().saturating_mul(jitter_basis) / 10_000;
    Duration::from_millis(u64::try_from(millis).unwrap_or(u64::MAX).max(1))
}

fn merge_tool_call_deltas(
    tool_call_map: &mut BTreeMap<u64, Map<String, Value>>,
    tool_call_deltas: &[Value],
) {
    for delta in tool_call_deltas {
        let Some(obj) = delta.as_object() else {
            continue;
        };
        let index = obj.get("index").and_then(Value::as_u64).unwrap_or(0);
        let entry = tool_call_map.entry(index).or_insert_with(|| {
            let mut base = Map::new();
            base.insert("id".to_string(), Value::String(String::new()));
            base.insert("type".to_string(), Value::String("function".to_string()));
            let mut func = Map::new();
            func.insert("name".to_string(), Value::String(String::new()));
            func.insert("arguments".to_string(), Value::String(String::new()));
            base.insert("function".to_string(), Value::Object(func));
            base
        });
        if let Some(id) = obj.get("id").and_then(Value::as_str)
            && !id.is_empty()
        {
            entry.insert("id".to_string(), Value::String(id.to_string()));
        }
        if let Some(t) = obj.get("type").and_then(Value::as_str)
            && !t.is_empty()
        {
            entry.insert("type".to_string(), Value::String(t.to_string()));
        }
        let Some(delta_func) = obj.get("function") else {
            continue;
        };
        if delta_func.is_null() {
            continue;
        }
        let Some(delta_func_obj) = delta_func.as_object() else {
            continue;
        };
        let Some(entry_func) = entry.get_mut("function").and_then(|v| v.as_object_mut()) else {
            continue;
        };
        if let Some(name) = delta_func_obj.get("name").and_then(Value::as_str)
            && !name.is_empty()
        {
            let current = entry_func
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            entry_func.insert("name".to_string(), Value::String(current + name));
        }
        if let Some(args) = delta_func_obj.get("arguments").and_then(Value::as_str)
            && !args.is_empty()
        {
            let current = entry_func
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            entry_func.insert("arguments".to_string(), Value::String(current + args));
        }
    }
}

/// Remove keys whose values are JSON null. Mirrors Pydantic's
/// `exclude_none=True` used when normalising model responses.
fn prune_nulls(map: Map<String, Value>) -> Map<String, Value> {
    map.into_iter().filter(|(_, v)| !v.is_null()).collect()
}

/// Shape messages the same way Python's `_prepare_messages` does.
fn prepare_messages(messages: &[Value]) -> Vec<Value> {
    messages.iter().map(prepare_message).collect()
}

fn prepare_message(message: &Value) -> Value {
    let Some(obj) = message.as_object() else {
        return message.clone();
    };
    let mut payload = obj.clone();
    let role = payload
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let content = payload.get("content").cloned().unwrap_or(Value::Null);
    let new_content = match content {
        Value::Array(items) => Value::Array(items.iter().map(prepare_content_part).collect()),
        Value::Null => Value::Null,
        Value::String(s) => Value::String(s),
        other => Value::String(serde_json::to_string(&other).unwrap_or_default()),
    };
    payload.insert("content".to_string(), new_content.clone());
    if role == "tool" && matches!(new_content, Value::Null) {
        payload.insert("content".to_string(), Value::String(String::new()));
    }
    Value::Object(payload)
}

fn prepare_content_part(part: &Value) -> Value {
    let Some(obj) = part.as_object() else {
        return json!({"type": "text", "text": value_to_string(part)});
    };

    let part_type = obj
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");

    match part_type {
        "text" | "input_text" => {
            let text = obj
                .get("text")
                .or_else(|| obj.get("input_text"))
                .cloned()
                .unwrap_or(Value::Null);
            let text_str = match text {
                Value::Null => String::new(),
                Value::String(s) => s,
                other => value_to_string(&other),
            };
            json!({"type": "text", "text": text_str})
        }
        "image" => {
            let data = obj
                .get("data")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            let mime_type = obj
                .get("mimeType")
                .or_else(|| obj.get("mime_type"))
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if !data.is_empty() && !mime_type.is_empty() {
                return json!({
                    "type": "image_url",
                    "image_url": {"url": format!("data:{mime_type};base64,{data}")},
                });
            }
            let uri = obj
                .get("uri")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if !uri.is_empty() {
                return json!({"type": "image_url", "image_url": {"url": uri}});
            }
            json!({"type": "text", "text": serde_json::to_string(part).unwrap_or_default()})
        }
        "image_url" | "input_image" => {
            let url = match obj.get("image_url") {
                Some(Value::Object(m)) => m
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("")
                    .to_string(),
                Some(Value::String(s)) => s.trim().to_string(),
                _ => obj
                    .get("url")
                    .or_else(|| obj.get("uri"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("")
                    .to_string(),
            };
            if !url.is_empty() {
                return json!({"type": "image_url", "image_url": {"url": url}});
            }
            json!({"type": "text", "text": serde_json::to_string(part).unwrap_or_default()})
        }
        "resource" | "resource_link" => {
            json!({"type": "text", "text": serde_json::to_string(part).unwrap_or_default()})
        }
        _ => json!({"type": "text", "text": serde_json::to_string(part).unwrap_or_default()}),
    }
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc as StdArc, Mutex as StdMutex, Once};
    use std::thread;

    fn test_client(tool_use: bool) -> BaseLlmClient {
        test_client_with_base(tool_use, "https://example.test/v1")
    }

    fn test_client_with_base(tool_use: bool, api_base: &str) -> BaseLlmClient {
        ensure_loopback_no_proxy();
        BaseLlmClient::new(
            ModelConfig {
                provider: "test".to_string(),
                model_id: "test-model".to_string(),
                api_key_env: None,
                api_key: Some("test-key".to_string()),
                api_base: Some(api_base.to_string()),
                temperature: None,
                top_p: None,
                timeout_seconds: None,
                max_tokens: None,
            },
            ModelCapabilities {
                vision: false,
                tool_use,
            },
            "auto",
            Box::new(PassthroughReasoning),
        )
        .unwrap()
    }

    fn ensure_loopback_no_proxy() {
        static INIT: Once = Once::new();
        INIT.call_once(|| unsafe {
            std::env::set_var("NO_PROXY", "127.0.0.1,localhost,::1");
            std::env::set_var("no_proxy", "127.0.0.1,localhost,::1");
        });
    }

    fn read_http_request(stream: &mut TcpStream) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut content_length = 0_usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(raw) = lower.strip_prefix("content-length:") {
                content_length = raw.trim().parse().unwrap_or(0);
            }
        }
        if content_length > 0 {
            let mut body = vec![0_u8; content_length];
            reader.read_exact(&mut body).unwrap();
        }
    }

    fn write_sse_response(mut stream: TcpStream, body: &str) {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
    }

    #[test]
    fn parse_tool_calls_accepts_json_string_arguments() {
        let client = test_client(true);
        let assistant = serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {
                    "name": "terminal_exec",
                    "arguments": "{\"command\":\"cargo check\"}"
                }
            }]
        });
        let Value::Object(message) = assistant else {
            panic!("assistant payload must be object");
        };

        let calls = client.parse_tool_calls(&message).unwrap();

        assert_eq!(
            calls,
            vec![serde_json::json!({
                "id": "call_1",
                "name": "terminal_exec",
                "arguments": {"command": "cargo check"},
            })]
        );
    }

    #[test]
    fn parse_tool_calls_marks_non_object_arguments_as_recoverable_error() {
        let client = test_client(true);
        let assistant = serde_json::json!({
            "tool_calls": [{
                "id": "call_bad",
                "function": {
                    "name": "terminal_exec",
                    "arguments": "[1,2,3]"
                }
            }]
        });
        let Value::Object(message) = assistant else {
            panic!("assistant payload must be object");
        };

        let calls = client.parse_tool_calls(&message).unwrap();

        assert_eq!(calls[0]["id"], "call_bad");
        assert_eq!(calls[0]["name"], "terminal_exec");
        assert_eq!(calls[0]["arguments"], serde_json::json!({}));
        assert!(
            calls[0][TOOL_ARG_PARSE_ERROR_FIELD]
                .as_str()
                .unwrap()
                .contains("must be a JSON object")
        );
    }

    #[test]
    fn parse_tool_calls_marks_malformed_json_arguments_as_recoverable_error() {
        let client = test_client(true);
        let assistant = serde_json::json!({
            "tool_calls": [{
                "id": "call_file",
                "function": {
                    "name": "file_edit",
                    "arguments": "{\"patch\":\"*** Begin Patch"
                }
            }]
        });
        let Value::Object(message) = assistant else {
            panic!("assistant payload must be object");
        };

        let calls = client.parse_tool_calls(&message).unwrap();

        assert_eq!(calls[0]["id"], "call_file");
        assert_eq!(calls[0]["name"], "file_edit");
        assert_eq!(calls[0]["arguments"], serde_json::json!({}));
        let error = calls[0][TOOL_ARG_PARSE_ERROR_FIELD].as_str().unwrap();
        assert_eq!(
            error,
            "Tool arguments were invalid JSON and could not be parsed."
        );
    }

    #[test]
    fn prepare_message_rewrites_images_for_text_models() {
        let message = serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "data": "abc", "mimeType": "image/png"}
            ]
        });

        let prepared = prepare_message(&message);

        assert_eq!(
            prepared["content"],
            serde_json::json!([
                {"type": "text", "text": "hello"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,abc"}}
            ])
        );
    }

    #[tokio::test]
    async fn stream_retry_discards_failed_attempt_buffer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = StdArc::new(AtomicUsize::new(0));
        let attempts_for_thread = attempts.clone();
        thread::spawn(move || {
            for stream in listener.incoming().take(2) {
                let mut stream = stream.unwrap();
                read_http_request(&mut stream);
                let attempt = attempts_for_thread.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    let partial = json!({
                        "choices": [{"index": 0, "delta": {"content": "partial "}}]
                    });
                    let body = format!("data: {partial}\n\n");
                    write_sse_response(stream, &body);
                } else {
                    let final_chunk = json!({
                        "choices": [{"index": 0, "delta": {"content": "final"}}]
                    });
                    let usage = json!({
                        "choices": [],
                        "usage": {
                            "prompt_tokens": 1,
                            "completion_tokens": 1,
                            "total_tokens": 2
                        }
                    });
                    let body = format!("data: {final_chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
                    write_sse_response(stream, &body);
                }
            }
        });

        let client = test_client_with_base(false, &format!("http://{addr}/v1"));
        let chunks = StdArc::new(StdMutex::new(Vec::<String>::new()));
        let chunks_for_cb = chunks.clone();
        let on_text: StreamChunkCallback = Arc::new(move |chunk| {
            let chunks = chunks_for_cb.clone();
            Box::pin(async move {
                chunks.lock().unwrap().push(chunk);
                Ok(())
            })
        });
        let retry_events = StdArc::new(StdMutex::new(Vec::<LlmRetryEvent>::new()));
        let retry_events_for_cb = retry_events.clone();
        let on_retry: LlmRetryCallback = Arc::new(move |event| {
            let retry_events = retry_events_for_cb.clone();
            Box::pin(async move {
                retry_events.lock().unwrap().push(event);
                Ok(())
            })
        });

        let response = client
            .request_stream_with_usage(
                &[json!({"role": "user", "content": "hi"})],
                None,
                Some(&[]),
                Some(on_text),
                None,
                None,
                LlmRequestOptions {
                    retry: LlmRetryPolicy {
                        request_max_retries: 0,
                        stream_max_retries: 1,
                        stream_idle_timeout: Duration::from_secs(5),
                        base_delay: Duration::from_millis(1),
                    },
                    cancel: None,
                    on_retry: Some(on_retry),
                },
            )
            .await
            .unwrap();

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(response.message["content"], "final");
        assert_eq!(response.total_tokens, 2);
        assert_eq!(
            chunks.lock().unwrap().as_slice(),
            ["partial ".to_string(), "final".to_string()]
        );
        let events = retry_events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].kind, LlmRetryKind::Stream));
        assert_eq!(events[0].attempt, 1);
        assert_eq!(events[0].max_retries, 1);
    }

    #[tokio::test]
    async fn stream_response_records_finish_reason() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_request(&mut stream);
            let final_chunk = json!({
                "choices": [{
                    "index": 0,
                    "delta": {"content": "partial"},
                    "finish_reason": "length"
                }]
            });
            let usage = json!({
                "choices": [],
                "usage": {
                    "prompt_tokens": 1,
                    "completion_tokens": 4096,
                    "total_tokens": 4097
                }
            });
            let body = format!("data: {final_chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
            write_sse_response(stream, &body);
        });

        let client = test_client_with_base(false, &format!("http://{addr}/v1"));
        let response = client
            .request_stream_with_usage(
                &[json!({"role": "user", "content": "hi"})],
                None,
                Some(&[]),
                None,
                None,
                None,
                LlmRequestOptions {
                    retry: LlmRetryPolicy {
                        request_max_retries: 0,
                        stream_max_retries: 0,
                        stream_idle_timeout: Duration::from_secs(5),
                        base_delay: Duration::from_millis(1),
                    },
                    cancel: None,
                    on_retry: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(response.message["content"], "partial");
        assert_eq!(response.finish_reason.as_deref(), Some("length"));
        assert_eq!(response.usage.completion_tokens, 4096);
    }

    #[test]
    fn retry_status_filter_only_accepts_transient_statuses() {
        assert!(is_retryable_status(reqwest::StatusCode::REQUEST_TIMEOUT));
        assert!(is_retryable_status(reqwest::StatusCode::CONFLICT));
        assert!(is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(reqwest::StatusCode::BAD_GATEWAY));

        assert!(!is_retryable_status(reqwest::StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(reqwest::StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(reqwest::StatusCode::NOT_FOUND));
    }

    #[test]
    fn retry_delay_uses_bounded_exponential_backoff() {
        assert_eq!(
            raw_retry_delay(Duration::from_millis(200), 1),
            Duration::from_millis(200)
        );
        assert_eq!(
            raw_retry_delay(Duration::from_millis(200), 2),
            Duration::from_millis(400)
        );
        assert_eq!(
            raw_retry_delay(Duration::from_millis(200), 3),
            Duration::from_millis(800)
        );
    }

    #[test]
    fn retry_delay_applies_small_jitter() {
        let delay = retry_delay(Duration::from_millis(200), 1);
        assert!(delay >= Duration::from_millis(180));
        assert!(delay <= Duration::from_millis(220));
    }
}
