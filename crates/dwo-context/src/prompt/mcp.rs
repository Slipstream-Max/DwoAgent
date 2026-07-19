use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{stable_fingerprint, xml_escape};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpSnapshot {
    pub path: PathBuf,
    pub fingerprint: String,
    #[serde(default)]
    pub server_count: usize,
    #[serde(default)]
    pub summary: String,
}

impl McpSnapshot {
    pub(crate) fn read(path: &Path) -> std::io::Result<Option<Self>> {
        if !path.is_file() {
            return Ok(None);
        }
        let bytes = std::fs::read(path)?;
        let config: Value = match serde_json::from_slice(&bytes) {
            Ok(config) => config,
            Err(error) => {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error));
            }
        };
        let Some(servers) = config.get("mcpServers").and_then(Value::as_object) else {
            return Ok(None);
        };
        if servers.is_empty() {
            return Ok(None);
        }
        let fingerprint = stable_fingerprint(&bytes);
        let catalog_path = path
            .parent()
            .and_then(Path::parent)
            .map(|root| root.join("runtime/mcp/catalog.json"));
        let summary = catalog_path
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|source| serde_json::from_str::<Value>(&source).ok())
            .filter(|catalog| {
                catalog.get("configFingerprint").and_then(Value::as_str)
                    == Some(fingerprint.as_str())
            })
            .and_then(|catalog| {
                catalog
                    .get("summary")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|summary| !summary.trim().is_empty())
            .unwrap_or_else(|| {
                servers
                    .keys()
                    .map(|name| format!("{name}    ? tools    starting"))
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        Ok(Some(Self {
            path: std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
            fingerprint,
            server_count: servers.len(),
            summary,
        }))
    }

    pub(crate) fn render(&self) -> String {
        format!(
            "<mcp>\nconfig: {}\n\nservers:\n{}\n\nMCP servers are initialized by the host before the session starts and their connections stay managed. Search the static catalog with `dwo mcp search <query>`, call a discovered tool with `dwo mcp call <server.tool> --args '<json>'`, and authenticate with `dwo mcp auth <server>` or `--logout`.\n</mcp>",
            xml_escape(&self.path.display().to_string()),
            xml_escape(&self.summary)
        )
    }
}
