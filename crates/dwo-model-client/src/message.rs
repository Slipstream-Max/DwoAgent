use std::collections::BTreeMap;

use dwo_context::{
    ContentBlock, ContextMessage, EmbeddedResourceContents, MessageContent, MessageRole,
};
use serde_json::{Map, Value, json};

use crate::{FinishReason, ModelClientError, ModelReply, ModelUsage, StreamToolCall};

#[derive(Debug, Default)]
pub(crate) struct StreamAccumulator {
    pub content: String,
    pub reasoning: String,
    pub output_items: BTreeMap<u64, Value>,
    pub response: Option<Value>,
}

impl StreamAccumulator {
    pub fn add_output_item(&mut self, index: u64, item: Value) {
        self.output_items.insert(index, item);
    }

    pub fn update_function_arguments(
        &mut self,
        index: u64,
        arguments: &str,
        append: bool,
    ) -> Option<&Value> {
        let item = self
            .output_items
            .entry(index)
            .or_insert_with(|| json!({"type":"function_call", "arguments":""}));
        let object = item.as_object_mut()?;
        let arguments = if append {
            format!(
                "{}{}",
                object
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                arguments
            )
        } else {
            arguments.to_string()
        };
        object.insert("arguments".to_string(), Value::String(arguments));
        Some(item)
    }

    pub fn set_output_item_status(
        &mut self,
        index: Option<u64>,
        item_id: Option<&str>,
        status: &str,
    ) -> Option<&Value> {
        let item = match index {
            Some(index) => self.output_items.get_mut(&index),
            None => {
                let item_id = item_id?;
                self.output_items.values_mut().find(|item| {
                    item.get("id")
                        .or_else(|| item.get("call_id"))
                        .and_then(Value::as_str)
                        == Some(item_id)
                })
            }
        }?;
        item.as_object_mut()?
            .insert("status".to_string(), Value::String(status.to_string()));
        Some(item)
    }

    pub fn finish(self) -> Result<ModelReply, ModelClientError> {
        if let Some(response) = self.response {
            return parse_response(&response);
        }
        let output = self.output_items.into_values().collect::<Vec<_>>();
        reply_from_output(
            self.content,
            (!self.reasoning.is_empty()).then_some(self.reasoning),
            output,
            FinishReason::Stop,
            ModelUsage::default(),
        )
    }
}

pub(crate) fn provider_input(
    messages: &[ContextMessage],
    allow_image_input: bool,
) -> Result<Vec<Value>, ModelClientError> {
    let mut output = Vec::new();
    for message in messages {
        if let Some(item) = message.response_item.as_ref() {
            let mut item = item.clone();
            if let Some(item) = item.as_object_mut() {
                item.remove("id");
            }
            output.push(item);
            continue;
        }
        match message.role {
            MessageRole::System | MessageRole::User | MessageRole::Assistant => {
                if !message.content.is_empty() {
                    output.push(json!({
                        "type":"message",
                        "role":role_name(message.role),
                        "content":provider_content(&message.content, message.role, allow_image_input)?,
                    }));
                }
                if message.role == MessageRole::Assistant {
                    output.extend(
                        message
                            .tool_calls
                            .iter()
                            .map(provider_function_call)
                            .collect::<Result<Vec<_>, _>>()?,
                    );
                }
            }
            MessageRole::Tool => output.push(json!({
                "type":"function_call_output",
                "call_id":message.tool_call_id.as_deref().unwrap_or_default(),
                "output":content_as_output(&message.content)?,
            })),
        }
    }
    Ok(output)
}

pub(crate) fn provider_tools(
    local_tools: &[Value],
    hosted_tools: &[Value],
) -> Result<Vec<Value>, ModelClientError> {
    let mut tools = hosted_tools.to_vec();
    for tool in local_tools {
        let object = tool
            .as_object()
            .ok_or_else(|| ModelClientError::protocol("tool definition must be an object"))?;
        if object.get("type").and_then(Value::as_str) != Some("function") {
            tools.push(tool.clone());
            continue;
        }
        let function = object
            .get("function")
            .and_then(Value::as_object)
            .unwrap_or(object);
        let mut flattened = function.clone();
        flattened.insert("type".to_string(), Value::String("function".to_string()));
        tools.push(Value::Object(flattened));
    }
    Ok(tools)
}

fn provider_content(
    content: &MessageContent,
    role: MessageRole,
    allow_image_input: bool,
) -> Result<Vec<Value>, ModelClientError> {
    if let Some(text) = content.as_text() {
        return Ok(vec![json!({"type":"input_text", "text":text})]);
    }
    let mut blocks = Vec::with_capacity(content.as_blocks().len());
    for block in content.as_blocks() {
        match block {
            ContentBlock::Text { text, .. } => blocks.push(json!({
                "type":"input_text",
                "text":text,
            })),
            ContentBlock::Image {
                mime_type, data, ..
            } if role == MessageRole::User && allow_image_input => {
                blocks.push(json!({
                    "type":"input_image",
                    "image_url":format!("data:{mime_type};base64,{data}"),
                }));
            }
            ContentBlock::Image { .. } => {
                let reason = if role != MessageRole::User {
                    "images are only valid in user messages"
                } else {
                    "selected model does not support image input"
                };
                return Err(ModelClientError::protocol(reason));
            }
            ContentBlock::Resource {
                resource:
                    EmbeddedResourceContents::Text {
                        uri,
                        mime_type,
                        text,
                        ..
                    },
                ..
            } => {
                let mime = mime_type.as_deref().unwrap_or("text/plain");
                blocks.push(json!({
                    "type":"input_text",
                    "text":format!("<embedded_resource uri=\"{uri}\" mime_type=\"{mime}\">\n{text}\n</embedded_resource>")
                }));
            }
            ContentBlock::Resource {
                resource: EmbeddedResourceContents::Blob { .. },
                ..
            } => {
                return Err(ModelClientError::protocol(
                    "Responses API blob resource input is not implemented",
                ));
            }
            ContentBlock::ResourceLink {
                uri,
                name,
                mime_type,
                title,
                description,
                size,
                ..
            } => {
                let mut text = format!("Resource: {} ({uri})", title.as_deref().unwrap_or(name));
                if let Some(mime_type) = mime_type {
                    text.push_str(&format!("\nMIME type: {mime_type}"));
                }
                if let Some(size) = size {
                    text.push_str(&format!("\nSize: {size} bytes"));
                }
                if let Some(description) = description {
                    text.push_str(&format!("\n{description}"));
                }
                blocks.push(json!({"type":"input_text", "text":text}));
            }
            ContentBlock::Audio { .. } => {
                return Err(ModelClientError::protocol(
                    "Responses API audio input is not implemented",
                ));
            }
        }
    }
    Ok(blocks)
}

fn role_name(role: MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "user",
    }
}

fn content_as_output(content: &MessageContent) -> Result<String, ModelClientError> {
    if let Some(text) = content.as_text() {
        return Ok(text.to_string());
    }
    serde_json::to_string(content).map_err(|error| ModelClientError::protocol(error.to_string()))
}

fn provider_function_call(call: &Value) -> Result<Value, ModelClientError> {
    let object = call
        .as_object()
        .ok_or_else(|| ModelClientError::protocol("stored tool call must be an object"))?;
    Ok(json!({
        "type":"function_call",
        "call_id":object.get("id").and_then(Value::as_str).unwrap_or_default(),
        "name":object.get("name").and_then(Value::as_str).unwrap_or_default(),
        "arguments":arguments_string(object.get("arguments"))?,
    }))
}

fn arguments_string(value: Option<&Value>) -> Result<String, ModelClientError> {
    match value {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(value) => serde_json::to_string(value)
            .map_err(|error| ModelClientError::protocol(error.to_string())),
        None => Ok("{}".to_string()),
    }
}

pub(crate) fn parse_response(payload: &Value) -> Result<ModelReply, ModelClientError> {
    let output = payload
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| ModelClientError::protocol("missing response.output"))?;
    let status = payload.get("status").and_then(Value::as_str);
    let finish_reason = match status {
        Some("incomplete") => FinishReason::Length,
        Some("completed") | None => FinishReason::Stop,
        Some(other) => FinishReason::Other(other.to_string()),
    };
    reply_from_output(
        response_text(&output),
        response_reasoning(&output),
        output,
        finish_reason,
        usage(payload.get("usage")),
    )
}

fn reply_from_output(
    content: String,
    reasoning: Option<String>,
    output_items: Vec<Value>,
    mut finish_reason: FinishReason,
    usage: ModelUsage,
) -> Result<ModelReply, ModelClientError> {
    let tool_calls = normalize_function_calls(&output_items);
    if !tool_calls.is_empty() {
        finish_reason = FinishReason::ToolCalls;
    }
    let remote_tool_calls = normalize_remote_tool_calls(&output_items);
    Ok(ModelReply {
        content,
        reasoning,
        tool_calls,
        remote_tool_calls,
        output_items,
        finish_reason,
        usage,
    })
}

fn response_text(output: &[Value]) -> String {
    output
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<String>()
}

fn response_reasoning(output: &[Value]) -> Option<String> {
    let text = output
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
        .filter_map(|item| item.get("summary").and_then(Value::as_array))
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn normalize_function_calls(output: &[Value]) -> Vec<Value> {
    output
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        .map(|item| {
            json!({
                "id":item.get("call_id").or_else(|| item.get("id")).and_then(Value::as_str).unwrap_or_default(),
                "name":item.get("name").and_then(Value::as_str).unwrap_or_default(),
                "arguments":normalize_arguments(item.get("arguments").and_then(Value::as_str).unwrap_or_default()),
            })
        })
        .collect()
}

fn normalize_remote_tool_calls(output: &[Value]) -> Vec<Value> {
    output
        .iter()
        .filter(|item| {
            item.get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.ends_with("_call") && kind != "function_call")
        })
        .map(|item| {
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("remote_tool_call");
            json!({
                "id":item.get("call_id").or_else(|| item.get("id")).and_then(Value::as_str).unwrap_or_default(),
                "name":kind.strip_suffix("_call").unwrap_or(kind),
                "arguments":item,
                "status":item.get("status").cloned().unwrap_or(Value::Null),
                "remote":true,
            })
        })
        .collect()
}

fn normalize_arguments(arguments: &str) -> Value {
    let arguments = arguments.trim();
    if arguments.is_empty() {
        return Value::Object(Map::new());
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(value) => value,
        Err(_) => Value::String(arguments.to_string()),
    }
}

pub(crate) fn stream_tool_call(item: &Value, hosted_tools: &[Value]) -> Option<StreamToolCall> {
    let kind = item.get("type").and_then(Value::as_str)?;
    let provider_hosted = kind != "function_call";
    if provider_hosted && !kind.ends_with("_call") {
        return None;
    }
    let tool_call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)?
        .to_string();
    if tool_call_id.is_empty() {
        return None;
    }
    let kind_name = canonical_tool_kind(kind);
    let tool_name = item
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .or_else(|| {
            hosted_tools.iter().find_map(|tool| {
                let configured = tool.get("type").and_then(Value::as_str)?;
                (canonical_tool_kind(configured) == kind_name)
                    .then(|| tool.get("name").and_then(Value::as_str))
                    .flatten()
                    .filter(|name| !name.is_empty())
            })
        })
        .unwrap_or(kind_name)
        .to_string();
    let raw_input = if provider_hosted {
        item.clone()
    } else {
        match item.get("arguments") {
            Some(Value::String(arguments)) => normalize_arguments(arguments),
            Some(arguments) => arguments.clone(),
            None => Value::Object(Map::new()),
        }
    };
    let status = if provider_hosted {
        match item.get("status").and_then(Value::as_str) {
            Some("searching" | "running" | "interpreting" | "generating") => "in_progress",
            Some("canceled") => "cancelled",
            Some(status) => status,
            None => "in_progress",
        }
    } else {
        "pending"
    }
    .to_string();
    Some(StreamToolCall {
        tool_call_id,
        tool_name,
        raw_input,
        status,
    })
}

pub(crate) fn provider_tool_event_state(event_type: &str) -> Option<&'static str> {
    let (kind, phase) = event_type.strip_prefix("response.")?.rsplit_once('.')?;
    if kind == "function_call" || !kind.ends_with("_call") {
        return None;
    }
    match phase {
        "in_progress" | "searching" | "running" | "interpreting" | "generating" => {
            Some("in_progress")
        }
        "completed" => Some("completed"),
        "failed" => Some("failed"),
        "cancelled" | "canceled" => Some("cancelled"),
        _ => None,
    }
}

fn canonical_tool_kind(kind: &str) -> &str {
    kind.strip_suffix("_call")
        .or_else(|| kind.strip_suffix("_preview"))
        .unwrap_or(kind)
}

pub(crate) fn usage(value: Option<&Value>) -> ModelUsage {
    let Some(value) = value else {
        return ModelUsage::default();
    };
    let input_tokens = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let output_tokens = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let total_tokens = value
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| input_tokens.saturating_add(output_tokens));
    ModelUsage {
        input_tokens,
        output_tokens,
        total_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_input_replays_native_items_and_function_outputs() {
        let reasoning = ContextMessage::response_item(
            json!({"type":"reasoning", "id":"reason-1", "summary":[], "encrypted_content":"opaque"}),
            Some("provider-a".to_string()),
        );
        let hosted = ContextMessage::response_item(
            json!({"type":"web_search_call", "id":"ws-1", "status":"completed"}),
            Some("provider-a".to_string()),
        );
        let message = ContextMessage::response_item(
            json!({"type":"message", "id":"message-1", "role":"assistant", "content":[{"type":"output_text", "text":"done"}]}),
            None,
        );
        let result = dwo_context::ToolResultRecord {
            tool_call_id: "call-1".to_string(),
            tool_name: "terminal".to_string(),
            output: json!({"status":"ok"}),
            model_context: Vec::new(),
        };
        let input = provider_input(
            &[reasoning, hosted, message, ContextMessage::tool(&result)],
            false,
        )
        .unwrap();
        assert_eq!(input[0]["type"], "reasoning");
        assert!(input[0].get("id").is_none());
        assert_eq!(input[1]["type"], "web_search_call");
        assert!(input[1].get("id").is_none());
        assert_eq!(input[2]["type"], "message");
        assert!(input[2].get("id").is_none());
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call-1");
    }

    #[test]
    fn response_item_replay_drops_item_id_but_preserves_call_id() {
        let call = ContextMessage::response_item(
            json!({
                "type":"function_call",
                "id":"fc-1",
                "call_id":"call-1",
                "name":"terminal",
                "arguments":"{\"command\":\"pwd\"}"
            }),
            None,
        );
        let input = provider_input(&[call], false).unwrap();
        assert_eq!(input[0]["type"], "function_call");
        assert!(input[0].get("id").is_none());
        assert_eq!(input[0]["call_id"], "call-1");
    }

    #[test]
    fn response_parser_separates_local_and_hosted_tools() {
        let reply = parse_response(&json!({
            "status":"completed",
            "output":[
                {"type":"web_search_call", "id":"ws-1", "status":"completed"},
                {"type":"function_call", "call_id":"call-1", "name":"terminal", "arguments":"{\"command\":\"pwd\"}"},
                {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"ok"}]}
            ],
            "usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}
        })).unwrap();
        assert_eq!(reply.content, "ok");
        assert_eq!(reply.tool_calls[0]["name"], "terminal");
        assert_eq!(reply.remote_tool_calls[0]["name"], "web_search");
        assert_eq!(reply.usage.total_tokens, 5);
    }

    #[test]
    fn function_tools_are_flattened_for_responses() {
        let tools = provider_tools(
            &[json!({
                "type":"function",
                "function":{"name":"terminal","description":"run","parameters":{"type":"object"}}
            })],
            &[json!({"type":"web_search"})],
        )
        .unwrap();
        assert_eq!(tools[0]["type"], "web_search");
        assert_eq!(tools[1]["name"], "terminal");
        assert!(tools[1].get("function").is_none());
    }

    #[test]
    fn streamed_tools_use_call_id_and_generic_hosted_names() {
        let local = stream_tool_call(
            &json!({
                "type":"function_call",
                "id":"fc-1",
                "call_id":"call-1",
                "name":"terminal",
                "arguments":""
            }),
            &[],
        )
        .unwrap();
        assert_eq!(local.tool_call_id, "call-1");
        assert_eq!(local.status, "pending");

        let hosted = stream_tool_call(
            &json!({
                "type":"file_search_call",
                "id":"fs-1",
                "status":"in_progress"
            }),
            &[json!({"type":"file_search", "name":"knowledge_search"})],
        )
        .unwrap();
        assert_eq!(hosted.tool_call_id, "fs-1");
        assert_eq!(hosted.tool_name, "knowledge_search");
    }

    #[test]
    fn provider_tool_event_states_are_type_agnostic() {
        assert_eq!(
            provider_tool_event_state("response.file_search_call.searching"),
            Some("in_progress")
        );
        assert_eq!(
            provider_tool_event_state("response.image_generation_call.completed"),
            Some("completed")
        );
        assert_eq!(
            provider_tool_event_state("response.output_text.delta"),
            None
        );
    }
}
