use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentAudienceRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentAnnotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<Vec<ContentAudienceRole>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddedResourceContents {
    Text {
        uri: String,
        #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        text: String,
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        meta: Option<Map<String, Value>>,
    },
    Blob {
        uri: String,
        #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        blob: String,
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        meta: Option<Map<String, Value>>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        meta: Option<Map<String, Value>>,
    },
    Image {
        #[serde(rename = "mimeType")]
        mime_type: String,
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        meta: Option<Map<String, Value>>,
    },
    Audio {
        #[serde(rename = "mimeType")]
        mime_type: String,
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        meta: Option<Map<String, Value>>,
    },
    Resource {
        resource: EmbeddedResourceContents,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        meta: Option<Map<String, Value>>,
    },
    ResourceLink {
        uri: String,
        name: String,
        #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        meta: Option<Map<String, Value>>,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            annotations: None,
            meta: None,
        }
    }

    pub fn image(mime_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Image {
            mime_type: mime_type.into(),
            data: data.into(),
            uri: None,
            annotations: None,
            meta: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageContent(Vec<ContentBlock>);

impl MessageContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self(vec![ContentBlock::text(text)])
    }

    pub fn blocks(blocks: Vec<ContentBlock>) -> Self {
        Self(blocks)
    }

    pub fn as_blocks(&self) -> &[ContentBlock] {
        &self.0
    }

    pub fn into_blocks(self) -> Vec<ContentBlock> {
        self.0
    }

    pub fn as_text(&self) -> Option<&str> {
        match self.0.as_slice() {
            [ContentBlock::Text { text, .. }] => Some(text),
            _ => None,
        }
    }

    pub fn text_bytes(&self) -> usize {
        self.0
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text, .. } => text.len(),
                _ => 0,
            })
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
            || self.0.iter().all(|block| match block {
                ContentBlock::Text { text, .. } => text.is_empty(),
                _ => false,
            })
    }

    pub fn contains_images(&self) -> bool {
        self.0
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }))
    }

    pub fn len(&self) -> usize {
        self.text_bytes()
    }

    pub fn contains(&self, pattern: &str) -> bool {
        self.0.iter().any(|block| match block {
            ContentBlock::Text { text, .. } => text.contains(pattern),
            _ => false,
        })
    }

    pub fn starts_with(&self, pattern: &str) -> bool {
        self.as_text().is_some_and(|text| text.starts_with(pattern))
    }

    pub fn ends_with(&self, pattern: &str) -> bool {
        self.as_text().is_some_and(|text| text.ends_with(pattern))
    }
}

impl From<String> for MessageContent {
    fn from(value: String) -> Self {
        Self::text(value)
    }
}

impl From<&str> for MessageContent {
    fn from(value: &str) -> Self {
        Self::text(value)
    }
}

impl std::fmt::Display for MessageContent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(text) = self.as_text() {
            formatter.write_str(text)
        } else {
            formatter.write_str(&serde_json::to_string(&self.0).unwrap_or_default())
        }
    }
}

impl PartialEq<&str> for MessageContent {
    fn eq(&self, other: &&str) -> bool {
        self.as_text() == Some(*other)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(String);

impl TurnId {
    pub fn new() -> Self {
        Self(format!("turn-{}", Uuid::new_v4()))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.is_empty()
            || !value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
        {
            return Err("invalid TurnId".to_string());
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TurnId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TurnId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    #[default]
    Conversation,
    CompactionSummary,
    EnvWatcher,
    Permission,
    Config,
    Runtime,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextMessage {
    pub role: MessageRole,
    pub content: MessageContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "is_conversation")]
    pub kind: MessageKind,
}

fn is_conversation(kind: &MessageKind) -> bool {
    *kind == MessageKind::Conversation
}

impl ContextMessage {
    pub fn system(content: impl Into<MessageContent>) -> Self {
        Self::plain(MessageRole::System, content)
    }

    pub fn user(content: impl Into<MessageContent>) -> Self {
        Self::plain(MessageRole::User, content)
    }

    pub fn assistant(content: impl Into<MessageContent>, tool_calls: Vec<Value>) -> Self {
        Self::assistant_with_reasoning(content, None, tool_calls)
    }

    pub fn assistant_with_reasoning(
        content: impl Into<MessageContent>,
        reasoning: Option<String>,
        tool_calls: Vec<Value>,
    ) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            reasoning,
            tool_calls,
            tool_call_id: None,
            tool_name: None,
            kind: MessageKind::Conversation,
        }
    }

    pub fn tool(result: &ToolResultRecord) -> Self {
        Self {
            role: MessageRole::Tool,
            content: result.output.to_string().into(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: Some(result.tool_call_id.clone()),
            tool_name: Some(result.tool_name.clone()),
            kind: MessageKind::Conversation,
        }
    }

    pub fn internal(kind: MessageKind, content: impl Into<MessageContent>) -> Self {
        debug_assert!(kind != MessageKind::Conversation);
        Self {
            kind,
            ..Self::plain(MessageRole::User, content)
        }
    }

    pub fn summary(content: impl Into<MessageContent>) -> Self {
        Self {
            kind: MessageKind::CompactionSummary,
            ..Self::plain(MessageRole::Assistant, content)
        }
    }

    fn plain(role: MessageRole, content: impl Into<MessageContent>) -> Self {
        Self {
            role,
            content: content.into(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            kind: MessageKind::Conversation,
        }
    }

    pub fn is_real_user(&self) -> bool {
        self.role == MessageRole::User && self.kind == MessageKind::Conversation
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultRecord {
    pub tool_call_id: String,
    pub tool_name: String,
    pub output: Value,
}
