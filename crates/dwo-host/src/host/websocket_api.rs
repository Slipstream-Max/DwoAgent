use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dwo_agent_service::WebsocketConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Host;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebsocketRuntime {
    pub config: WebsocketConfig,
    pub acp_token: String,
    pub management_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WebsocketSecret {
    acp_token: String,
    management_token: String,
}

impl WebsocketSecret {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.acp_token.trim().is_empty(),
            "websocket acp token is empty"
        );
        anyhow::ensure!(
            !self.management_token.trim().is_empty(),
            "websocket management token is empty"
        );
        anyhow::ensure!(
            self.acp_token != self.management_token,
            "websocket ACP and management tokens must be different"
        );
        Ok(())
    }
}

impl Host {
    pub fn websocket_snapshot(&self) -> WebsocketConfig {
        self.profile
            .read()
            .expect("profile lock poisoned")
            .config
            .websocket
            .clone()
    }

    pub fn set_websocket_running(&self, running: bool) {
        self.websocket_running.store(running, Ordering::Release);
    }

    pub async fn websocket_runtime(&self) -> Result<WebsocketRuntime> {
        let secret = self.ensure_websocket_secret().await?;
        Ok(WebsocketRuntime {
            config: self.websocket_snapshot(),
            acp_token: secret.acp_token,
            management_token: secret.management_token,
        })
    }

    pub async fn websocket_status(&self) -> Result<Value> {
        let config = self.websocket_snapshot();
        Ok(json!({
            "enabled": config.enabled,
            "running": self.websocket_running.load(Ordering::Acquire),
            "bind": config.bind,
            "port": config.port,
            "listen": format!("{}:{}", config.bind, config.port),
            "paths": ["/acp", "/dwo"],
            "authentication": "token",
        }))
    }

    pub async fn websocket_set_enabled(self: &Arc<Self>, enabled: bool) -> Result<Value> {
        self.config_manager
            .update(|profile| {
                profile.websocket.enabled = enabled;
                Ok(())
            })
            .await?;
        self.reload_profile_if_changed().await?;
        let status = self.websocket_status().await?;
        self.events
            .publish("websocket.status", status.clone())
            .await;
        Ok(status)
    }

    pub async fn websocket_config(self: &Arc<Self>, update: Option<Value>) -> Result<Value> {
        if let Some(update) = update {
            let config: WebsocketConfig = serde_json::from_value(update)?;
            config.validate().map_err(anyhow::Error::msg)?;
            self.config_manager
                .update(|profile| {
                    profile.websocket = config.clone();
                    Ok(())
                })
                .await?;
            self.reload_profile_if_changed().await?;
            self.events
                .publish("websocket.status", self.websocket_status().await?)
                .await;
        }
        Ok(serde_json::to_value(self.websocket_snapshot())?)
    }

    pub async fn websocket_token(&self) -> Result<Value> {
        let runtime = self.websocket_runtime().await?;
        Ok(json!({
            "acpToken": runtime.acp_token,
            "managementToken": runtime.management_token,
            "bind": runtime.config.bind,
            "port": runtime.config.port,
            "acpPath": "/acp",
            "managementPath": "/dwo",
        }))
    }

    pub async fn websocket_reset_token(&self) -> Result<Value> {
        let secret = new_secret()?;
        self.write_websocket_secret(&secret).await?;
        self.events
            .publish("websocket.status", json!({"status": "token_reset"}))
            .await;
        Ok(json!({
            "acpToken": secret.acp_token,
            "managementToken": secret.management_token,
            "reset": true,
        }))
    }

    async fn ensure_websocket_secret(&self) -> Result<WebsocketSecret> {
        let path = self.websocket_secret_path();
        if path.is_file() {
            let source = tokio::fs::read_to_string(&path)
                .await
                .with_context(|| format!("read {}", path.display()))?;
            let secret: WebsocketSecret = serde_yaml::from_str(&source)
                .with_context(|| format!("parse {}", path.display()))?;
            secret.validate()?;
            return Ok(secret);
        }
        let secret = new_secret()?;
        self.write_websocket_secret(&secret).await?;
        Ok(secret)
    }

    async fn write_websocket_secret(&self, secret: &WebsocketSecret) -> Result<()> {
        let path = self.websocket_secret_path();
        let source = serde_yaml::to_string(secret)?;
        dwo_agent_service::atomic_file::write(&path, source.into_bytes()).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
        }
        Ok(())
    }

    fn websocket_secret_path(&self) -> std::path::PathBuf {
        self.profile_root.join("runtime/websocket/secret.yaml")
    }
}

fn new_secret() -> Result<WebsocketSecret> {
    let token = || -> Result<String> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).context("generate WebSocket token")?;
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    };
    Ok(WebsocketSecret {
        acp_token: token()?,
        management_token: token()?,
    })
}
