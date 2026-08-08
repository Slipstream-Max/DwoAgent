use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub config_fingerprint: String,
    pub servers: Vec<CatalogServer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogServer {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub status: ServerStatus,
    pub tool_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub tools: Vec<CatalogTool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServerStatus {
    Starting,
    Ready,
    AuthRequired,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolRef {
    pub server: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SearchGroup {
    pub server: String,
    pub status: ServerStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub tools: Vec<SearchTool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SearchTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
    pub show_schema: bool,
}

impl Catalog {
    pub fn list_all_tools(&self) -> Vec<ToolRef> {
        self.servers
            .iter()
            .flat_map(|server| {
                server.tools.iter().map(move |tool| ToolRef {
                    server: server.name.clone(),
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                })
            })
            .collect()
    }

    /// Search is case-insensitive and ANDs whitespace-separated terms. Results remain grouped by
    /// server; a server-name/description match includes all its tools, otherwise only matching tools.
    pub fn search(&self, query: &str) -> Vec<SearchGroup> {
        let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        self.servers
            .iter()
            .filter_map(|server| {
                let server_text = format!(
                    "{} {}",
                    server.name,
                    server.description.as_deref().unwrap_or("")
                )
                .to_lowercase();
                let server_match = terms.is_empty() || matches_terms(&terms, &server_text);
                let tools = server
                    .tools
                    .iter()
                    .filter_map(|tool| {
                        let tool_text = format!(
                            "{} {}",
                            tool.name,
                            tool.description.as_deref().unwrap_or("")
                        )
                        .to_lowercase();
                        let tool_match = !terms.is_empty() && matches_terms(&terms, &tool_text);
                        let combined_match =
                            matches_terms(&terms, &format!("{server_text} {tool_text}"));
                        (server_match || combined_match).then(|| SearchTool {
                            name: tool.name.clone(),
                            description: tool.description.clone(),
                            input_schema: tool.input_schema.clone(),
                            show_schema: tool_match || !server_match,
                        })
                    })
                    .collect::<Vec<_>>();
                (server_match || !tools.is_empty()).then(|| SearchGroup {
                    server: server.name.clone(),
                    status: server.status,
                    error: server.error.clone(),
                    tools,
                })
            })
            .collect()
    }
}

fn matches_terms(terms: &[String], text: &str) -> bool {
    terms.iter().all(|term| text.contains(term))
}

#[cfg(test)]
pub(crate) fn mock_catalog() -> Catalog {
    Catalog {
        config_fingerprint: "abc123".into(),
        servers: vec![
            CatalogServer {
                name: "files".into(),
                description: Some("Local documents".into()),
                status: ServerStatus::Ready,
                tool_count: 2,
                error: None,
                tools: vec![
                    CatalogTool {
                        name: "read".into(),
                        description: Some("Read a file".into()),
                        input_schema: serde_json::json!({"type":"object"}),
                    },
                    CatalogTool {
                        name: "search".into(),
                        description: Some("Search documents".into()),
                        input_schema: serde_json::json!({"type":"object"}),
                    },
                ],
            },
            CatalogServer {
                name: "github".into(),
                description: None,
                status: ServerStatus::AuthRequired,
                tool_count: 0,
                error: Some("authorization required".into()),
                tools: vec![],
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_groups_match_server_and_tool_at_different_depths() {
        let catalog = mock_catalog();
        let result = catalog.search("files read");
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0]
                .tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["read"]
        );
        assert!(result[0].tools[0].show_schema);
        let server_match = catalog.search("local documents");
        assert_eq!(server_match[0].tools.len(), 2);
        assert!(server_match[0].tools.iter().all(|tool| !tool.show_schema));
        let both_match = catalog.search("documents");
        assert_eq!(both_match[0].tools.len(), 2);
        assert_eq!(
            both_match[0]
                .tools
                .iter()
                .filter(|tool| tool.show_schema)
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["search"]
        );
        assert!(catalog.search("read missing").is_empty());
    }

    #[test]
    fn json_shape_is_camel_case_and_secret_free() {
        let value = serde_json::to_value(mock_catalog()).unwrap();
        assert_eq!(value["servers"][0]["toolCount"], 2);
        assert!(value["servers"][0]["tools"][0].get("inputSchema").is_some());
        assert_eq!(value["servers"][1]["status"], "auth_required");
        assert!(value.get("configFingerprint").is_some());
    }
}
