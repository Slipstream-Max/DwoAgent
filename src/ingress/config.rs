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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FeishuChannelDomain {
    Feishu,
    Lark,
}

impl Default for FeishuChannelDomain {
    fn default() -> Self {
        Self::Feishu
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FeishuAccessPolicy {
    #[serde(rename = "allow_all", alias = "open")]
    AllowAll,
    #[serde(rename = "white_list", alias = "whitelist", alias = "allowlist")]
    WhiteList,
}

impl Default for FeishuAccessPolicy {
    fn default() -> Self {
        Self::WhiteList
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeishuChannelConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_workspace_dir")]
    pub workspace_dir: String,
    #[serde(default)]
    pub domain: FeishuChannelDomain,
    #[serde(default)]
    pub dm_policy: FeishuAccessPolicy,
    #[serde(default)]
    pub group_policy: FeishuAccessPolicy,
    #[serde(default)]
    pub allow_from: Vec<String>,
    #[serde(default)]
    pub group_allow_from: Vec<String>,
    #[serde(default = "default_true")]
    pub group_require_mention: bool,
    #[serde(default)]
    pub media_input: bool,
    #[serde(default)]
    pub media_output: bool,
    #[serde(default)]
    pub card_output: bool,
    #[serde(default)]
    pub override_model: Option<String>,
    #[serde(default)]
    pub override_reasoning_mode: Option<ReasoningMode>,
}

impl Default for FeishuChannelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            workspace_dir: default_workspace_dir(),
            domain: FeishuChannelDomain::default(),
            dm_policy: FeishuAccessPolicy::default(),
            group_policy: FeishuAccessPolicy::default(),
            allow_from: Vec::new(),
            group_allow_from: Vec::new(),
            group_require_mention: true,
            media_input: false,
            media_output: false,
            card_output: false,
            override_model: None,
            override_reasoning_mode: None,
        }
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

    #[test]
    fn feishu_config_accepts_policy_and_session_options() {
        let config: ChannelRuntimeConfig = serde_yaml::from_str(
            r#"
feishu:
  enabled: true
  workspace_dir: .
  domain: feishu
  dm_policy: allow_all
  group_policy: white_list
  allow_from: ["*"]
  group_allow_from: ["oc_abc"]
  group_require_mention: true
  media_input: true
  media_output: true
  card_output: true
  override_model: deepseek-v4-pro
  override_reasoning_mode: high
"#,
        )
        .unwrap();

        assert!(config.feishu.enabled);
        assert_eq!(config.feishu.domain, FeishuChannelDomain::Feishu);
        assert_eq!(config.feishu.dm_policy, FeishuAccessPolicy::AllowAll);
        assert_eq!(config.feishu.group_policy, FeishuAccessPolicy::WhiteList);
        assert_eq!(config.feishu.allow_from, vec!["*"]);
        assert_eq!(config.feishu.group_allow_from, vec!["oc_abc"]);
        assert!(config.feishu.group_require_mention);
        assert!(config.feishu.media_input);
        assert!(config.feishu.media_output);
        assert!(config.feishu.card_output);
        assert_eq!(
            config.feishu.override_model.as_deref(),
            Some("deepseek-v4-pro")
        );
        assert_eq!(
            config.feishu.override_reasoning_mode,
            Some(ReasoningMode::High)
        );
    }
}
