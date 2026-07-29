use dwo_context::MessageContent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub output: Value,
    #[serde(skip)]
    pub model_context: Vec<MessageContent>,
}

impl ToolResult {
    pub fn context_record(&self) -> dwo_context::ToolResultRecord {
        dwo_context::ToolResultRecord {
            tool_call_id: self.tool_call_id.clone(),
            tool_name: self.tool_name.clone(),
            output: self.output.clone(),
            model_context: self.model_context.clone(),
        }
    }
}
