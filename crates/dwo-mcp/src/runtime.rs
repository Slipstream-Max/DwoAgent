use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::{
    AuthStatus, CallResult, Catalog, FileOAuthProvider, McpClient, McpConfig, Result, oauth_login,
    oauth_logout, oauth_status, write_catalog_cache,
};

pub struct McpRuntime {
    config_path: PathBuf,
    catalog_path: PathBuf,
    oauth_root: PathBuf,
    client: McpClient,
    state: Mutex<Option<Catalog>>,
}

impl McpRuntime {
    pub fn new(profile_root: impl AsRef<Path>) -> Self {
        let profile_root = profile_root.as_ref();
        let oauth_root = profile_root.join("mcp/oauth");
        Self {
            config_path: profile_root.join("resource/mcp.json"),
            catalog_path: profile_root.join("mcp/catalog.json"),
            client: McpClient::with_file_oauth(
                Arc::new(FileOAuthProvider::new(oauth_root.clone())),
                oauth_root.clone(),
            ),
            oauth_root,
            state: Mutex::new(None),
        }
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn catalog_path(&self) -> &Path {
        &self.catalog_path
    }

    pub async fn refresh(&self) -> Result<Catalog> {
        let config = self.load_config()?;
        let catalog = self.client.discover(&config).await;
        if let Some(parent) = self.catalog_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_catalog_cache(&self.catalog_path, &catalog)?;
        *self.state.lock().await = Some(catalog.clone());
        Ok(catalog)
    }

    pub async fn refresh_if_changed(&self) -> Result<Catalog> {
        let config = self.load_config()?;
        if let Some(catalog) = self.state.lock().await.as_ref()
            && catalog.config_fingerprint == config.fingerprint
        {
            return Ok(catalog.clone());
        }
        self.refresh().await
    }

    pub async fn catalog(&self) -> Result<Catalog> {
        self.refresh_if_changed().await
    }

    pub async fn call(&self, selector: &str, arguments: Value) -> Result<CallResult> {
        let config = self.load_config()?;
        self.client.call(&config, selector, arguments).await
    }

    pub async fn auth_status(&self, server: &str) -> Result<AuthStatus> {
        let config = self.load_config()?;
        oauth_status(&config, server, &self.oauth_root)
    }

    pub async fn auth_login(&self, server: &str) -> Result<()> {
        let config = self.load_config()?;
        oauth_login(&config, server, &self.oauth_root).await?;
        self.refresh().await?;
        Ok(())
    }

    pub async fn auth_logout(&self, server: &str) -> Result<()> {
        let config = self.load_config()?;
        oauth_logout(&config, server, &self.oauth_root).await?;
        self.refresh().await?;
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
