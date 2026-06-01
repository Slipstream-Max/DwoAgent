//! Configuration for optional agent ingress channels.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::models::ReasoningMode;
use crate::utils::files::read_utf8_text;

pub const CHANNELS_CONFIG_FILE: &str = "channels.yaml";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSocketIngressConfig {
    #[serde(default)]
    pub enabled: bool,
}

impl Default for WebSocketIngressConfig {
    fn default() -> Self {
        Self { enabled: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeixinChannelConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_workspace_dir")]
    pub workspace_dir: String,
    #[serde(default = "default_true")]
    pub markdown_filter: bool,
    #[serde(default)]
    pub media_input: bool,
    #[serde(default)]
    pub media_output: bool,
    #[serde(default)]
    pub override_model: Option<String>,
    #[serde(default)]
    pub override_reasoning_mode: Option<ReasoningMode>,
}

impl Default for WeixinChannelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            workspace_dir: default_workspace_dir(),
            markdown_filter: true,
            media_input: false,
            media_output: false,
            override_model: None,
            override_reasoning_mode: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeishuChannelConfig {
    #[serde(default)]
    pub enabled: bool,
}

impl Default for FeishuChannelConfig {
    fn default() -> Self {
        Self { enabled: false }
    }
}

/// Optional ingress configuration loaded from an agent's channels.yaml.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelRuntimeConfig {
    #[serde(default)]
    pub websocket: WebSocketIngressConfig,
    #[serde(default)]
    pub weixin: WeixinChannelConfig,
    #[serde(default)]
    pub feishu: FeishuChannelConfig,
}

impl Default for ChannelRuntimeConfig {
    fn default() -> Self {
        Self {
            websocket: WebSocketIngressConfig::default(),
            weixin: WeixinChannelConfig::default(),
            feishu: FeishuChannelConfig::default(),
        }
    }
}

impl ChannelRuntimeConfig {
    pub fn has_enabled_channels(&self) -> bool {
        self.websocket.enabled || self.weixin.enabled || self.feishu.enabled
    }
}

fn default_workspace_dir() -> String {
    ".".to_string()
}

fn default_true() -> bool {
    true
}

pub fn load_channel_runtime_config(agent_structure_dir: &Path) -> Result<ChannelRuntimeConfig> {
    let path = agent_structure_dir.join(CHANNELS_CONFIG_FILE);
    if !path.is_file() {
        return Ok(ChannelRuntimeConfig::default());
    }

    let text = read_utf8_text(&path)?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(ChannelRuntimeConfig::default());
    }

    let loaded: Value = serde_yaml::from_str(trimmed)
        .with_context(|| format!("parse YAML in {}", path.display()))?;

    match loaded {
        Value::Null => Ok(ChannelRuntimeConfig::default()),
        Value::Object(map) => {
            let config: ChannelRuntimeConfig = serde_json::from_value(Value::Object(map))
                .with_context(|| format!("Invalid YAML config in {}", path.display()))?;
            Ok(config)
        }
        _ => bail!(
            "Invalid YAML config in {}: expected a mapping",
            path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weixin_config_accepts_session_creation_overrides() {
        let config: ChannelRuntimeConfig = serde_yaml::from_str(
            r#"
weixin:
  enabled: true
  media_output: true
  override_model: deepseek-v4-pro
  override_reasoning_mode: high
"#,
        )
        .unwrap();

        assert!(config.weixin.enabled);
        assert!(config.weixin.media_output);
        assert_eq!(
            config.weixin.override_model.as_deref(),
            Some("deepseek-v4-pro")
        );
        assert_eq!(
            config.weixin.override_reasoning_mode,
            Some(ReasoningMode::High)
        );
    }
}
