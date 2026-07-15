use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dwo_context::SystemPromptBuilder;
use dwo_model_client::{AgentModelConfig, ModelCatalog, ModelClientConfig};
use dwo_tools::SessionMode;
use serde::{Deserialize, Serialize};

use crate::AgentServiceError;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentProfileConfig {
    pub name: String,
    pub description: String,
    pub policy_mode: SessionMode,
    pub model: AgentModelConfig,
    #[serde(default)]
    pub channels: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone)]
pub struct LoadedAgentProfile {
    pub root: PathBuf,
    pub config: AgentProfileConfig,
    pub models: ModelClientConfig,
}

impl LoadedAgentProfile {
    pub fn load(root: impl AsRef<Path>) -> Result<Self, AgentServiceError> {
        let root = std::fs::canonicalize(root.as_ref()).map_err(anyhow::Error::from)?;
        let config = AgentProfileConfig::load(&root)?;
        let models = config.resolve_models(
            &ModelCatalog::builtin()
                .map_err(|error| AgentServiceError::InvalidConfig(error.to_string()))?,
        )?;
        SystemPromptBuilder::new(Some(root.clone()), root.clone())
            .build_initial()
            .map_err(anyhow::Error::from)?;
        Ok(Self {
            root,
            config,
            models,
        })
    }
}

pub fn load_profile(root: impl AsRef<Path>) -> Result<LoadedAgentProfile, AgentServiceError> {
    LoadedAgentProfile::load(root)
}

impl AgentProfileConfig {
    pub fn from_yaml(source: &str) -> Result<Self, AgentServiceError> {
        let profile: Self = serde_yaml::from_str(source)
            .map_err(|error| AgentServiceError::InvalidConfig(error.to_string()))?;
        profile.validate()?;
        Ok(profile)
    }

    pub fn load(root: impl AsRef<Path>) -> Result<Self, AgentServiceError> {
        let path = root.as_ref().join("profile.yaml");
        let source = std::fs::read_to_string(&path).map_err(anyhow::Error::from)?;
        Self::from_yaml(&source)
    }

    pub fn validate(&self) -> Result<(), AgentServiceError> {
        validate_text(&self.name, "name")?;
        validate_text(&self.description, "description")?;
        self.model
            .validate()
            .map_err(|error| AgentServiceError::InvalidConfig(error.to_string()))
    }

    pub fn resolve_models(
        &self,
        catalog: &ModelCatalog,
    ) -> Result<ModelClientConfig, AgentServiceError> {
        ModelClientConfig::resolve(catalog, &self.model)
            .map_err(|error| AgentServiceError::InvalidConfig(error.to_string()))
    }
}

fn validate_text(value: &str, field: &str) -> Result<(), AgentServiceError> {
    if value.trim().is_empty() {
        return Err(AgentServiceError::InvalidConfig(format!(
            "{field} must not be empty"
        )));
    }
    if value != value.trim() {
        return Err(AgentServiceError::InvalidConfig(format!(
            "{field} must not contain surrounding whitespace"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_has_no_tool_switches_and_keeps_provider_credentials_shared() {
        let profile = AgentProfileConfig::from_yaml(
            r#"
name: coder
description: coding agent
policyMode: confirm
channels:
  weixin:
    enabled: true
    streamMode: answer
    replayTurns: 5
    markdownFilter: true
model:
  defaultModelId: deepseek-v4-pro
  providers:
    deepseek:
      type: deepseek
      apiKeyEnv: DEEPSEEK_API_KEY
  models:
    - modelName: deepseek-v4-pro
      provider: deepseek
      modelId: deepseek-v4-pro
    - modelName: deepseek-v4-flash
      provider: deepseek
      modelId: deepseek-v4-flash
"#,
        )
        .unwrap();

        assert_eq!(profile.name, "coder");
        assert_eq!(profile.policy_mode, SessionMode::Confirm);
        assert!(profile.channels.contains_key("weixin"));
        assert_eq!(profile.model.providers.len(), 1);
        assert_eq!(profile.model.models.len(), 2);
        let resolved = profile
            .resolve_models(&ModelCatalog::builtin().unwrap())
            .unwrap();
        assert_eq!(resolved.providers.len(), 1);
        assert_eq!(resolved.models.len(), 2);
    }

    #[test]
    fn profile_rejects_removed_tool_switches() {
        let error = AgentProfileConfig::from_yaml(
            r#"
name: coder
description: coding agent
policyMode: confirm
tools: {}
model:
  defaultModelId: chat
  providers:
    local:
      type: local
  models:
    - modelName: chat
      provider: local
      modelId: chat
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown field `tools`"));
    }

    #[test]
    fn profile_rejects_provider_transport_configuration() {
        let error = AgentProfileConfig::from_yaml(
            r#"
name: coder
description: coding agent
policyMode: confirm
model:
  defaultModelId: chat
  providers:
    local:
      type: local
      request:
        maxRetries: 0
  models:
    - modelName: chat
      provider: local
      modelId: chat
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown field `request`"));
    }

    #[test]
    fn load_profile_resolves_models_and_fixed_resources_from_one_path() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("resource/prompts")).unwrap();
        std::fs::write(
            root.path().join("resource/prompts/System.md"),
            "You are a coding agent.",
        )
        .unwrap();
        std::fs::write(
            root.path().join("profile.yaml"),
            r#"
name: coder
description: coding agent
policyMode: confirm
model:
  defaultModelId: deepseek-v4-pro
  providers:
    deepseek:
      type: deepseek
  models:
    - modelName: deepseek-v4-pro
      provider: deepseek
      modelId: deepseek-v4-pro
"#,
        )
        .unwrap();

        let loaded = load_profile(root.path()).unwrap();
        assert!(loaded.root.is_absolute());
        assert_eq!(loaded.config.name, "coder");
        assert_eq!(loaded.models.default_model_id, "deepseek-v4-pro");
    }
}
