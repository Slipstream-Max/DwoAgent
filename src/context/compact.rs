//! Context compaction.

use std::collections::BTreeMap;

use anyhow::{Error, Result, bail};
use once_cell::sync::OnceCell;
use serde_json::{Map, Value, json};
use tiktoken_rs::{CoreBPE, o200k_base};

use super::content_block::{image_placeholder, text as text_block};
use super::manager::CancelEvent;
use crate::llm::client::{BaseLlmClient, LlmRequestOptions};
use crate::templates;

const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 20_000;

static TOKEN_ENCODER: OnceCell<CoreBPE> = OnceCell::new();

fn token_encoder() -> Result<&'static CoreBPE> {
    TOKEN_ENCODER.get_or_try_init(|| o200k_base().map_err(Error::from))
}

fn read_template(name: &str) -> &'static str {
    match name {
        "compact_prompt.md" => templates::compact::COMPACT_PROMPT,
        "summary_prefix.md" => templates::compact::SUMMARY_PREFIX,
        other => unreachable!("unknown compact template: {other}"),
    }
}

/// Produce the compacted conversation, or `None` if the run was cancelled.
pub async fn compact_context(
    conversation: &[Value],
    model_client: &BaseLlmClient,
    cancel_event: Option<&CancelEvent>,
    reasoning_mode: Option<&str>,
    llm_options: LlmRequestOptions,
) -> Result<Option<Vec<Value>>> {
    let compact_prompt = read_template("compact_prompt.md").trim();
    let summary_prefix = read_template("summary_prefix.md").trim();

    let summary_suffix = match request_summary_suffix(
        conversation,
        compact_prompt,
        model_client,
        cancel_event,
        reasoning_mode,
        llm_options,
    )
    .await
    {
        Ok(Some(text)) => text,
        Ok(None) => return Ok(None),
        Err(err) => {
            if err.downcast_ref::<CompactionCancelled>().is_some() {
                return Ok(None);
            }
            return Err(err);
        }
    };

    let user_messages = collect_user_messages(conversation, summary_prefix)?;
    let compacted_user_messages = match build_compacted_user_messages(
        &user_messages,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
        cancel_event,
    ) {
        Ok(messages) => messages,
        Err(err) => {
            if err.downcast_ref::<CompactionCancelled>().is_some() {
                return Ok(None);
            }
            return Err(err);
        }
    };

    let summary_text = build_summary_text(summary_prefix, &summary_suffix);
    let mut out = compacted_user_messages;
    out.push(json!({"role": "user", "content": [text_block(&summary_text)?]}));
    Ok(Some(out))
}

async fn request_summary_suffix(
    conversation: &[Value],
    compact_prompt: &str,
    model_client: &BaseLlmClient,
    cancel_event: Option<&CancelEvent>,
    reasoning_mode: Option<&str>,
    llm_options: LlmRequestOptions,
) -> Result<Option<String>> {
    let mut remaining: Vec<Value> = conversation.to_vec();

    loop {
        raise_if_cancelled(cancel_event)?;
        let mut messages: Vec<Value> = Vec::with_capacity(remaining.len() + 1);
        messages.push(json!({"role": "system", "content": compact_prompt}));
        messages.extend(remaining.iter().cloned());

        let result = model_client
            .request_with_usage(
                &messages,
                None,
                Some(&[]),
                reasoning_mode,
                llm_options.clone(),
            )
            .await;

        match result {
            Ok(response) => {
                let content = response
                    .message
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow::anyhow!("Compaction summary content must be plain text.")
                    })?;
                let summary_text = content.trim();
                if summary_text.is_empty() {
                    bail!("Compaction summary content cannot be empty.");
                }
                return Ok(Some(summary_text.to_string()));
            }
            Err(err) => {
                if is_context_window_error(&err) {
                    if remaining.is_empty() {
                        return Err(err);
                    }
                    remaining.remove(0);
                    continue;
                }
                return Err(err);
            }
        }
    }
}

fn collect_user_messages(conversation: &[Value], summary_prefix: &str) -> Result<Vec<Value>> {
    let mut out: Vec<Value> = Vec::new();
    let prefix = summary_prefix.trim();

    for item in conversation {
        let role = item
            .get("role")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if role != "user" {
            continue;
        }
        let content = compact_user_content(item.get("content").unwrap_or(&Value::Null))?;
        if is_empty_user_content(&content)? {
            continue;
        }
        if !prefix.is_empty() && summary_detection_text(&content)?.starts_with(prefix) {
            continue;
        }
        out.push(content);
    }
    Ok(out)
}

fn build_compacted_user_messages(
    user_messages: &[Value],
    user_token_budget: usize,
    cancel_event: Option<&CancelEvent>,
) -> Result<Vec<Value>> {
    let mut remaining_tokens = user_token_budget;
    let mut token_cache: BTreeMap<String, usize> = BTreeMap::new();
    let mut selected_newest_first: Vec<Value> = Vec::new();

    for message_content in user_messages.iter().rev() {
        if remaining_tokens == 0 {
            break;
        }
        raise_if_cancelled(cancel_event)?;

        let tokens = count_content_tokens(message_content, &mut token_cache)?;
        if tokens <= remaining_tokens {
            selected_newest_first.push(message_content.clone());
            remaining_tokens -= tokens;
            continue;
        }

        if let Some(truncated) = truncate_user_content_to_budget(
            message_content,
            remaining_tokens,
            &mut token_cache,
            cancel_event,
        )? {
            selected_newest_first.push(truncated);
        }
        break;
    }

    let selected_in_order: Vec<Value> = selected_newest_first.into_iter().rev().collect();
    Ok(selected_in_order
        .into_iter()
        .map(|content| json!({"role": "user", "content": content}))
        .collect())
}

fn truncate_user_content_to_budget(
    content: &Value,
    token_budget: usize,
    token_cache: &mut BTreeMap<String, usize>,
    cancel_event: Option<&CancelEvent>,
) -> Result<Option<Value>> {
    if let Value::String(s) = content {
        let truncated = truncate_text_to_budget(s, token_budget, token_cache, cancel_event)?;
        if truncated.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Value::String(truncated)))
        }
    } else {
        Ok(None)
    }
}

fn truncate_text_to_budget(
    text: &str,
    token_budget: usize,
    token_cache: &mut BTreeMap<String, usize>,
    cancel_event: Option<&CancelEvent>,
) -> Result<String> {
    let normalized = text.trim();
    if token_budget == 0 || normalized.is_empty() {
        return Ok(String::new());
    }

    // Python slices by str length which counts code units (≈chars for ASCII);
    // for Unicode safety we operate on char indices.
    let chars: Vec<char> = normalized.chars().collect();
    let mut low: usize = 1;
    let mut high: usize = chars.len();
    let mut best = String::new();

    while low <= high {
        raise_if_cancelled(cancel_event)?;
        let mid = (low + high) / 2;
        let candidate_str: String = chars[..mid].iter().collect();
        let candidate = candidate_str.trim().to_string();
        if candidate.is_empty() {
            low = mid + 1;
            continue;
        }

        let token_count = count_content_tokens(&Value::String(candidate.clone()), token_cache)?;
        if token_count <= token_budget {
            best = candidate;
            low = mid + 1;
        } else {
            if mid == 0 {
                break;
            }
            high = mid - 1;
        }
    }
    Ok(best)
}

fn count_content_tokens(
    content: &Value,
    token_cache: &mut BTreeMap<String, usize>,
) -> Result<usize> {
    if is_empty_user_content(content)? {
        return Ok(0);
    }
    let cache_key = token_cache_key(content);
    if let Some(count) = token_cache.get(&cache_key) {
        return Ok(*count);
    }
    let token_text = content_to_token_text(content)?;
    let encoder = token_encoder()?;
    let count = encoder.encode_with_special_tokens(&token_text).len();
    token_cache.insert(cache_key, count);
    Ok(count)
}

fn content_to_token_text(content: &Value) -> Result<String> {
    match content {
        Value::String(s) => Ok(s.clone()),
        Value::Array(items) => {
            let mut parts: Vec<String> = Vec::new();
            for part in items {
                let obj = part.as_object().ok_or_else(|| {
                    anyhow::anyhow!("Compacted user content blocks must be objects.")
                })?;
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
                        let text_value = match text {
                            Value::Null => String::new(),
                            Value::String(s) => s,
                            other => other.to_string(),
                        };
                        if !text_value.is_empty() {
                            parts.push(text_value);
                        }
                    }
                    "resource" | "resource_link" => {
                        parts.push(serde_json::to_string(part)?);
                    }
                    other => bail!("Unsupported compacted user content block type: {other}"),
                }
            }
            Ok(parts.join("\n"))
        }
        _ => bail!("Compacted user content must be a string or list of content blocks."),
    }
}

fn build_summary_text(summary_prefix: &str, summary_suffix: &str) -> String {
    let prefix = summary_prefix.trim();
    let suffix = summary_suffix.trim();
    if prefix.is_empty() {
        return suffix.to_string();
    }
    if suffix.is_empty() {
        return prefix.to_string();
    }
    format!("{prefix}\n{suffix}")
}

fn compact_user_content(content: &Value) -> Result<Value> {
    match content {
        Value::String(_) => Ok(content.clone()),
        Value::Array(parts) => {
            let mut out: Vec<Value> = Vec::with_capacity(parts.len());
            for part in parts {
                out.push(compact_user_part(part)?);
            }
            Ok(Value::Array(out))
        }
        _ => bail!("User content must be a string or list of content blocks."),
    }
}

fn compact_user_part(part: &Value) -> Result<Value> {
    let obj = part
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("User content blocks must be objects."))?;
    let part_type = obj
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    match part_type {
        "image" | "image_url" | "input_image" => Ok(image_placeholder()),
        "text" | "input_text" | "resource" | "resource_link" => Ok(Value::Object(obj.clone())),
        other => bail!("Unsupported user content block type: {other}"),
    }
}

fn summary_detection_text(content: &Value) -> Result<String> {
    match content {
        Value::String(s) => Ok(s.trim().to_string()),
        Value::Array(items) => {
            let mut parts: Vec<String> = Vec::new();
            for item in items {
                let text = extract_text_part(item)?;
                if !text.is_empty() {
                    parts.push(text);
                }
            }
            Ok(parts.join("\n").trim().to_string())
        }
        _ => bail!("Summary detection content must be a string or list of content blocks."),
    }
}

fn extract_text_part(part: &Value) -> Result<String> {
    let obj = part
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Content blocks must be objects."))?;
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
                other => other.to_string(),
            };
            Ok(text_str.trim().to_string())
        }
        "resource" | "resource_link" => Ok(String::new()),
        other => bail!("Unsupported content block type for text extraction: {other}"),
    }
}

fn is_empty_user_content(content: &Value) -> Result<bool> {
    match content {
        Value::String(s) => Ok(s.trim().is_empty()),
        Value::Array(items) => Ok(items.is_empty()),
        _ => bail!("User content must be a string or list of content blocks."),
    }
}

fn token_cache_key(content: &Value) -> String {
    if let Value::String(s) = content {
        return format!("text:{s}");
    }
    serde_json::to_string(&sorted_value(content)).unwrap_or_default()
}

/// Recursively sort object keys so `json.dumps(sort_keys=True)` and our cache
/// key stay stable across runs.
fn sorted_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: Vec<(String, Value)> = map
                .iter()
                .map(|(k, v)| (k.clone(), sorted_value(v)))
                .collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = Map::new();
            for (k, v) in sorted {
                out.insert(k, v);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted_value).collect()),
        other => other.clone(),
    }
}

fn is_context_window_error(err: &Error) -> bool {
    let text = format!("{err:#}").to_lowercase();
    const MARKERS: &[&str] = &[
        "context window",
        "context length",
        "maximum context",
        "maximum token",
        "token limit",
        "prompt is too long",
        "too many tokens",
        "contextwindowexceeded",
    ];
    MARKERS.iter().any(|marker| text.contains(marker))
}

#[derive(Debug, thiserror::Error)]
#[error("compaction cancelled")]
struct CompactionCancelled;

fn raise_if_cancelled(cancel_event: Option<&CancelEvent>) -> Result<()> {
    if let Some(ev) = cancel_event
        && ev.is_set()
    {
        return Err(anyhow::Error::new(CompactionCancelled));
    }
    Ok(())
}
