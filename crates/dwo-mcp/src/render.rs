use crate::{Catalog, SearchGroup, ServerStatus, ShowResult};

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
        return "No matching MCP tools.".into();
    }
    groups
        .iter()
        .map(|group| {
            let tools = group
                .tools
                .iter()
                .map(|tool| match &tool.description {
                    Some(description) => format!("  {} - {}", tool.name, one_line(description)),
                    None => format!("  {}", tool.name),
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "{} [{}]\n{}",
                group.server,
                status_label(group.status),
                tools
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render_show(result: ShowResult<'_>) -> String {
    match result {
        ShowResult::Server(server) => {
            let header = format!("{} [{}]", server.name, status_label(server.status));
            let description = server
                .description
                .as_deref()
                .map(|v| format!("\n{}", one_line(v)))
                .unwrap_or_default();
            let tools = server
                .tools
                .iter()
                .map(|t| format!("\n  {}", t.name))
                .collect::<String>();
            format!("{header}{description}{tools}")
        }
        ShowResult::Tool { server, tool } => {
            let description = tool
                .description
                .as_deref()
                .map(|v| format!("\n{}", one_line(v)))
                .unwrap_or_default();
            let schema =
                serde_json::to_string_pretty(&tool.input_schema).unwrap_or_else(|_| "{}".into());
            format!(
                "{}.{}{description}\n\nArguments:\n{schema}",
                server.name, tool.name
            )
        }
    }
}

fn status_label(status: ServerStatus) -> &'static str {
    match status {
        ServerStatus::Pending => "pending",
        ServerStatus::Ready => "ready",
        ServerStatus::AuthRequired => "auth required",
        ServerStatus::Unavailable => "unavailable",
    }
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::mock_catalog;

    #[test]
    fn renders_compact_stable_list() {
        assert_eq!(
            render_list(&mock_catalog()),
            "files    2 tools    ready\ngithub    ? tools    auth required"
        );
    }

    #[test]
    fn renders_tool_schema() {
        let catalog = mock_catalog();
        let text = render_show(catalog.show("files.read").unwrap());
        assert!(text.starts_with("files.read\nRead a file"));
        assert!(text.contains("Arguments:"));
    }
}
