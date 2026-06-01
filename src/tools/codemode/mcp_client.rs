//! MCP backend for code mode.
//!
//! Uses `rmcp` (the official Rust MCP SDK) to provide search / call_tool /
//! get_prompt / read_resource operations.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use rmcp::model::{CallToolRequestParam, GetPromptRequestParam, ReadResourceRequestParam};
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::process::Command;
use tokio::sync::Mutex;

/// MCP capability kind (tool / prompt / resource).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum McpKind {
    Tool,
    Prompt,
    Resource,
}

impl McpKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Prompt => "prompt",
            Self::Resource => "resource",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "tool" => Ok(Self::Tool),
            "prompt" => Ok(Self::Prompt),
            "resource" => Ok(Self::Resource),
            other => bail!("Unsupported MCP capability kind: {other}"),
        }
    }
}

/// One entry in the capability catalog. Mirrors the Python Pydantic classes
/// (`McpToolCatalogItem`, `McpPromptCatalogItem`, `McpResourceCatalogItem`)
/// collapsed into a tagged enum.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpCatalogItem {
    Tool {
        server: String,
        kind: String,
        name: String,
        description: String,
        signature: Value,
    },
    Prompt {
        server: String,
        kind: String,
        name: String,
        description: String,
        signature: Value,
    },
    Resource {
        server: String,
        kind: String,
        name: String,
        description: String,
        uri: String,
        mime_type: Option<String>,
    },
}

impl McpCatalogItem {
    pub fn server(&self) -> &str {
        match self {
            Self::Tool { server, .. }
            | Self::Prompt { server, .. }
            | Self::Resource { server, .. } => server,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Tool { name, .. } | Self::Prompt { name, .. } | Self::Resource { name, .. } => {
                name
            }
        }
    }

    fn as_search_text(&self) -> String {
        let value = serde_json::to_value(self).unwrap_or(Value::Null);
        let Value::Object(map) = value else {
            return String::new();
        };
        map.values()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }
}

/// Config file shape — a thin wrapper around `{ "mcpServers": {...} }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfigModel {
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: BTreeMap<String, Value>,
    #[serde(skip)]
    pub base_dir: Option<PathBuf>,
}

/// Own the MCP client connections and a startup-loaded capability catalog.
pub struct McpClient {
    clients: Mutex<Vec<(String, RunningService<RoleClient, ()>)>>,
    server_names: Vec<String>,
    catalog: HashMap<McpKind, Vec<McpCatalogItem>>,
}

impl McpClient {
    /// Build the client from the parsed config shape. Must be called on the
    /// Tokio runtime that will later own the MCP child processes.
    pub async fn new(config: McpConfigModel) -> Result<Arc<Self>> {
        let mut server_names: Vec<String> = Vec::new();
        let mut catalog: HashMap<McpKind, Vec<McpCatalogItem>> = HashMap::new();
        catalog.insert(McpKind::Tool, Vec::new());
        catalog.insert(McpKind::Prompt, Vec::new());
        catalog.insert(McpKind::Resource, Vec::new());

        // Sort case-insensitively to match Python's `sorted(..., key=lambda ...: k.lower())`.
        let mut entries: Vec<(String, Value)> = config.mcp_servers.into_iter().collect();
        entries.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));

        let mut clients: Vec<(String, RunningService<RoleClient, ()>)> = Vec::new();
        for (name, server_config) in entries {
            let service =
                match connect_stdio_server(&server_config, config.base_dir.as_deref()).await {
                    Ok(service) => service,
                    Err(err) => {
                        // Roll back: cancel everything we already opened.
                        for (_, svc) in clients.drain(..).rev() {
                            let _ = svc.cancel().await;
                        }
                        return Err(err.context(format!("connect MCP server `{name}`")));
                    }
                };

            load_tools(&name, &service, catalog.get_mut(&McpKind::Tool).unwrap()).await?;
            load_prompts(&name, &service, catalog.get_mut(&McpKind::Prompt).unwrap()).await?;
            load_resources(
                &name,
                &service,
                catalog.get_mut(&McpKind::Resource).unwrap(),
            )
            .await?;
            server_names.push(name.clone());
            clients.push((name, service));
        }

        Ok(Arc::new(Self {
            clients: Mutex::new(clients),
            server_names,
            catalog,
        }))
    }

    pub fn server_names(&self) -> &[String] {
        &self.server_names
    }

    pub async fn shutdown(&self) {
        let mut guard = self.clients.lock().await;
        for (_, service) in guard.drain(..).rev() {
            let _ = service.cancel().await;
        }
    }

    /// Catalog search — same three filters as Python: server, query, limit.
    pub async fn search(
        &self,
        query: &str,
        servername: &str,
        kind: McpKind,
        limit: usize,
    ) -> Result<Vec<Value>> {
        if limit < 1 {
            bail!("limit must be greater than or equal to 1");
        }
        let server_filter = servername.trim();
        if !server_filter.is_empty() && !self.server_names.iter().any(|n| n == server_filter) {
            bail!("Unknown MCP server: {server_filter}");
        }

        let query_text = query.trim().to_lowercase();
        let mut items: Vec<McpCatalogItem> = self.catalog.get(&kind).cloned().unwrap_or_default();
        if !server_filter.is_empty() {
            items.retain(|item| item.server() == server_filter);
        }
        if !query_text.is_empty() {
            items.retain(|item| item.as_search_text().contains(&query_text));
        }
        items.sort_by(|a, b| {
            a.server()
                .to_lowercase()
                .cmp(&b.server().to_lowercase())
                .then_with(|| a.name().to_lowercase().cmp(&b.name().to_lowercase()))
        });
        let values: Vec<Value> = items
            .into_iter()
            .take(limit)
            .map(|item| serde_json::to_value(item).unwrap_or(Value::Null))
            .collect();
        Ok(values)
    }

    pub async fn call_tool(
        &self,
        servername: &str,
        toolname: &str,
        arguments: Map<String, Value>,
    ) -> Result<Value> {
        let guard = self.clients.lock().await;
        let service = require_service(&guard, servername)?;
        let param = CallToolRequestParam {
            name: toolname.to_string().into(),
            arguments: Some(arguments),
        };
        let result = service
            .call_tool(param)
            .await
            .map_err(anyhow::Error::from)?;
        let value = serde_json::to_value(&result).unwrap_or(Value::Null);
        let content = value
            .get("content")
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));
        let structured_content = value
            .get("structuredContent")
            .or_else(|| value.get("structured_content"))
            .cloned()
            .unwrap_or(Value::Null);
        let is_error = value
            .get("isError")
            .or_else(|| value.get("is_error"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(serde_json::json!({
            "server": servername,
            "name": toolname,
            "content": content,
            "structured_content": structured_content,
            "is_error": is_error,
        }))
    }

    pub async fn get_prompt(
        &self,
        servername: &str,
        promptname: &str,
        arguments: Map<String, Value>,
    ) -> Result<Value> {
        let guard = self.clients.lock().await;
        let service = require_service(&guard, servername)?;
        let param = GetPromptRequestParam {
            name: promptname.to_string(),
            arguments: if arguments.is_empty() {
                None
            } else {
                Some(arguments)
            },
        };
        let result = service
            .get_prompt(param)
            .await
            .map_err(anyhow::Error::from)?;
        let value = serde_json::to_value(&result).unwrap_or(Value::Null);
        let messages = value
            .get("messages")
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));
        Ok(serde_json::json!({
            "server": servername,
            "name": promptname,
            "messages": messages,
        }))
    }

    pub async fn read_resource(&self, servername: &str, resourcename: &str) -> Result<Value> {
        let guard = self.clients.lock().await;
        let service = require_service(&guard, servername)?;
        let param = ReadResourceRequestParam {
            uri: resourcename.to_string(),
        };
        let result = service
            .read_resource(param)
            .await
            .map_err(anyhow::Error::from)?;
        let contents = serde_json::to_value(&result).unwrap_or(Value::Null);
        let mime_type = resource_mime_type(&contents);
        Ok(serde_json::json!({
            "server": servername,
            "uri": resourcename,
            "mime_type": mime_type,
            "contents": contents,
        }))
    }
}

fn require_service<'a>(
    clients: &'a [(String, RunningService<RoleClient, ()>)],
    servername: &str,
) -> Result<&'a RunningService<RoleClient, ()>> {
    let name = servername.trim();
    if name.is_empty() {
        bail!("servername is required");
    }
    for (k, svc) in clients {
        if k == name {
            return Ok(svc);
        }
    }
    bail!("Unknown MCP server: {name}")
}

async fn connect_stdio_server(
    config: &Value,
    base_dir: Option<&Path>,
) -> Result<RunningService<RoleClient, ()>> {
    let obj = config
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("MCP server config must be an object"))?;
    let command = obj
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("MCP server config missing `command`"))?;
    let args: Vec<String> = obj
        .get("args")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    let env: HashMap<String, String> = obj
        .get("env")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();

    let cwd = obj
        .get("cwd")
        .and_then(Value::as_str)
        .map(|value| resolve_mcp_path(value, base_dir));
    let program = match cwd.as_deref() {
        Some(cwd) => resolve_program_path(command, cwd),
        None => PathBuf::from(command),
    };

    let transport = TokioChildProcess::new(Command::new(program).configure(|cmd| {
        if let Some(cwd) = &cwd {
            cmd.current_dir(cwd);
        }
        cmd.args(&args);
        for (k, v) in &env {
            cmd.env(k, v);
        }
    }))?;
    let service = ().serve(transport).await.map_err(anyhow::Error::from)?;
    Ok(service)
}

fn resolve_mcp_path(value: &str, base_dir: Option<&Path>) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else if let Some(base) = base_dir {
        base.join(path)
    } else {
        path
    }
}

fn resolve_program_path(command: &str, cwd: &Path) -> PathBuf {
    let command_path = PathBuf::from(command);
    if command_path.is_absolute() || !is_path_like_command(command) {
        command_path
    } else {
        cwd.join(command_path)
    }
}

fn is_path_like_command(command: &str) -> bool {
    command.starts_with('.')
        || command.starts_with('~')
        || command.contains('/')
        || command.contains('\\')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_program_path_keeps_path_lookup_commands() {
        let cwd = Path::new("C:/workspace");
        assert_eq!(
            resolve_program_path("mcp-server", cwd),
            PathBuf::from("mcp-server")
        );
    }

    #[test]
    fn resolve_program_path_anchors_relative_paths_to_cwd() {
        let cwd = Path::new("C:/workspace");
        assert_eq!(
            resolve_program_path(".bin/mcp-server.exe", cwd),
            cwd.join(".bin/mcp-server.exe")
        );
    }

    #[tokio::test]
    async fn empty_mcp_config_loads_empty_catalog() {
        let config = McpConfigModel::default();
        let client = McpClient::new(config).await.expect("load empty MCP config");

        assert!(client.server_names().is_empty());
        let tools = client
            .search("", "", McpKind::Tool, 20)
            .await
            .expect("search empty tool catalog");
        assert!(tools.is_empty());

        client.shutdown().await;
    }
}

async fn load_tools(
    servername: &str,
    service: &RunningService<RoleClient, ()>,
    bucket: &mut Vec<McpCatalogItem>,
) -> Result<()> {
    let listing = service
        .list_all_tools()
        .await
        .map_err(anyhow::Error::from)?;
    for tool in listing {
        let name = tool.name.to_string();
        let description = tool.description.as_deref().unwrap_or("").to_string();
        let signature = serde_json::to_value(&tool.input_schema).unwrap_or_else(
            |_| serde_json::json!({ "type": "object", "properties": {}, "required": [] }),
        );
        bucket.push(McpCatalogItem::Tool {
            server: servername.to_string(),
            kind: "tool".to_string(),
            name,
            description,
            signature,
        });
    }
    Ok(())
}

async fn load_prompts(
    servername: &str,
    service: &RunningService<RoleClient, ()>,
    bucket: &mut Vec<McpCatalogItem>,
) -> Result<()> {
    let listing = service
        .list_all_prompts()
        .await
        .map_err(anyhow::Error::from)?;
    for prompt in listing {
        let name = prompt.name.clone();
        let description = prompt.description.clone().unwrap_or_default();
        let mut args: Vec<Value> = Vec::new();
        if let Some(items) = prompt.arguments {
            for arg in items {
                args.push(serde_json::json!({
                    "name": arg.name,
                    "description": arg.description.unwrap_or_default(),
                    "required": arg.required.unwrap_or(false),
                }));
            }
        }
        bucket.push(McpCatalogItem::Prompt {
            server: servername.to_string(),
            kind: "prompt".to_string(),
            name,
            description,
            signature: serde_json::json!({ "arguments": args }),
        });
    }
    Ok(())
}

async fn load_resources(
    servername: &str,
    service: &RunningService<RoleClient, ()>,
    bucket: &mut Vec<McpCatalogItem>,
) -> Result<()> {
    let listing = service
        .list_all_resources()
        .await
        .map_err(anyhow::Error::from)?;
    for resource in listing {
        let uri = resource.raw.uri.clone();
        let description = resource.raw.description.clone().unwrap_or_default();
        let mime_type = resource.raw.mime_type.clone();
        bucket.push(McpCatalogItem::Resource {
            server: servername.to_string(),
            kind: "resource".to_string(),
            name: uri.clone(),
            description,
            uri,
            mime_type,
        });
    }
    Ok(())
}

fn resource_mime_type(contents: &Value) -> Option<String> {
    let list = contents.get("contents").and_then(Value::as_array)?;
    let first = list.first()?;
    first
        .get("mimeType")
        .or_else(|| first.get("mime_type"))
        .and_then(Value::as_str)
        .map(str::to_string)
}
