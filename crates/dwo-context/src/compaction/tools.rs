use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::{ContextMessage, MessageRole};

use super::OMITTED_MARKER;

const FILE_PATCH_OMITTED_MARKER: &str = "file patch omitted";

pub(super) fn compact_tool_exchanges(messages: &[ContextMessage]) -> Vec<ContextMessage> {
    let mut output = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        if message.role == MessageRole::Assistant && !message.tool_calls.is_empty() {
            let mut end = index + 1;
            while end < messages.len() && messages[end].role == MessageRole::Tool {
                end += 1;
            }
            push_compacted_tool_exchange(message, &messages[index + 1..end], &mut output);
            index = end;
            continue;
        }
        if message.role != MessageRole::Tool {
            output.push(message.clone());
        }
        index += 1;
    }
    output
}

fn push_compacted_tool_exchange(
    assistant: &ContextMessage,
    results: &[ContextMessage],
    output: &mut Vec<ContextMessage>,
) {
    let result_by_id = results
        .iter()
        .filter_map(|result| result.tool_call_id.as_deref().map(|id| (id, result)))
        .collect::<HashMap<_, _>>();
    let mut paired_ids = HashSet::new();
    let tool_calls = assistant
        .tool_calls
        .iter()
        .filter(|call| {
            tool_call_id(call).is_some_and(|id| {
                let paired = result_by_id.contains_key(id);
                if paired {
                    paired_ids.insert(id.to_string());
                }
                paired
            })
        })
        .map(compact_tool_call)
        .collect::<Vec<_>>();
    let mut normalized = assistant.clone();
    normalized.tool_calls = tool_calls;

    if !normalized.content.is_empty()
        || normalized.reasoning.is_some()
        || !normalized.tool_calls.is_empty()
    {
        output.push(normalized);
    }
    output.extend(
        results
            .iter()
            .filter(|result| {
                result
                    .tool_call_id
                    .as_ref()
                    .is_some_and(|id| paired_ids.contains(id))
            })
            .map(compact_tool_result),
    );
}

fn compact_tool_result(result: &ContextMessage) -> ContextMessage {
    let mut compacted = result.clone();
    if result.tool_name.as_deref() != Some("terminal") {
        return compacted;
    }
    let Some(content) = result.content.as_text() else {
        return compacted;
    };
    let Ok(mut value) = serde_json::from_str::<Value>(content) else {
        return compacted;
    };
    let Some(output) = value.get_mut("output") else {
        return compacted;
    };
    *output = Value::String(OMITTED_MARKER.trim().to_string());
    compacted.content = value.to_string().into();
    compacted
}

fn tool_call_id(call: &Value) -> Option<&str> {
    call.get("id")
        .and_then(Value::as_str)
        .or_else(|| call.get("tool_call_id").and_then(Value::as_str))
}

fn compact_tool_call(call: &Value) -> Value {
    let mut call = call.clone();
    omit_file_patch(&mut call);
    call
}

fn omit_file_patch(call: &mut Value) -> bool {
    let is_file_edit = call
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| name == "file_edit");
    if !is_file_edit {
        return false;
    }
    call.get_mut("arguments")
        .and_then(|arguments| arguments.get_mut("patch"))
        .is_some_and(replace_with_omission)
}

fn replace_with_omission(value: &mut Value) -> bool {
    let bytes = match &*value {
        Value::String(text) => text.len(),
        value => serde_json::to_vec(value).map_or(0, |encoded| encoded.len()),
    };
    *value = Value::String(format!(
        "<{FILE_PATCH_OMITTED_MARKER}: {bytes} UTF-8 bytes>"
    ));
    true
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn history_compaction_preserves_tool_envelope_and_omits_file_patch() {
        let compacted = compact_tool_call(&json!({
            "id":"edit-1",
            "name":"file_edit",
            "arguments":{"patch":"*** Begin Patch\nlarge body\n*** End Patch"}
        }));

        assert_eq!(compacted["id"], "edit-1");
        assert_eq!(compacted["name"], "file_edit");
        assert!(
            compacted["arguments"]["patch"]
                .as_str()
                .unwrap()
                .contains("file patch omitted")
        );
    }

    #[test]
    fn history_compaction_keeps_terminal_command_unchanged() {
        let command = "echo terminal-command ".repeat(300);
        let call = json!({
            "id":"terminal-1",
            "name":"terminal",
            "arguments":{"action":"run", "command":command}
        });

        assert_eq!(compact_tool_call(&call), call);
    }
}
