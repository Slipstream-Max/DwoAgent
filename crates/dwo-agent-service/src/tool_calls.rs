use dwo_model_client::ModelReply;
use dwo_tools::ParsedToolCall;
use serde_json::Value;

use crate::events::ActiveToolCall;

pub(crate) struct ParsedToolCalls {
    pub(crate) local: Vec<Value>,
    pub(crate) local_display: Vec<ActiveToolCall>,
    pub(crate) remote: Vec<Value>,
    pub(crate) remote_display: Vec<ActiveToolCall>,
}

impl ParsedToolCalls {
    pub(crate) fn from_reply(response: &ModelReply) -> Self {
        Self {
            local: response.tool_calls.clone(),
            local_display: response.tool_calls.iter().map(active_tool_call).collect(),
            remote: response.remote_tool_calls.clone(),
            remote_display: response
                .remote_tool_calls
                .iter()
                .map(active_tool_call)
                .collect(),
        }
    }

    pub(crate) fn assistant_visible(&self) -> Vec<ActiveToolCall> {
        self.local_display
            .iter()
            .filter(|call| call.tool_name != "todo")
            .chain(self.remote_display.iter())
            .cloned()
            .collect()
    }
}

pub(crate) fn call_identity(call: &Value) -> (String, String) {
    match ParsedToolCall::parse(call.clone()) {
        Ok(call) => (call.id, call.call.name().to_string()),
        Err(error) => (error.id, error.name),
    }
}

fn active_tool_call(raw: &Value) -> ActiveToolCall {
    match ParsedToolCall::parse(raw.clone()) {
        Ok(call) => ActiveToolCall {
            tool_call_id: call.id,
            tool_name: call.call.name().to_string(),
            raw_input: Value::Object(call.raw_arguments),
            status: "pending".to_string(),
        },
        Err(error) => ActiveToolCall {
            tool_call_id: error.id,
            tool_name: error.name,
            raw_input: raw.get("arguments").cloned().unwrap_or(Value::Null),
            status: raw
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("in_progress")
                .to_string(),
        },
    }
}
