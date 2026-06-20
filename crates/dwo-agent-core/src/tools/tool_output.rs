use serde_json::{Map, Value};

use super::tool_catalog::tool_kind;

pub(crate) struct ToolOutput {
    fields: Map<String, Value>,
}

impl ToolOutput {
    pub(crate) fn new(tool: &str, kind: &str, status: &str) -> Self {
        let mut fields = Map::new();
        fields.insert("tool".to_string(), Value::String(tool.to_string()));
        fields.insert("kind".to_string(), Value::String(kind.to_string()));
        fields.insert("status".to_string(), Value::String(status.to_string()));
        Self { fields }
    }

    pub(crate) fn completed(tool: &str, kind: &str) -> Self {
        Self::new(tool, kind, "completed")
    }

    pub(crate) fn error(tool: &str, message: impl Into<String>) -> Value {
        Self::error_with_kind(tool, tool_kind(tool), message)
    }

    pub(crate) fn error_with_kind(tool: &str, kind: &str, message: impl Into<String>) -> Value {
        Self::new(tool, kind, "error")
            .field("error", Value::String(message.into()))
            .into_value()
    }

    pub(crate) fn cancelled(tool: &str, message: impl Into<String>) -> Value {
        Self::new(tool, tool_kind(tool), "cancelled")
            .field("error", Value::String(message.into()))
            .into_value()
    }

    pub(crate) fn field(mut self, key: &str, value: Value) -> Self {
        self.fields.insert(key.to_string(), value);
        self
    }

    pub(crate) fn into_value(self) -> Value {
        Value::Object(self.fields)
    }
}
