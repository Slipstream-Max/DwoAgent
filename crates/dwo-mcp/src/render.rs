use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::{Catalog, SearchGroup, ServerStatus};

pub fn render_list(catalog: &Catalog) -> String {
    if catalog.servers.is_empty() {
        return "No MCP servers configured.".into();
    }
    catalog
        .servers
        .iter()
        .map(|server| {
            let status = status_label(server.status);
            let tool_count = if server.status == ServerStatus::Ready {
                server.tool_count.to_string()
            } else {
                "?".to_string()
            };
            format!("{}    {} tools    {}", server.name, tool_count, status)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render_search(groups: &[SearchGroup]) -> String {
    if groups.is_empty() {
        return "No matching MCP servers or tools.\n".into();
    }
    let output = groups
        .iter()
        .map(|group| {
            let tools = group
                .tools
                .iter()
                .map(|tool| SearchToolOutput {
                    name: &tool.name,
                    schema: tool.show_schema.then_some(&tool.input_schema),
                })
                .collect();
            (
                group.server.as_str(),
                SearchServerOutput {
                    status: status_label(group.status),
                    error: group.error.as_deref(),
                    tools,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    serde_yaml::to_string(&output).unwrap_or_else(|_| "value: <unrenderable>\n".to_string())
}

#[derive(Serialize)]
struct SearchServerOutput<'a> {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    tools: Vec<SearchToolOutput<'a>>,
}

#[derive(Serialize)]
struct SearchToolOutput<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<&'a Value>,
}

fn status_label(status: ServerStatus) -> &'static str {
    match status {
        ServerStatus::Starting => "starting",
        ServerStatus::Ready => "ready",
        ServerStatus::AuthRequired => "auth_required",
        ServerStatus::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::mock_catalog;

    #[test]
    fn renders_compact_stable_list() {
        assert_eq!(
            render_list(&mock_catalog()),
            "files    2 tools    ready\ngithub    ? tools    auth_required"
        );
    }

    #[test]
    fn search_expands_only_directly_matching_tool_schemas() {
        let catalog = mock_catalog();
        let text = render_search(&catalog.search("documents"));
        assert!(text.contains("name: read"));
        assert!(text.contains("- name: search\n    schema:"));
        assert_eq!(text.matches("schema:").count(), 1);
    }
}
