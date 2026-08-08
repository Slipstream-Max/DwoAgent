use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use futures::future::join_all;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::client::{ConnectedClient, ConnectedPeer, catalog_tool, parse_tool_selector};
use crate::{
    AuthStatus, CallResult, Catalog, CatalogServer, McpClient, McpConfig, McpServerConfig, Result,
    ServerStatus, oauth_login, oauth_logout,
};

const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub struct McpRuntime {
    config_path: PathBuf,
    oauth_root: PathBuf,
    client: McpClient,
    state: Mutex<RuntimeState>,
}

#[derive(Default)]
struct RuntimeState {
    config: Option<McpConfig>,
    catalog: Option<Catalog>,
    servers: BTreeMap<String, Arc<ManagedServer>>,
}

struct ManagedServer {
    connection: Mutex<Option<ManagedConnection>>,
    next_generation: AtomicU64,
}

struct ManagedConnection {
    service: ConnectedClient,
    peer: ConnectedPeer,
    generation: u64,
}

impl ManagedServer {
    fn new() -> Self {
        Self {
            connection: Mutex::new(None),
            next_generation: AtomicU64::new(1),
        }
    }

    async fn close(&self) {
        let Some(mut connection) = self.connection.lock().await.take() else {
            return;
        };
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, connection.service.close()).await;
    }
}

impl McpRuntime {
    pub fn new(profile_root: impl AsRef<Path>) -> Self {
        let profile_root = profile_root.as_ref();
        let oauth_root = profile_root.join("runtime/mcp/oauth");
        Self {
            config_path: profile_root.join("resource/mcp.json"),
            client: McpClient::with_file_oauth(
                Arc::new(crate::FileOAuthProvider::new(oauth_root.clone())),
                oauth_root.clone(),
            ),
            oauth_root,
            state: Mutex::new(RuntimeState::default()),
        }
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Reads changed configuration and reconciles the managed-server set without starting new
    /// servers. Lifecycle boundaries use `sync_and_start` to obtain a ready catalog.
    pub async fn sync(&self) -> Result<Catalog> {
        let config = self.load_config()?;
        let (catalog, retired) = {
            let mut state = self.state.lock().await;
            if state.config.as_ref().is_some_and(|current| {
                current.fingerprint == config.fingerprint && current.servers == config.servers
            }) {
                return Ok(state
                    .catalog
                    .clone()
                    .expect("MCP runtime state must have a catalog after synchronization"));
            }

            let previous_config = state.config.take();
            let previous_catalog = state.catalog.take();

            let mut retained = BTreeMap::new();
            let mut retired = Vec::new();
            for (name, managed) in std::mem::take(&mut state.servers) {
                let unchanged = previous_config
                    .as_ref()
                    .and_then(|previous| previous.servers.get(&name))
                    .zip(config.servers.get(&name))
                    .is_some_and(|(previous, current)| previous == current);
                if unchanged {
                    retained.insert(name, managed);
                } else {
                    retired.push(managed);
                }
            }

            let catalog =
                self.build_catalog(&config, previous_config.as_ref(), previous_catalog.as_ref());
            state.config = Some(config);
            state.catalog = Some(catalog.clone());
            state.servers = retained;
            (catalog, retired)
        };
        self.publish_current_catalog().await?;
        join_all(
            retired
                .into_iter()
                .map(|server| async move { server.close().await }),
        )
        .await;
        Ok(catalog)
    }

    pub async fn catalog(&self) -> Result<Catalog> {
        self.sync().await
    }

    /// Concurrently initializes every configured server that does not have a managed connection.
    /// Individual startup errors are recorded in the catalog and do not fail the host.
    pub async fn sync_and_start(&self) -> Result<Catalog> {
        self.sync().await?;
        let names = {
            let state = self.state.lock().await;
            let config = state
                .config
                .as_ref()
                .expect("MCP runtime must have a config after synchronization");
            config
                .servers
                .keys()
                .filter(|name| !state.servers.contains_key(*name))
                .cloned()
                .collect::<Vec<_>>()
        };
        let attempts = names.iter().map(|name| self.activate(name));
        let _ = join_all(attempts).await;
        self.catalog_snapshot().await
    }

    /// Returns the current catalog without reading configuration or starting a server.
    pub async fn catalog_snapshot(&self) -> Result<Catalog> {
        self.state
            .lock()
            .await
            .catalog
            .clone()
            .ok_or_else(|| crate::Error::InvalidConfig("MCP runtime is not initialized".into()))
    }

    pub async fn call(&self, selector: &str, arguments: Value) -> Result<CallResult> {
        let (server_name, tool_name) = parse_tool_selector(selector)?;
        let arguments = arguments.as_object().cloned().ok_or_else(|| {
            crate::Error::InvalidConfig("tool arguments must be a JSON object".into())
        })?;
        let (config, managed) = self.server(server_name).await?;
        self.require_authorization(server_name, &config).await?;

        let (peer, generation) = {
            let mut connection = managed.connection.lock().await;
            self.connect_if_needed(&managed, server_name, &config, &mut connection)
                .await?;
            let connection = connection
                .as_ref()
                .expect("connected MCP server must retain its client");
            (connection.peer.clone(), connection.generation)
        };
        let result = self
            .client
            .call(server_name, &peer, tool_name, arguments)
            .await;
        if let Err(error) = &result
            && peer.is_transport_closed()
        {
            let invalidated = {
                let mut connection = managed.connection.lock().await;
                if connection
                    .as_ref()
                    .is_some_and(|current| current.generation == generation)
                {
                    *connection = None;
                    true
                } else {
                    false
                }
            };
            if invalidated {
                self.set_catalog_server(
                    server_name,
                    &config,
                    failed_server(server_name, &config, error.to_string()),
                )
                .await?;
            }
        }
        result
    }

    pub async fn auth_login(&self, server: &str) -> Result<()> {
        self.sync().await?;
        let config = self.current_config().await?;
        oauth_login(&config, server, &self.oauth_root).await?;
        self.invalidate_server(server).await?;
        self.activate(server).await
    }

    pub async fn auth_logout(&self, server: &str) -> Result<()> {
        self.sync().await?;
        let config = self.current_config().await?;
        oauth_logout(&config, server, &self.oauth_root).await?;
        self.invalidate_server(server).await
    }

    pub async fn shutdown(&self) {
        let servers = {
            let mut state = self.state.lock().await;
            std::mem::take(&mut state.servers)
        };
        join_all(
            servers
                .into_values()
                .map(|server| async move { server.close().await }),
        )
        .await;
    }

    async fn activate(&self, name: &str) -> Result<()> {
        let (config, managed) = self.server(name).await?;
        self.require_authorization(name, &config).await?;
        let mut connection = managed.connection.lock().await;
        self.connect_if_needed(&managed, name, &config, &mut connection)
            .await
    }

    async fn server(&self, name: &str) -> Result<(McpServerConfig, Arc<ManagedServer>)> {
        self.sync().await?;
        let mut state = self.state.lock().await;
        let config = state
            .config
            .as_ref()
            .expect("MCP runtime state must have a config after synchronization")
            .servers
            .get(name)
            .cloned()
            .ok_or_else(|| crate::Error::UnknownServer(name.to_string()))?;
        let managed = state
            .servers
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(ManagedServer::new()))
            .clone();
        Ok((config, managed))
    }

    async fn current_config(&self) -> Result<McpConfig> {
        self.state
            .lock()
            .await
            .config
            .clone()
            .ok_or_else(|| crate::Error::InvalidConfig("MCP runtime is not initialized".into()))
    }

    async fn require_authorization(&self, name: &str, config: &McpServerConfig) -> Result<()> {
        match self.client.auth_status(name, config) {
            Ok(AuthStatus::Required) => {
                self.set_catalog_server(name, config, auth_required_server(name, config))
                    .await?;
                Err(crate::Error::AuthRequired {
                    server: name.to_string(),
                })
            }
            Ok(AuthStatus::NotRequired | AuthStatus::Ready) => Ok(()),
            Err(error) => {
                self.set_catalog_server(
                    name,
                    config,
                    failed_server(name, config, error.to_string()),
                )
                .await?;
                Err(error)
            }
        }
    }

    async fn connect_if_needed(
        &self,
        managed: &ManagedServer,
        name: &str,
        config: &McpServerConfig,
        connection: &mut Option<ManagedConnection>,
    ) -> Result<()> {
        if connection.as_ref().is_some_and(|connection| {
            !connection.service.is_closed() && !connection.peer.is_transport_closed()
        }) {
            return Ok(());
        }
        *connection = None;
        let client = match self.client.connect(name, config).await {
            Ok(client) => client,
            Err(error) => {
                self.record_connection_error(name, config, &error).await?;
                return Err(error);
            }
        };
        let peer = client.peer().clone();
        let tools = match self.client.list_tools(name, &peer).await {
            Ok(tools) => tools,
            Err(error) => {
                self.record_connection_error(name, config, &error).await?;
                return Err(error);
            }
        };
        self.set_catalog_server(name, config, ready_server(name, config, tools))
            .await?;
        *connection = Some(ManagedConnection {
            service: client,
            peer,
            generation: managed.next_generation.fetch_add(1, Ordering::Relaxed),
        });
        Ok(())
    }

    async fn record_connection_error(
        &self,
        name: &str,
        config: &McpServerConfig,
        error: &crate::Error,
    ) -> Result<()> {
        let catalog = match error {
            crate::Error::AuthRequired { .. } => auth_required_server(name, config),
            _ => failed_server(name, config, error.to_string()),
        };
        self.set_catalog_server(name, config, catalog).await
    }

    async fn invalidate_server(&self, name: &str) -> Result<()> {
        let managed = {
            let mut state = self.state.lock().await;
            let config = state
                .config
                .as_ref()
                .expect("MCP runtime state must have a config after synchronization")
                .servers
                .get(name)
                .cloned()
                .ok_or_else(|| crate::Error::UnknownServer(name.to_string()))?;
            let managed = state.servers.remove(name);
            let catalog_server = initial_server(&self.client, name, &config);
            replace_catalog_server(
                state
                    .catalog
                    .as_mut()
                    .expect("MCP runtime state must have a catalog after synchronization"),
                catalog_server,
            );
            managed
        };
        self.publish_current_catalog().await?;
        if let Some(managed) = managed {
            managed.close().await;
        }
        Ok(())
    }

    async fn set_catalog_server(
        &self,
        name: &str,
        expected_config: &McpServerConfig,
        server: CatalogServer,
    ) -> Result<()> {
        {
            let mut state = self.state.lock().await;
            let Some(current) = state
                .config
                .as_ref()
                .and_then(|config| config.servers.get(name))
            else {
                return Ok(());
            };
            if current != expected_config {
                return Ok(());
            }
            replace_catalog_server(
                state
                    .catalog
                    .as_mut()
                    .expect("MCP runtime state must have a catalog after synchronization"),
                server,
            );
        }
        self.publish_current_catalog().await
    }

    fn build_catalog(
        &self,
        config: &McpConfig,
        previous_config: Option<&McpConfig>,
        previous_catalog: Option<&Catalog>,
    ) -> Catalog {
        let servers = config
            .servers
            .iter()
            .map(|(name, server_config)| {
                let unchanged = previous_config
                    .and_then(|previous| previous.servers.get(name))
                    .is_some_and(|previous| previous == server_config);
                let prior = unchanged
                    .then(|| {
                        previous_catalog.and_then(|catalog| {
                            catalog.servers.iter().find(|server| server.name == *name)
                        })
                    })
                    .flatten()
                    .cloned();
                prior.unwrap_or_else(|| initial_server(&self.client, name, server_config))
            })
            .collect();
        Catalog {
            config_fingerprint: config.fingerprint.clone(),
            servers,
        }
    }

    async fn publish_current_catalog(&self) -> Result<()> {
        let catalog =
            self.state.lock().await.catalog.clone().ok_or_else(|| {
                crate::Error::InvalidConfig("MCP runtime is not initialized".into())
            })?;
        let profile_root = self
            .config_path
            .parent()
            .and_then(Path::parent)
            .map(|root| std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()))
            .ok_or_else(|| crate::Error::InvalidConfig("MCP profile root is unavailable".into()))?;
        dwo_context::McpSnapshot::set_runtime(
            profile_root,
            dwo_context::McpSnapshot {
                path: std::fs::canonicalize(&self.config_path)
                    .unwrap_or_else(|_| self.config_path.clone()),
                fingerprint: catalog.config_fingerprint.clone(),
                server_count: catalog.servers.len(),
                summary: crate::render_list(&catalog),
            },
        );
        Ok(())
    }

    fn load_config(&self) -> Result<McpConfig> {
        if self.config_path.is_file() {
            McpConfig::from_path(&self.config_path)
        } else {
            McpConfig::from_slice(br#"{"mcpServers":{}}"#, self.config_path.parent())
        }
    }
}

fn initial_server(client: &McpClient, name: &str, config: &McpServerConfig) -> CatalogServer {
    match client.auth_status(name, config) {
        Ok(AuthStatus::Required) => auth_required_server(name, config),
        Ok(AuthStatus::NotRequired | AuthStatus::Ready) => CatalogServer {
            name: name.to_string(),
            description: config.description().map(str::to_owned),
            status: ServerStatus::Starting,
            tool_count: 0,
            error: None,
            tools: Vec::new(),
        },
        Err(error) => failed_server(name, config, error.to_string()),
    }
}

fn auth_required_server(name: &str, config: &McpServerConfig) -> CatalogServer {
    CatalogServer {
        name: name.to_string(),
        description: config.description().map(str::to_owned),
        status: ServerStatus::AuthRequired,
        tool_count: 0,
        error: Some("authorization required".to_string()),
        tools: Vec::new(),
    }
}

fn ready_server(
    name: &str,
    config: &McpServerConfig,
    tools: Vec<rmcp::model::Tool>,
) -> CatalogServer {
    CatalogServer {
        name: name.to_string(),
        description: config.description().map(str::to_owned),
        status: ServerStatus::Ready,
        tool_count: tools.len(),
        error: None,
        tools: tools.into_iter().map(catalog_tool).collect(),
    }
}

fn failed_server(name: &str, config: &McpServerConfig, error: String) -> CatalogServer {
    CatalogServer {
        name: name.to_string(),
        description: config.description().map(str::to_owned),
        status: ServerStatus::Failed,
        tool_count: 0,
        error: Some(error),
        tools: Vec::new(),
    }
}

fn replace_catalog_server(catalog: &mut Catalog, replacement: CatalogServer) {
    if let Some(existing) = catalog
        .servers
        .iter_mut()
        .find(|server| server.name == replacement.name)
    {
        *existing = replacement;
    } else {
        catalog.servers.push(replacement);
        catalog
            .servers
            .sort_by(|left, right| left.name.cmp(&right.name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn startup_records_failed_servers_in_memory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("resource")).unwrap();
        std::fs::write(
            root.path().join("resource/mcp.json"),
            r#"{"mcpServers":{"local":{"command":"definitely-not-installed-dwo-mcp"}}}"#,
        )
        .unwrap();

        let runtime = McpRuntime::new(root.path());
        let catalog = runtime.catalog().await.unwrap();
        assert_eq!(catalog.servers[0].status, ServerStatus::Starting);
        assert!(!root.path().join("runtime/mcp/catalog.json").is_file());
        assert!(!root.path().join("mcp_runtime").exists());

        let catalog = runtime.sync_and_start().await.unwrap();
        assert_eq!(catalog.servers[0].status, ServerStatus::Failed);
        assert!(
            catalog.servers[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("definitely-not-installed-dwo-mcp"))
        );
        let error = runtime
            .call("local.ping", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("definitely-not-installed-dwo-mcp")
        );
        assert_eq!(
            runtime.catalog().await.unwrap().servers[0].status,
            ServerStatus::Failed
        );
    }

    #[tokio::test]
    async fn catalog_snapshot_does_not_sync_new_configuration() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("resource")).unwrap();
        std::fs::write(
            root.path().join("resource/mcp.json"),
            r#"{"mcpServers":{}}"#,
        )
        .unwrap();
        let runtime = McpRuntime::new(root.path());
        runtime.sync_and_start().await.unwrap();

        std::fs::write(
            root.path().join("resource/mcp.json"),
            r#"{"mcpServers":{"new":{"command":"definitely-not-installed-dwo-mcp"}}}"#,
        )
        .unwrap();
        assert!(runtime.catalog_snapshot().await.unwrap().servers.is_empty());
        assert_eq!(
            runtime.sync_and_start().await.unwrap().servers[0].status,
            ServerStatus::Failed
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn startup_and_calls_reuse_one_managed_stdio_server_session() {
        let root = tempfile::tempdir().unwrap();
        let resource = root.path().join("resource");
        std::fs::create_dir_all(&resource).unwrap();
        let script = root.path().join("persistent-server.ps1");
        std::fs::write(&script, POWERSHELL_MCP_SERVER).unwrap();
        let pid_file = root.path().join("server.pid");
        let config = serde_json::json!({
            "mcpServers": {
                "local": {
                    "command": "powershell",
                    "args": [
                        "-NoProfile",
                        "-ExecutionPolicy",
                        "Bypass",
                        "-File",
                        script.to_string_lossy(),
                    ],
                    "env": {
                        "DWO_MCP_TEST_PID_FILE": pid_file.to_string_lossy(),
                    }
                }
            }
        });
        std::fs::write(
            resource.join("mcp.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();

        let runtime = McpRuntime::new(root.path());
        let catalog = runtime.sync_and_start().await.unwrap();
        assert_eq!(catalog.servers[0].status, ServerStatus::Ready);
        assert_eq!(catalog.servers[0].tools[0].name, "ping");
        let first = runtime
            .call("local.ping", serde_json::json!({}))
            .await
            .unwrap();
        let managed = runtime
            .state
            .lock()
            .await
            .servers
            .get("local")
            .cloned()
            .expect("first call must register a managed server");
        assert!(
            !managed
                .connection
                .lock()
                .await
                .as_ref()
                .expect("first call must retain the connection")
                .service
                .is_closed(),
            "the server session must remain open after its first call"
        );
        let second = runtime
            .call("local.ping", serde_json::json!({}))
            .await
            .unwrap();
        let first_pid = call_result_text(&first).to_string();
        assert_eq!(first_pid, call_result_text(&second));
        assert_eq!(std::fs::read_to_string(pid_file).unwrap().trim(), first_pid);
        assert_eq!(
            runtime.catalog().await.unwrap().servers[0].status,
            ServerStatus::Ready
        );

        runtime.shutdown().await;

        let restarted = McpRuntime::new(root.path());
        let catalog = restarted.sync_and_start().await.unwrap();
        assert_eq!(catalog.servers[0].status, ServerStatus::Ready);
        let restarted_call = restarted
            .call("local.ping", serde_json::json!({}))
            .await
            .unwrap();
        assert_ne!(first_pid, call_result_text(&restarted_call));
        restarted.shutdown().await;
    }

    #[cfg(windows)]
    fn call_result_text(result: &CallResult) -> &str {
        result.content[0]
            .get("text")
            .and_then(Value::as_str)
            .expect("PowerShell fixture must return a text content block")
    }

    #[cfg(windows)]
    const POWERSHELL_MCP_SERVER: &str = r#"
[System.IO.File]::WriteAllText($env:DWO_MCP_TEST_PID_FILE, [string]$PID)
while ($null -ne ($line = [Console]::In.ReadLine())) {
    $message = $line | ConvertFrom-Json
    $response = $null
    if ($message.method -eq 'initialize') {
        $response = @{
            jsonrpc = '2.0'
            id = $message.id
            result = @{
                protocolVersion = $message.params.protocolVersion
                capabilities = @{ tools = @{} }
                serverInfo = @{ name = 'persistent-test'; version = '1.0.0' }
            }
        }
    } elseif ($message.method -eq 'tools/list') {
        $response = @{
            jsonrpc = '2.0'
            id = $message.id
            result = @{
                tools = @(@{
                    name = 'ping'
                    description = 'Returns the server process id'
                    inputSchema = @{ type = 'object' }
                })
            }
        }
    } elseif ($message.method -eq 'tools/call') {
        $response = @{
            jsonrpc = '2.0'
            id = $message.id
            result = @{ content = @(@{ type = 'text'; text = [string]$PID }) }
        }
    }
    if ($null -ne $response) {
        $response | ConvertTo-Json -Compress -Depth 10
        [Console]::Out.Flush()
    }
}
"#;
}
