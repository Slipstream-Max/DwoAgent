//! Tool schema assembly.

use serde_json::Value;

use crate::config::models::AgentTools;
use crate::templates;

pub fn tool_schemas(tools: &AgentTools) -> Vec<Value> {
    let mut sources: Vec<&str> = Vec::new();
    if tools.file_edit_enabled() {
        sources.push(templates::files::TOOL_SCHEMA);
    }
    if tools.terminal_enabled() {
        sources.push(templates::terminal::TOOL_SCHEMA);
    }
    if tools.subagent_enabled() {
        sources.push(templates::subagent::TOOL_SCHEMA);
    }
    if tools.terminal_enabled() || tools.subagent_enabled() {
        sources.push(templates::wait::TOOL_SCHEMA);
    }
    tool_schemas_from_templates(&sources)
}

pub fn tool_schemas_from_templates(sources: &[&str]) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();
    for raw in sources {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(Value::Array(items)) => merged.extend(items),
            Ok(_) => tracing::warn!(target: "tools", "tool schema must be a list"),
            Err(err) => tracing::warn!(target: "tools", error = %err, "invalid tool schema JSON"),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::ToolSwitch;

    fn schema_names(schemas: Vec<Value>) -> Vec<String> {
        schemas
            .iter()
            .filter_map(|schema| {
                schema
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    }

    #[test]
    fn tool_schemas_follow_agent_tools() {
        let tools = AgentTools {
            file_edit: ToolSwitch::Disable,
            terminal: ToolSwitch::Enable,
            subagent: ToolSwitch::Disable,
        };

        let names = schema_names(tool_schemas(&tools));

        assert!(names.contains(&"terminal_exec".to_string()));
        assert!(!names.contains(&"file_edit".to_string()));
        assert!(!names.contains(&"spawn_subagent".to_string()));
    }

    #[test]
    fn channel_tool_schema_templates_are_parseable() {
        let names = schema_names(tool_schemas_from_templates(&[
            templates::channel::feishu::MEDIA_TOOL_SCHEMA,
            templates::channel::feishu::CARD_TOOL_SCHEMA,
            templates::channel::weixin::TOOL_SCHEMA,
        ]));

        assert_eq!(
            names,
            vec![
                "feishu_reply_media".to_string(),
                "feishu_reply_card".to_string(),
                "weixin_reply_media".to_string()
            ]
        );
    }
}
