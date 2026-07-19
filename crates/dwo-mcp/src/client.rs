use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};

use http::{HeaderName, HeaderValue};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, ClientInfo, Tool},
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::auth::authorized_http_client;
use crate::{AuthConfig, Error, McpServerConfig, Result, StdioConfig, StreamableHttpConfig};

const OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub(crate) type ConnectedClient = RunningService<RoleClient, ClientInfo>;

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

    /// Connects and initializes one MCP server. The returned service owns its transport and must
    /// be retained by the host for the server session to remain alive.
    pub(crate) async fn connect(
        &self,
        name: &str,
        config: &McpServerConfig,
    ) -> Result<ConnectedClient> {
        if self.auth_status(name, config)? == AuthStatus::Required {
            return Err(Error::AuthRequired {
                server: name.to_string(),
            });
        }
        tokio::time::timeout(OPERATION_TIMEOUT, self.connect_server(name, config))
            .await
            .map_err(|_| timeout_error(name))?
    }

    pub(crate) async fn list_tools(
        &self,
        name: &str,
        client: &ConnectedClient,
    ) -> Result<Vec<Tool>> {
        tokio::time::timeout(OPERATION_TIMEOUT, client.list_all_tools())
            .await
            .map_err(|_| timeout_error(name))?
            .map_err(|error| operation(name, error))
    }

    pub(crate) async fn call(
        &self,
        name: &str,
        client: &ConnectedClient,
        tool: &str,
        arguments: Map<String, Value>,
    ) -> Result<CallResult> {
        tokio::time::timeout(
            OPERATION_TIMEOUT,
            client.call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments)),
        )
        .await
        .map_err(|_| timeout_error(name))?
        .map(CallResult::from)
        .map_err(|error| operation(name, error))
    }

    async fn connect_server(
        &self,
        name: &str,
        config: &McpServerConfig,
    ) -> Result<ConnectedClient> {
        match config {
            McpServerConfig::Stdio(stdio) => ClientInfo::default()
                .serve(stdio_transport(name, stdio)?)
                .await
                .map_err(|error| operation(name, error)),
            McpServerConfig::StreamableHttp(http) => {
                if http.auth.is_some()
                    && let Some(root) = &self.oauth_root
                {
                    let auth_client = authorized_http_client(name, &http.url, root).await?;
                    let transport = StreamableHttpClientTransport::with_client(
                        auth_client,
                        http_transport_config(name, http)?,
                    );
                    return ClientInfo::default()
                        .serve(transport)
                        .await
                        .map_err(|error| operation(name, error));
                }
                ClientInfo::default()
                    .serve(self.http_transport(name, http)?)
                    .await
                    .map_err(|error| operation(name, error))
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
                HeaderName::try_from(key).map_err(|error| operation(name, error))?,
                HeaderValue::try_from(value).map_err(|error| operation(name, error))?,
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
                HeaderValue::try_from(value).map_err(|error| operation(name, error))?,
            );
        }
        Ok(StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(config.url.clone())
                .custom_headers(headers),
        ))
    }
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
                .map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
                .collect(),
            structured_content: value.structured_content,
            is_error: value.is_error,
        }
    }
}

fn stdio_transport(name: &str, config: &StdioConfig) -> Result<TokioChildProcess> {
    let executable =
        resolve_executable(&config.command, &config.process_env, config.cwd.as_deref())
            .map_err(|error| operation(name, error))?;
    let mut command = tokio::process::Command::new(executable);
    command
        .args(&config.args)
        .env_clear()
        .envs(&config.process_env);
    if let Some(cwd) = &config.cwd {
        command.current_dir(cwd);
    }
    TokioChildProcess::new(command).map_err(|error| operation(name, error))
}

fn resolve_executable(
    command: &str,
    environment: &BTreeMap<String, String>,
    cwd: Option<&Path>,
) -> std::io::Result<PathBuf> {
    let path = Path::new(command);
    if path.is_absolute() || command.contains('/') || command.contains('\\') {
        let path = if path.is_relative() {
            cwd.unwrap_or_else(|| Path::new(".")).join(path)
        } else {
            path.to_path_buf()
        };
        return executable_candidate(path, environment).ok_or_else(|| not_found(command));
    }

    let Some(path_value) = environment_value(environment, "PATH") else {
        return Err(not_found(command));
    };
    for directory in std::env::split_paths(path_value) {
        if let Some(candidate) = executable_candidate(directory.join(command), environment) {
            return Ok(candidate);
        }
    }
    Err(not_found(command))
}

fn executable_candidate(path: PathBuf, environment: &BTreeMap<String, String>) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path);
    }
    #[cfg(windows)]
    {
        if path.extension().is_none() {
            let extensions = environment_value(environment, "PATHEXT")
                .unwrap_or(".COM;.EXE;.BAT;.CMD")
                .split(';');
            for extension in extensions {
                let extension = extension.trim().trim_start_matches('.');
                if extension.is_empty() {
                    continue;
                }
                let candidate = path.with_extension(extension);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn environment_value<'a>(environment: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    #[cfg(windows)]
    {
        environment
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.as_str())
    }
    #[cfg(not(windows))]
    {
        environment.get(key).map(String::as_str)
    }
}

fn not_found(command: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("MCP command {command:?} was not found on PATH"),
    )
}

pub(crate) fn catalog_tool(tool: Tool) -> crate::CatalogTool {
    crate::CatalogTool {
        name: tool.name.into_owned(),
        description: tool.description.map(|value| value.into_owned()),
        input_schema: Value::Object((*tool.input_schema).clone()),
    }
}

pub(crate) fn parse_tool_selector(selector: &str) -> Result<(&str, &str)> {
    let Some((server, tool)) = selector.split_once('.') else {
        return Err(Error::InvalidSelector(selector.into()));
    };
    if server.is_empty() || tool.is_empty() {
        return Err(Error::InvalidSelector(selector.into()));
    }
    Ok((server, tool))
}

fn timeout_error(server: &str) -> Error {
    Error::Operation {
        server: server.into(),
        message: "operation timed out".to_string(),
    }
}

fn operation(server: &str, error: impl std::fmt::Display) -> Error {
    Error::Operation {
        server: server.into(),
        message: error.to_string(),
    }
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

    #[cfg(windows)]
    #[test]
    fn resolves_windows_shims_from_the_supplied_path() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("uvx.exe");
        std::fs::write(&executable, b"").unwrap();
        let environment = BTreeMap::from([
            ("PATH".to_string(), directory.path().display().to_string()),
            ("PATHEXT".to_string(), ".EXE".to_string()),
        ]);
        let resolved = resolve_executable("uvx", &environment, None).unwrap();
        assert!(
            resolved
                .to_string_lossy()
                .eq_ignore_ascii_case(&executable.to_string_lossy())
        );
    }
}
