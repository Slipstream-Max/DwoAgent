use std::{collections::HashMap, path::PathBuf, sync::Arc};

use http::{HeaderName, HeaderValue};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ClientInfo, Tool},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::auth::authorized_http_client;
use crate::{
    AuthConfig, Catalog, CatalogServer, CatalogTool, Error, McpConfig, McpServerConfig, Result,
    ServerStatus, StdioConfig, StreamableHttpConfig,
};

const OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthStatus {
    NotRequired,
    Ready,
    Required,
}

#[derive(Debug, Clone)]
pub struct AuthContext<'a> {
    pub server: &'a str,
    pub url: &'a str,
    pub auth: &'a AuthConfig,
}

/// Hook for an OAuth implementation. The value is a complete Authorization header value.
pub trait AuthProvider: Send + Sync {
    fn authorization(&self, context: &AuthContext<'_>) -> Result<Option<String>>;
}

#[derive(Debug, Default)]
pub struct NoAuthProvider;

impl AuthProvider for NoAuthProvider {
    fn authorization(&self, _context: &AuthContext<'_>) -> Result<Option<String>> {
        Ok(None)
    }
}

pub struct McpClient {
    auth_provider: Arc<dyn AuthProvider>,
    oauth_root: Option<PathBuf>,
}

impl Default for McpClient {
    fn default() -> Self {
        Self {
            auth_provider: Arc::new(NoAuthProvider),
            oauth_root: None,
        }
    }
}

impl McpClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_auth_provider(provider: Arc<dyn AuthProvider>) -> Self {
        Self {
            auth_provider: provider,
            oauth_root: None,
        }
    }

    pub fn with_file_oauth(provider: Arc<dyn AuthProvider>, root: PathBuf) -> Self {
        Self {
            auth_provider: provider,
            oauth_root: Some(root),
        }
    }

    pub fn auth_status(&self, server: &str, config: &McpServerConfig) -> Result<AuthStatus> {
        let McpServerConfig::StreamableHttp(http) = config else {
            return Ok(AuthStatus::NotRequired);
        };
        let Some(auth) = &http.auth else {
            return Ok(AuthStatus::NotRequired);
        };
        Ok(
            if self
                .auth_provider
                .authorization(&AuthContext {
                    server,
                    url: &http.url,
                    auth,
                })?
                .is_some()
            {
                AuthStatus::Ready
            } else {
                AuthStatus::Required
            },
        )
    }

    pub async fn discover(&self, config: &McpConfig) -> Catalog {
        let mut servers = Vec::with_capacity(config.servers.len());
        for (name, server_config) in &config.servers {
            let description = server_config.description().map(str::to_owned);
            let server = match self.auth_status(name, server_config) {
                Ok(AuthStatus::Required) => CatalogServer {
                    name: name.clone(),
                    description,
                    status: ServerStatus::AuthRequired,
                    tool_count: 0,
                    error: Some("authorization required".into()),
                    tools: vec![],
                },
                Err(error) => unavailable(name, description, error.to_string()),
                _ => match tokio::time::timeout(
                    OPERATION_TIMEOUT,
                    self.discover_server(name, server_config),
                )
                .await
                .map_err(|_| Error::Operation {
                    server: name.clone(),
                    message: "operation timed out".to_string(),
                })
                .and_then(|result| result)
                {
                    Ok(tools) => CatalogServer {
                        name: name.clone(),
                        description,
                        status: ServerStatus::Ready,
                        tool_count: tools.len(),
                        error: None,
                        tools: tools.into_iter().map(catalog_tool).collect(),
                    },
                    Err(error) => unavailable(name, description, error.to_string()),
                },
            };
            servers.push(server);
        }
        Catalog {
            config_fingerprint: config.fingerprint.clone(),
            servers,
        }
    }

    pub async fn call(
        &self,
        config: &McpConfig,
        selector: &str,
        arguments: Value,
    ) -> Result<CallResult> {
        let (server_name, tool_name) = parse_tool_selector(selector)?;
        let server = config
            .servers
            .get(server_name)
            .ok_or_else(|| Error::UnknownServer(server_name.into()))?;
        if self.auth_status(server_name, server)? == AuthStatus::Required {
            return Err(Error::AuthRequired {
                server: server_name.into(),
            });
        }
        let arguments = arguments
            .as_object()
            .cloned()
            .ok_or_else(|| Error::InvalidConfig("tool arguments must be a JSON object".into()))?;
        tokio::time::timeout(
            OPERATION_TIMEOUT,
            self.call_server(server_name, server, tool_name, arguments),
        )
        .await
        .map_err(|_| Error::Operation {
            server: server_name.to_string(),
            message: "operation timed out".to_string(),
        })?
    }

    async fn discover_server(&self, name: &str, config: &McpServerConfig) -> Result<Vec<Tool>> {
        match config {
            McpServerConfig::Stdio(stdio) => discover_stdio(name, stdio).await,
            McpServerConfig::StreamableHttp(http) => {
                if http.auth.is_some()
                    && let Some(root) = &self.oauth_root
                {
                    return discover_oauth_http(name, http, root).await;
                }
                let client = ClientInfo::default()
                    .serve(self.http_transport(name, http)?)
                    .await
                    .map_err(|e| operation(name, e))?;
                let result = client
                    .list_all_tools()
                    .await
                    .map_err(|e| operation(name, e));
                let _ = client.cancel().await;
                result
            }
        }
    }

    async fn call_server(
        &self,
        name: &str,
        config: &McpServerConfig,
        tool: &str,
        arguments: Map<String, Value>,
    ) -> Result<CallResult> {
        match config {
            McpServerConfig::Stdio(stdio) => call_stdio(name, stdio, tool, arguments).await,
            McpServerConfig::StreamableHttp(http) => {
                if http.auth.is_some()
                    && let Some(root) = &self.oauth_root
                {
                    return call_oauth_http(name, http, root, tool, arguments).await;
                }
                let client = ClientInfo::default()
                    .serve(self.http_transport(name, http)?)
                    .await
                    .map_err(|e| operation(name, e))?;
                let result = client
                    .call_tool(
                        CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments),
                    )
                    .await
                    .map(CallResult::from)
                    .map_err(|e| operation(name, e));
                let _ = client.cancel().await;
                result
            }
        }
    }

    fn http_transport(
        &self,
        name: &str,
        config: &StreamableHttpConfig,
    ) -> Result<StreamableHttpClientTransport<reqwest::Client>> {
        let mut headers = HashMap::new();
        for (key, value) in &config.headers {
            headers.insert(
                HeaderName::try_from(key).map_err(|e| operation(name, e))?,
                HeaderValue::try_from(value).map_err(|e| operation(name, e))?,
            );
        }
        if let Some(auth) = &config.auth
            && let Some(value) = self.auth_provider.authorization(&AuthContext {
                server: name,
                url: &config.url,
                auth,
            })?
        {
            headers.insert(
                http::header::AUTHORIZATION,
                HeaderValue::try_from(value).map_err(|e| operation(name, e))?,
            );
        }
        Ok(StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(config.url.clone())
                .custom_headers(headers),
        ))
    }
}

async fn discover_oauth_http(
    name: &str,
    config: &StreamableHttpConfig,
    root: &std::path::Path,
) -> Result<Vec<Tool>> {
    let auth_client = authorized_http_client(name, &config.url, root).await?;
    let transport = StreamableHttpClientTransport::with_client(
        auth_client,
        http_transport_config(name, config)?,
    );
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .map_err(|error| operation(name, error))?;
    let result = client
        .list_all_tools()
        .await
        .map_err(|error| operation(name, error));
    let _ = client.cancel().await;
    result
}

async fn call_oauth_http(
    name: &str,
    config: &StreamableHttpConfig,
    root: &std::path::Path,
    tool: &str,
    arguments: Map<String, Value>,
) -> Result<CallResult> {
    let auth_client = authorized_http_client(name, &config.url, root).await?;
    let transport = StreamableHttpClientTransport::with_client(
        auth_client,
        http_transport_config(name, config)?,
    );
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .map_err(|error| operation(name, error))?;
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments))
        .await
        .map(CallResult::from)
        .map_err(|error| operation(name, error));
    let _ = client.cancel().await;
    result
}

fn http_transport_config(
    name: &str,
    config: &StreamableHttpConfig,
) -> Result<StreamableHttpClientTransportConfig> {
    let mut headers = HashMap::new();
    for (key, value) in &config.headers {
        headers.insert(
            HeaderName::try_from(key).map_err(|error| operation(name, error))?,
            HeaderValue::try_from(value).map_err(|error| operation(name, error))?,
        );
    }
    Ok(StreamableHttpClientTransportConfig::with_uri(config.url.clone()).custom_headers(headers))
}

pub async fn discover(config: &McpConfig) -> Catalog {
    McpClient::new().discover(config).await
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CallResult {
    pub content: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

impl From<rmcp::model::CallToolResult> for CallResult {
    fn from(value: rmcp::model::CallToolResult) -> Self {
        Self {
            content: value
                .content
                .into_iter()
                .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
                .collect(),
            structured_content: value.structured_content,
            is_error: value.is_error,
        }
    }
}

async fn discover_stdio(name: &str, config: &StdioConfig) -> Result<Vec<Tool>> {
    let client = ClientInfo::default()
        .serve(stdio_transport(name, config)?)
        .await
        .map_err(|e| operation(name, e))?;
    let result = client
        .list_all_tools()
        .await
        .map_err(|e| operation(name, e));
    let _ = client.cancel().await;
    result
}

async fn call_stdio(
    name: &str,
    config: &StdioConfig,
    tool: &str,
    arguments: Map<String, Value>,
) -> Result<CallResult> {
    let client = ClientInfo::default()
        .serve(stdio_transport(name, config)?)
        .await
        .map_err(|e| operation(name, e))?;
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments))
        .await
        .map(CallResult::from)
        .map_err(|e| operation(name, e));
    let _ = client.cancel().await;
    result
}

fn stdio_transport(name: &str, config: &StdioConfig) -> Result<TokioChildProcess> {
    let mut command =
        rmcp::transport::which_command(&config.command).map_err(|error| operation(name, error))?;
    command.args(&config.args).envs(&config.env);
    if let Some(cwd) = &config.cwd {
        command.current_dir(cwd);
    }
    TokioChildProcess::new(command).map_err(|e| operation(name, e))
}

fn catalog_tool(tool: Tool) -> CatalogTool {
    CatalogTool {
        name: tool.name.into_owned(),
        description: tool.description.map(|v| v.into_owned()),
        input_schema: Value::Object((*tool.input_schema).clone()),
    }
}

fn unavailable(name: &str, description: Option<String>, error: String) -> CatalogServer {
    CatalogServer {
        name: name.into(),
        description,
        status: ServerStatus::Unavailable,
        tool_count: 0,
        error: Some(error),
        tools: vec![],
    }
}

fn operation(server: &str, error: impl std::fmt::Display) -> Error {
    Error::Operation {
        server: server.into(),
        message: error.to_string(),
    }
}

fn parse_tool_selector(selector: &str) -> Result<(&str, &str)> {
    let Some((server, tool)) = selector.split_once('.') else {
        return Err(Error::InvalidSelector(selector.into()));
    };
    if server.is_empty() || tool.is_empty() {
        return Err(Error::InvalidSelector(selector.into()));
    }
    Ok((server, tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_result_preserves_error_flags() {
        let false_result = CallResult::from(rmcp::model::CallToolResult::success(Vec::new()));
        let true_result = CallResult::from(rmcp::model::CallToolResult::error(Vec::new()));
        assert_eq!(false_result.is_error, Some(false));
        assert_eq!(true_result.is_error, Some(true));
        assert_eq!(
            serde_json::to_value(false_result).unwrap()["isError"],
            false
        );
    }

    #[test]
    fn selector_keeps_dots_in_tool_name() {
        assert_eq!(
            parse_tool_selector("server.tool.with.dots").unwrap(),
            ("server", "tool.with.dots")
        );
    }
}
