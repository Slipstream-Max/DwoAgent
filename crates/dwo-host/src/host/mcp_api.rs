use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use super::Host;

#[derive(Deserialize)]
pub(crate) struct McpInstallParam {
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    config: Option<Value>,
    #[serde(default)]
    servers: Option<serde_json::Map<String, Value>>,
}

#[derive(Deserialize)]
struct McpSearchParam {
    query: String,
}

#[derive(Deserialize)]
struct McpCallParam {
    selector: String,
    arguments: Value,
}

#[derive(Deserialize)]
struct McpAuthParam {
    server: String,
}

#[derive(Deserialize)]
struct McpServerParam {
    server: String,
}

impl Host {
    pub(crate) async fn dispatch_mcp(&self, method: &str, params: Value) -> Result<Value> {
        match method {
            "mcp.list" => self.mcp_list().await,
            "mcp.config" => self.mcp_config(),
            "mcp.search" => {
                let params: McpSearchParam = serde_json::from_value(params)?;
                self.mcp_search(params.query).await
            }
            "mcp.call" => {
                let params: McpCallParam = serde_json::from_value(params)?;
                self.mcp_call(params.selector, params.arguments).await
            }
            "mcp.auth.login" => {
                let params: McpAuthParam = serde_json::from_value(params)?;
                self.mcp_auth(params.server, true).await
            }
            "mcp.auth.logout" | "mcp.auth.unauth" => {
                let params: McpAuthParam = serde_json::from_value(params)?;
                self.mcp_auth(params.server, false).await
            }
            "mcp.enable" | "mcp.disable" => {
                let params: McpServerParam = serde_json::from_value(params)?;
                self.mcp_set_enabled(params.server, method == "mcp.enable")
                    .await
            }
            "mcp.install" => {
                let params: McpInstallParam = serde_json::from_value(params)?;
                self.mcp_install(params).await
            }
            "mcp.uninstall" => {
                let params: McpServerParam = serde_json::from_value(params)?;
                self.mcp_uninstall(params.server).await
            }
            other => anyhow::bail!("unknown MCP method: {other}"),
        }
    }
}

impl Host {
    pub(crate) async fn mcp_list(&self) -> Result<Value> {
        Ok(serde_json::to_value(self.mcp.catalog_snapshot().await?)?)
    }

    pub(crate) fn mcp_config(&self) -> Result<Value> {
        Ok(super::redacted_mcp_config(&super::read_mcp_document(
            self.mcp.config_path(),
        )?))
    }

    pub(crate) async fn mcp_search(&self, query: String) -> Result<Value> {
        let catalog = self.mcp.catalog_snapshot().await?;
        Ok(serde_json::to_value(catalog.search(&query))?)
    }

    pub(crate) async fn mcp_call(&self, selector: String, arguments: Value) -> Result<Value> {
        Ok(serde_json::to_value(
            self.mcp.call(&selector, arguments).await?,
        )?)
    }

    pub(crate) async fn mcp_auth(&self, server: String, authorized: bool) -> Result<Value> {
        if authorized {
            self.mcp.auth_login(&server).await?;
        } else {
            self.mcp.auth_logout(&server).await?;
        }
        self.events
            .publish(
                "mcp.status",
                json!({
                    "server": server,
                    "status": if authorized { "authorized" } else { "unauthorized" }
                }),
            )
            .await;
        Ok(json!({"authorized": authorized, "server": server}))
    }

    pub(crate) async fn mcp_set_enabled(&self, server: String, enabled: bool) -> Result<Value> {
        let changed = self.mutate_mcp_server(&server, enabled).await?;
        self.mcp.sync_and_start().await?;
        self.events
            .publish("mcp.status", json!({"server": server, "enabled": enabled}))
            .await;
        Ok(json!({"server": server, "enabled": enabled, "changed": changed}))
    }

    pub(crate) async fn mcp_install(&self, params: McpInstallParam) -> Result<Value> {
        let installed = self.install_mcp(params).await?;
        self.mcp.sync_and_start().await?;
        self.events
            .publish("mcp.status", json!({"status": "installed"}))
            .await;
        Ok(json!({"installed": installed}))
    }

    pub(crate) async fn mcp_uninstall(&self, server: String) -> Result<Value> {
        let removed = self.uninstall_mcp(&server).await?;
        self.mcp.sync_and_start().await?;
        self.events
            .publish(
                "mcp.status",
                json!({"server": server, "status": "uninstalled"}),
            )
            .await;
        Ok(json!({"server": server, "removed": removed}))
    }

    async fn mutate_mcp_server(&self, name: &str, enabled: bool) -> Result<bool> {
        super::validate_resource_name(name)?;
        let mut root = super::read_mcp_document(self.mcp.config_path())?;
        let servers = root
            .get_mut("mcpServers")
            .and_then(Value::as_object_mut)
            .context("mcpServers must be an object")?;
        let Some(server) = servers.get_mut(name) else {
            anyhow::bail!("MCP server not found: {name}");
        };
        let previous = server
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        server["enabled"] = Value::Bool(enabled);
        let bytes = serde_json::to_vec_pretty(&root)?;
        self.config_manager
            .write_resource(self.mcp.config_path(), bytes, |bytes| {
                dwo_mcp::McpConfig::from_slice(bytes, self.mcp.config_path().parent())?;
                Ok(())
            })
            .await?;
        Ok(previous != enabled)
    }

    async fn install_mcp(&self, params: McpInstallParam) -> Result<Vec<String>> {
        let mut root = super::read_mcp_document(self.mcp.config_path())?;
        let target = root
            .get_mut("mcpServers")
            .and_then(Value::as_object_mut)
            .context("mcpServers must be an object")?;
        let mut entries = serde_json::Map::new();
        if let Some(servers) = params.servers {
            entries.extend(servers);
        }
        if let Some(config) = params.config {
            if let Some(map) = config.get("mcpServers").and_then(Value::as_object) {
                entries.extend(map.clone());
            } else if let Some(name) = params.server {
                entries.insert(name, config);
            } else {
                anyhow::bail!("config must contain mcpServers or server must be specified");
            }
        }
        anyhow::ensure!(!entries.is_empty(), "no MCP server configuration supplied");
        let names = entries.keys().cloned().collect::<Vec<_>>();
        for (name, mut value) in entries {
            super::validate_resource_name(&name)?;
            if let Some(object) = value.as_object_mut() {
                object.entry("enabled").or_insert(Value::Bool(true));
            }
            target.insert(name, value);
        }
        let bytes = serde_json::to_vec_pretty(&root)?;
        dwo_mcp::McpConfig::from_slice(&bytes, self.mcp.config_path().parent())?;
        self.config_manager
            .write_resource(self.mcp.config_path(), bytes, |bytes| {
                dwo_mcp::McpConfig::from_slice(bytes, self.mcp.config_path().parent())?;
                Ok(())
            })
            .await?;
        Ok(names)
    }

    async fn uninstall_mcp(&self, name: &str) -> Result<bool> {
        super::validate_resource_name(name)?;
        let mut root = super::read_mcp_document(self.mcp.config_path())?;
        let servers = root
            .get_mut("mcpServers")
            .and_then(Value::as_object_mut)
            .context("mcpServers must be an object")?;
        let removed = servers.remove(name).is_some();
        if removed {
            let bytes = serde_json::to_vec_pretty(&root)?;
            self.config_manager
                .write_resource(self.mcp.config_path(), bytes, |bytes| {
                    dwo_mcp::McpConfig::from_slice(bytes, self.mcp.config_path().parent())?;
                    Ok(())
                })
                .await?;
        }
        Ok(removed)
    }
}
