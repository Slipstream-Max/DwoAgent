//! Agent activity event types and wire payload conversion.

use serde_json::{Map, Value, json};

pub const EVENT_USER_MESSAGE_CHUNK: &str = "user_message_chunk";
pub const EVENT_AGENT_MESSAGE_CHUNK: &str = "agent_message_chunk";
pub const EVENT_AGENT_THOUGHT_CHUNK: &str = "agent_thought_chunk";
pub const EVENT_TOOL_CALL: &str = "tool_call";
pub const EVENT_TOOL_CALL_UPDATE: &str = "tool_call_update";
pub const EVENT_ACTIVITY_BOX: &str = "activity_box";
pub const EVENT_ACTIVITY_BOX_UPDATE: &str = "activity_box_update";
pub const EVENT_CURRENT_MODE: &str = "current_mode_update";
pub const EVENT_CONFIG_OPTION: &str = "config_option_update";
pub const EVENT_SESSION_INFO: &str = "session_info_update";
pub const EVENT_USAGE_UPDATE: &str = "usage_update";

#[derive(Debug, Clone)]
pub struct ToolCallEvent {
    pub tool_call_id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub raw_input: Option<Value>,
    pub raw_output: Option<Value>,
    pub content: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct ToolCallUpdateEvent {
    pub tool_call_id: String,
    pub status: String,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<Value>,
    pub content: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct ActivityBoxEvent {
    pub activity_id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub enum ActivityEvent {
    UserMessageChunk {
        content: Value,
    },
    AgentMessageChunk {
        content: Value,
    },
    AgentThoughtChunk {
        content: Value,
    },
    ToolCall(ToolCallEvent),
    ToolCallUpdate(ToolCallUpdateEvent),
    ActivityBox(ActivityBoxEvent),
    ActivityBoxUpdate(ActivityBoxEvent),
    CurrentModeUpdate {
        mode_id: String,
    },
    ConfigOptionUpdate,
    UsageUpdate {
        used: u64,
        size: u64,
    },
    SessionInfoUpdate {
        title: String,
        updated_at: Option<String>,
    },
}

impl ToolCallEvent {
    pub fn new(tool_call_id: &str, title: &str) -> Self {
        Self {
            tool_call_id: tool_call_id.to_string(),
            title: title.to_string(),
            kind: "other".to_string(),
            status: "pending".to_string(),
            raw_input: None,
            raw_output: None,
            content: None,
        }
    }
}

impl ToolCallUpdateEvent {
    pub fn new(tool_call_id: &str, status: &str) -> Self {
        Self {
            tool_call_id: tool_call_id.to_string(),
            status: status.to_string(),
            title: None,
            kind: None,
            raw_input: None,
            raw_output: None,
            content: None,
        }
    }
}

impl ActivityBoxEvent {
    pub fn new(activity_id: &str, title: &str, status: &str, text: &str) -> Self {
        Self {
            activity_id: activity_id.to_string(),
            title: title.to_string(),
            kind: "think".to_string(),
            status: status.to_string(),
            text: text.to_string(),
        }
    }
}

impl ActivityEvent {
    pub fn user_message_text(text: &str) -> Self {
        Self::UserMessageChunk {
            content: text_content(text),
        }
    }

    pub fn user_message_content(content: &Map<String, Value>) -> Self {
        Self::UserMessageChunk {
            content: Value::Object(content.clone()),
        }
    }

    pub fn agent_message_text(text: &str) -> Self {
        Self::AgentMessageChunk {
            content: text_content(text),
        }
    }

    pub fn agent_thought_text(text: &str) -> Self {
        Self::AgentThoughtChunk {
            content: text_content(text),
        }
    }

    pub fn update_type(&self) -> &'static str {
        match self {
            Self::UserMessageChunk { .. } => EVENT_USER_MESSAGE_CHUNK,
            Self::AgentMessageChunk { .. } => EVENT_AGENT_MESSAGE_CHUNK,
            Self::AgentThoughtChunk { .. } => EVENT_AGENT_THOUGHT_CHUNK,
            Self::ToolCall(_) => EVENT_TOOL_CALL,
            Self::ToolCallUpdate(_) => EVENT_TOOL_CALL_UPDATE,
            Self::ActivityBox(_) => EVENT_ACTIVITY_BOX,
            Self::ActivityBoxUpdate(_) => EVENT_ACTIVITY_BOX_UPDATE,
            Self::CurrentModeUpdate { .. } => EVENT_CURRENT_MODE,
            Self::ConfigOptionUpdate => EVENT_CONFIG_OPTION,
            Self::UsageUpdate { .. } => EVENT_USAGE_UPDATE,
            Self::SessionInfoUpdate { .. } => EVENT_SESSION_INFO,
        }
    }

    pub fn into_update(self) -> Map<String, Value> {
        let update_type = self.update_type();
        let mut payload = Map::new();
        payload.insert(
            "session_update".to_string(),
            Value::String(update_type.to_string()),
        );
        match self {
            Self::UserMessageChunk { content }
            | Self::AgentMessageChunk { content }
            | Self::AgentThoughtChunk { content } => {
                payload.insert("content".to_string(), content);
            }
            Self::ToolCall(event) => {
                payload.insert(
                    "tool_call_id".to_string(),
                    Value::String(event.tool_call_id),
                );
                payload.insert("title".to_string(), Value::String(event.title));
                payload.insert("kind".to_string(), Value::String(event.kind));
                payload.insert("status".to_string(), Value::String(event.status));
                insert_optional(&mut payload, "raw_input", event.raw_input);
                insert_optional(&mut payload, "raw_output", event.raw_output);
                insert_optional(&mut payload, "content", event.content);
            }
            Self::ToolCallUpdate(event) => {
                payload.insert(
                    "tool_call_id".to_string(),
                    Value::String(event.tool_call_id),
                );
                payload.insert("status".to_string(), Value::String(event.status));
                insert_optional_str(&mut payload, "title", event.title);
                insert_optional_str(&mut payload, "kind", event.kind);
                insert_optional(&mut payload, "raw_input", event.raw_input);
                insert_optional(&mut payload, "raw_output", event.raw_output);
                insert_optional(&mut payload, "content", event.content);
            }
            Self::ActivityBox(event) | Self::ActivityBoxUpdate(event) => {
                payload.insert("activity_id".to_string(), Value::String(event.activity_id));
                payload.insert("title".to_string(), Value::String(event.title));
                payload.insert("kind".to_string(), Value::String(event.kind));
                payload.insert("status".to_string(), Value::String(event.status));
                payload.insert("content".to_string(), json!([text_content(&event.text)]));
            }
            Self::CurrentModeUpdate { mode_id } => {
                payload.insert("current_mode_id".to_string(), Value::String(mode_id));
            }
            Self::ConfigOptionUpdate => {}
            Self::UsageUpdate { used, size } => {
                payload.insert("used".to_string(), Value::from(used));
                payload.insert("size".to_string(), Value::from(size));
            }
            Self::SessionInfoUpdate { title, updated_at } => {
                payload.insert("title".to_string(), Value::String(title));
                payload.insert(
                    "updated_at".to_string(),
                    updated_at.map(Value::String).unwrap_or(Value::Null),
                );
            }
        }
        payload
    }
}

pub fn update_type(update: &Map<String, Value>) -> &str {
    update
        .get("session_update")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
}

pub fn text_content(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

pub fn tool_permission_payload(
    tool_call_id: &str,
    title: &str,
    raw_input: &Map<String, Value>,
) -> Map<String, Value> {
    json!({
        "tool_call_id": tool_call_id,
        "title": title,
        "kind": "other",
        "status": "pending",
        "raw_input": raw_input,
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

fn insert_optional(payload: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        payload.insert(key.to_string(), value);
    }
}

fn insert_optional_str(payload: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        payload.insert(key.to_string(), Value::String(value));
    }
}
