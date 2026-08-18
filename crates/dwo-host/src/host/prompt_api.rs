use std::path::PathBuf;

use anyhow::Result;
use serde_json::{Value, json};

use super::Host;

impl Host {
    pub(crate) async fn prompt_list(&self) -> Result<Value> {
        let dir = self.profile_root.join("resource/prompts");
        let mut files = Vec::new();
        if dir.is_dir() {
            for entry in std::fs::read_dir(dir)? {
                let path = entry?.path();
                if path.extension().and_then(|extension| extension.to_str()) == Some("md") {
                    files.push(path.file_name().unwrap().to_string_lossy().into_owned());
                }
            }
        }
        files.sort();
        Ok(json!({"files": files}))
    }

    pub(crate) async fn prompt_get(&self, domain: &str, name: String) -> Result<Value> {
        let path = self.prompt_path(domain, &name)?;
        Ok(json!({
            "name": name,
            "content": tokio::fs::read_to_string(path).await.unwrap_or_default()
        }))
    }

    pub(crate) async fn prompt_set(
        &self,
        domain: &str,
        name: String,
        content: String,
    ) -> Result<Value> {
        let path = self.prompt_path(domain, &name)?;
        self.config_manager
            .write_resource(&path, content.into_bytes(), |_| Ok(()))
            .await?;
        self.events
            .publish("config.changed", json!({"source": domain, "name": name}))
            .await;
        Ok(json!({"name": name, "updated": true}))
    }

    fn prompt_path(&self, domain: &str, name: &str) -> Result<PathBuf> {
        anyhow::ensure!(
            matches!(domain, "prompt" | "rule"),
            "unknown resource domain: {domain}"
        );
        let default_name = if domain == "prompt" {
            "System.md"
        } else {
            "AGENTS.md"
        };
        let name = if name.is_empty() { default_name } else { name };
        super::validate_markdown_name(name)?;
        Ok(self.profile_root.join("resource/prompts").join(name))
    }

    pub(crate) fn default_prompt_name(domain: &str) -> Result<String> {
        Ok(match domain {
            "prompt" => "System.md".to_string(),
            "rule" => "AGENTS.md".to_string(),
            _ => anyhow::bail!("unknown resource domain: {domain}"),
        })
    }
}
