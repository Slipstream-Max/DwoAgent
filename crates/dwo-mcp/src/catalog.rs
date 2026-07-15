use std::{fs, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result, render_list};

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
    Ready,
    AuthRequired,
    Unavailable,
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
    pub tools: Vec<CatalogTool>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ShowResult<'a> {
    Server(&'a CatalogServer),
    Tool {
        server: &'a CatalogServer,
        tool: &'a CatalogTool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogCache {
    #[serde(flatten)]
    pub catalog: Catalog,
    pub summary: String,
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
        if terms.is_empty() {
            return self
                .servers
                .iter()
                .map(|s| SearchGroup {
                    server: s.name.clone(),
                    status: s.status,
                    tools: s.tools.clone(),
                })
                .collect();
        }
        self.servers
            .iter()
            .filter_map(|server| {
                let server_text = format!(
                    "{} {}",
                    server.name,
                    server.description.as_deref().unwrap_or("")
                )
                .to_lowercase();
                let server_match = terms.iter().all(|term| server_text.contains(term));
                let tools = if server_match {
                    server.tools.clone()
                } else {
                    server
                        .tools
                        .iter()
                        .filter(|tool| {
                            let text = format!(
                                "{} {} {}",
                                server.name,
                                tool.name,
                                tool.description.as_deref().unwrap_or("")
                            )
                            .to_lowercase();
                            terms.iter().all(|term| text.contains(term))
                        })
                        .cloned()
                        .collect()
                };
                (!tools.is_empty()).then(|| SearchGroup {
                    server: server.name.clone(),
                    status: server.status,
                    tools,
                })
            })
            .collect()
    }

    pub fn show(&self, selector: &str) -> Result<ShowResult<'_>> {
        if let Some((server_name, tool_name)) = selector.split_once('.') {
            if server_name.is_empty() || tool_name.is_empty() {
                return Err(Error::InvalidSelector(selector.into()));
            }
            let server = self
                .servers
                .iter()
                .find(|s| s.name == server_name)
                .ok_or_else(|| Error::UnknownServer(server_name.into()))?;
            let tool = server
                .tools
                .iter()
                .find(|t| t.name == tool_name)
                .ok_or_else(|| Error::InvalidSelector(selector.into()))?;
            Ok(ShowResult::Tool { server, tool })
        } else {
            self.servers
                .iter()
                .find(|s| s.name == selector)
                .map(ShowResult::Server)
                .ok_or_else(|| Error::UnknownServer(selector.into()))
        }
    }
}

impl CatalogCache {
    pub fn new(catalog: Catalog) -> Self {
        let summary = render_list(&catalog);
        Self { catalog, summary }
    }
}

pub fn write_catalog(path: impl AsRef<Path>, catalog: &Catalog) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(catalog)?)?;
    Ok(())
}

pub fn read_catalog(path: impl AsRef<Path>) -> Result<Catalog> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

pub fn write_catalog_cache(path: impl AsRef<Path>, catalog: &Catalog) -> Result<()> {
    fs::write(
        path,
        serde_json::to_vec_pretty(&CatalogCache::new(catalog.clone()))?,
    )?;
    Ok(())
}

pub fn read_catalog_cache(path: impl AsRef<Path>) -> Result<CatalogCache> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
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
    fn search_groups_matches_and_ands_terms() {
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
        assert_eq!(catalog.search("local documents")[0].tools.len(), 2);
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

    #[test]
    fn cache_round_trips_with_summary() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let catalog = mock_catalog();
        write_catalog_cache(file.path(), &catalog).unwrap();
        let cache = read_catalog_cache(file.path()).unwrap();
        assert_eq!(cache.catalog, catalog);
        assert_eq!(cache.summary, render_list(&catalog));
    }
}
