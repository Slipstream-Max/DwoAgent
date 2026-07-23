use std::collections::BTreeMap;

use dwo_context::{
    ContentBlock, ContextMessage, EmbeddedResourceContents, MessageContent, MessageRole,
};
use serde_json::{Map, Value, json};

use crate::{ModelClientError, ModelUsage};

#[derive(Debug, Default)]
pub(crate) struct StreamAccumulator {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: BTreeMap<u64, PartialToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<ModelUsage>,
}

#[derive(Debug, Default)]
pub(crate) struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl StreamAccumulator {
    pub fn merge_tool_calls(&mut self, deltas: &[Value]) {
        for (fallback_index, delta) in deltas.iter().enumerate() {
            let Some(object) = delta.as_object() else {
                continue;
            };
            let index = object
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(fallback_index as u64);
            let call = self.tool_calls.entry(index).or_default();
            if let Some(id) = object.get("id").and_then(Value::as_str) {
                call.id.push_str(id);
            }
            let function = object.get("function").and_then(Value::as_object);
            if let Some(name) = function
                .and_then(|value| value.get("name"))
                .and_then(Value::as_str)
            {
                call.name.push_str(name);
            }
            if let Some(arguments) = function
                .and_then(|value| value.get("arguments"))
                .and_then(Value::as_str)
            {
                call.arguments.push_str(arguments);
            }
        }
    }

    pub fn normalized_tool_calls(self) -> Vec<Value> {
        self.tool_calls
            .into_values()
            .map(|call| {
                json!({
                    "id": call.id,
                    "name": call.name,
                    "arguments": normalize_arguments(&call.arguments),
                })
            })
            .collect()
    }
}

pub(crate) fn provider_messages(
    messages: &[ContextMessage],
    allow_image_input: bool,
) -> Result<Vec<Value>, ModelClientError> {
    let mut output = Vec::with_capacity(messages.len());
    for message in messages {
        output.push(provider_message(message, allow_image_input)?);
    }
    Ok(output)
}

fn provider_message(
    message: &ContextMessage,
    allow_image_input: bool,
) -> Result<Value, ModelClientError> {
    match message.role {
        MessageRole::System => Ok(json!({
            "role":"system",
            "content": provider_content(&message.content, false, allow_image_input)?
        })),
        MessageRole::User => Ok(json!({
            "role":"user",
            "content": provider_content(&message.content, true, allow_image_input)?
        })),
        MessageRole::Assistant => {
            let tool_calls = message
                .tool_calls
                .iter()
                .map(provider_tool_call)
                .collect::<Result<Vec<_>, _>>()?;
            let mut object = Map::new();
            object.insert("role".to_string(), Value::String("assistant".to_string()));
            object.insert(
                "content".to_string(),
                if message.content.is_empty() && !tool_calls.is_empty() {
                    Value::Null
                } else {
                    provider_content(&message.content, false, allow_image_input)?
                },
            );
            if !tool_calls.is_empty() {
                object.insert("tool_calls".to_string(), Value::Array(tool_calls));
            }
            if let Some(reasoning) = &message.reasoning {
                object.insert(
                    "reasoning_content".to_string(),
                    Value::String(reasoning.clone()),
                );
            }
            Ok(Value::Object(object))
        }
        MessageRole::Tool => Ok(json!({
            "role":"tool",
            "content":provider_content(&message.content, false, allow_image_input)?,
            "tool_call_id":message.tool_call_id,
        })),
    }
}

fn provider_content(
    content: &MessageContent,
    is_user_message: bool,
    allow_image_input: bool,
) -> Result<Value, ModelClientError> {
    if let Some(text) = content.as_text() {
        return Ok(Value::String(text.to_string()));
    }
    let mut blocks = Vec::with_capacity(content.as_blocks().len());
    for block in content.as_blocks() {
        match block {
            ContentBlock::Text { text, .. } => {
                blocks.push(json!({"type":"text", "text":text}));
            }
            ContentBlock::Image {
                mime_type, data, ..
            } if is_user_message && allow_image_input => {
                let url = format!("data:{mime_type};base64,{data}");
                blocks.push(json!({"type":"image_url", "image_url":{"url":url}}));
            }
            ContentBlock::Image { .. } => {
                let reason = if !is_user_message {
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
                    "type":"text",
                    "text":format!("<embedded_resource uri=\"{uri}\" mime_type=\"{mime}\">\n{text}\n</embedded_resource>")
                }));
            }
            ContentBlock::Resource {
                resource: EmbeddedResourceContents::Blob { .. },
                ..
            } => {
                return Err(ModelClientError::protocol(
                    "OpenAI-compatible chat completions do not support blob resources",
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
                blocks.push(json!({"type":"text", "text":text}));
            }
            ContentBlock::Audio { .. } => {
                return Err(ModelClientError::protocol(
                    "OpenAI-compatible chat completions audio input is not implemented",
                ));
            }
        }
    }
    Ok(Value::Array(blocks))
}

fn provider_tool_call(call: &Value) -> Result<Value, ModelClientError> {
    let object = call
        .as_object()
        .ok_or_else(|| ModelClientError::protocol("stored tool call must be an object"))?;
    let id = object.get("id").and_then(Value::as_str).unwrap_or_default();
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = match object.get("arguments") {
        Some(Value::String(arguments)) => arguments.clone(),
        Some(arguments) => serde_json::to_string(arguments)
            .map_err(|error| ModelClientError::protocol(error.to_string()))?,
        None => "{}".to_string(),
    };
    Ok(json!({
        "id":id,
        "type":"function",
        "function":{"name":name, "arguments":arguments},
    }))
}

pub(crate) fn normalize_response_tool_calls(value: Option<&Value>) -> Vec<Value> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|call| {
            let object = call.as_object()?;
            let function = object.get("function").and_then(Value::as_object);
            Some(json!({
                "id": object.get("id").and_then(Value::as_str).unwrap_or_default(),
                "name": function
                    .and_then(|value| value.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                "arguments": normalize_arguments(
                    function
                        .and_then(|value| value.get("arguments"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                ),
            }))
        })
        .collect()
}

fn normalize_arguments(arguments: &str) -> Value {
    let arguments = arguments.trim();
    if arguments.is_empty() {
        return Value::Object(Map::new());
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(object)) => Value::Object(object),
        Ok(other) => other,
        Err(_) => Value::String(arguments.to_string()),
    }
}

pub(crate) fn usage(value: Option<&Value>) -> ModelUsage {
    let Some(value) = value else {
        return ModelUsage::default();
    };
    let input_tokens = value
        .get("prompt_tokens")
        .or_else(|| value.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let output_tokens = value
        .get("completion_tokens")
        .or_else(|| value.get("output_tokens"))
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
    fn malformed_arguments_remain_recoverable_by_the_tool_executor() {
        let calls = normalize_response_tool_calls(Some(&json!([{
            "id":"broken",
            "function":{"name":"terminal", "arguments":"{not-json"}
        }])));

        assert_eq!(calls[0]["id"], "broken");
        assert_eq!(calls[0]["name"], "terminal");
        assert_eq!(calls[0]["arguments"], "{not-json");
    }

    #[test]
    fn provider_messages_restore_assistant_reasoning_and_tool_shape() {
        let message = ContextMessage::assistant_with_reasoning(
            "",
            Some("private reasoning".to_string()),
            vec![json!({
                "id":"call-1",
                "name":"terminal",
                "arguments":{"action":"run", "command":"pwd"}
            })],
        );
        let messages = provider_messages(&[message], false).unwrap();

        assert_eq!(messages[0]["reasoning_content"], "private reasoning");
        assert_eq!(messages[0]["content"], Value::Null);
        assert_eq!(messages[0]["tool_calls"][0]["function"]["name"], "terminal");
        assert_eq!(
            messages[0]["tool_calls"][0]["function"]["arguments"],
            r#"{"action":"run","command":"pwd"}"#
        );
    }

    #[test]
    fn image_input_requires_an_explicit_model_capability() {
        let message = ContextMessage::user(MessageContent::blocks(vec![ContentBlock::image(
            "image/png",
            "aGVsbG8=",
        )]));

        let error = provider_messages(&[message], false).unwrap_err();
        assert!(error.to_string().contains("does not support image input"));
    }

    #[test]
    fn response_usage_is_optional_because_context_is_estimated_locally() {
        assert_eq!(usage(None), ModelUsage::default());
        assert_eq!(usage(Some(&json!({"prompt_tokens": 1}))).total_tokens, 1);
        assert_eq!(
            usage(Some(&json!({
                "prompt_tokens": 2,
                "completion_tokens": 3
            })))
            .total_tokens,
            5
        );
    }
}
