use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dwo_context::SystemPromptBuilder;
use dwo_model_client::{AgentModelConfig, ModelCatalog, ModelClientConfig};
use dwo_tools::SessionMode;
use serde::{Deserialize, Serialize};

use crate::AgentServiceError;
use crate::record::DEFAULT_MAX_MODEL_STEPS;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentProfileConfig {
    pub policy_mode: SessionMode,
    #[serde(default = "default_max_model_steps")]
    pub max_model_steps: usize,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub external_skills_dirs: Vec<PathBuf>,
    pub model: AgentModelConfig,
    #[serde(default)]
    pub channels: BTreeMap<String, serde_yaml::Value>,
    #[serde(default)]
    pub websocket: WebsocketConfig,
    #[serde(default)]
    pub automation: serde_yaml::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebsocketConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_websocket_bind")]
    pub bind: String,
    #[serde(default = "default_websocket_port")]
    pub port: u16,
}

impl Default for WebsocketConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_websocket_bind(),
            port: default_websocket_port(),
        }
    }
}

fn default_websocket_bind() -> String {
    "127.0.0.1".to_string()
}

fn default_websocket_port() -> u16 {
    8787
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LoggingConfig {
    #[serde(default)]
    pub level: LogLevel,
    #[serde(default = "default_retention_days")]
    pub retention_days: usize,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            retention_days: default_retention_days(),
        }
    }
}

fn default_retention_days() -> usize {
    14
}

fn default_max_model_steps() -> usize {
    DEFAULT_MAX_MODEL_STEPS
}

#[derive(Debug, Clone)]
pub struct LoadedAgentProfile {
    pub root: PathBuf,
    pub config: AgentProfileConfig,
    pub models: ModelClientConfig,
    pub external_skill_dirs: Vec<PathBuf>,
}

impl LoadedAgentProfile {
    pub fn load(root: impl AsRef<Path>) -> Result<Self, AgentServiceError> {
        let root = std::fs::canonicalize(root.as_ref()).map_err(anyhow::Error::from)?;
        let config = AgentProfileConfig::load(&root)?;
        let mut catalog = ModelCatalog::builtin()
            .map_err(|error| AgentServiceError::InvalidConfig(error.to_string()))?;
        catalog
            .merge_provider_directory(root.join("resource/providers"))
            .map_err(|error| AgentServiceError::InvalidConfig(error.to_string()))?;
        let models = config.resolve_models(&catalog)?;
        SystemPromptBuilder::new(Some(root.clone()), root.clone())
            .build_initial()
            .map_err(anyhow::Error::from)?;
        let external_skill_dirs = config
            .external_skills_dirs
            .iter()
            .map(|dir| {
                if dir.is_absolute() {
                    dir.clone()
                } else {
                    root.join(dir)
                }
            })
            .collect();
        Ok(Self {
            root,
            config,
            models,
            external_skill_dirs,
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
        if !(1..=365).contains(&self.logging.retention_days) {
            return Err(AgentServiceError::InvalidConfig(
                "logging.retentionDays must be between 1 and 365".to_string(),
            ));
        }
        if self.max_model_steps != 0 && !(5..=200).contains(&self.max_model_steps) {
            return Err(AgentServiceError::InvalidConfig(
                "maxModelSteps must be 0 (unlimited) or between 5 and 200".to_string(),
            ));
        }
        self.websocket
            .validate()
            .map_err(AgentServiceError::InvalidConfig)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_resolves_one_provider_for_multiple_models() {
        let profile = AgentProfileConfig::from_yaml(
            r#"
policyMode: confirm
channels:
  weixin:
    enabled: true
    replayTurns: 5
    markdownFilter: true
model:
  defaultModelName: deepseek-v4-pro
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

        assert_eq!(profile.policy_mode, SessionMode::Confirm);
        assert_eq!(profile.logging, LoggingConfig::default());
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
    fn profile_parses_logging_configuration() {
        let profile = AgentProfileConfig::from_yaml(
            r#"
policyMode: confirm
logging:
  level: debug
  retentionDays: 30
model:
  defaultModelName: chat
  providers:
    local:
      type: local
  models:
    - modelName: chat
      provider: local
      modelId: chat
"#,
        )
        .unwrap();

        assert_eq!(profile.logging.level, LogLevel::Debug);
        assert_eq!(profile.logging.retention_days, 30);
    }

    #[test]
    fn profile_rejects_invalid_log_retention() {
        let error = AgentProfileConfig::from_yaml(
            r#"
policyMode: confirm
logging:
  retentionDays: 0
model:
  defaultModelName: chat
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

        assert!(error.to_string().contains("logging.retentionDays"));
    }

    #[test]
    fn profile_max_model_steps_defaults_and_accepts_bounds() {
        let default = AgentProfileConfig::from_yaml(
            r#"
policyMode: confirm
model:
  defaultModelName: chat
  providers:
    local:
      type: local
  models:
    - modelName: chat
      provider: local
      modelId: chat
"#,
        )
        .unwrap();
        assert_eq!(default.max_model_steps, 100);

        for value in [0, 5, 200] {
            let profile = AgentProfileConfig::from_yaml(&format!(
                r#"
policyMode: confirm
maxModelSteps: {value}
model:
  defaultModelName: chat
  providers:
    local:
      type: local
  models:
    - modelName: chat
      provider: local
      modelId: chat
"#
            ))
            .unwrap();
            assert_eq!(profile.max_model_steps, value);
        }
    }

    #[test]
    fn profile_rejects_out_of_range_max_model_steps() {
        for value in [4, 201] {
            let error = AgentProfileConfig::from_yaml(&format!(
                r#"
policyMode: confirm
maxModelSteps: {value}
model:
  defaultModelName: chat
  providers:
    local:
      type: local
  models:
    - modelName: chat
      provider: local
      modelId: chat
"#
            ))
            .unwrap_err();
            assert!(error.to_string().contains("maxModelSteps"));
        }
    }

    #[test]
    fn profile_reports_invalid_websocket_fields() {
        for (websocket, expected) in [
            (
                "enabled: true\n  bind: localhost\n  port: 8787",
                "websocket.bind must be an IP address",
            ),
            (
                "enabled: true\n  bind: 127.0.0.1\n  port: 0",
                "websocket.port must be greater than 0",
            ),
        ] {
            let error = AgentProfileConfig::from_yaml(&format!(
                r#"
policyMode: confirm
websocket:
  {websocket}
model:
  defaultModelName: chat
  providers:
    local:
      type: local
  models:
    - modelName: chat
      provider: local
      modelId: chat
"#
            ))
            .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn profile_rejects_provider_transport_configuration() {
        let error = AgentProfileConfig::from_yaml(
            r#"
policyMode: confirm
model:
  defaultModelName: chat
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
policyMode: confirm
model:
  defaultModelName: deepseek-v4-pro
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
        assert_eq!(loaded.models.default_model_name, "deepseek-v4-pro");
    }

    #[test]
    fn load_profile_merges_custom_provider_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("resource/prompts")).unwrap();
        std::fs::create_dir_all(root.path().join("resource/providers")).unwrap();
        std::fs::write(
            root.path().join("resource/prompts/System.md"),
            "You are a coding agent.",
        )
        .unwrap();
        std::fs::write(
            root.path().join("resource/providers/newapi.yaml"),
            r#"
endpoint: https://gateway.example.com/v1/responses
models:
  custom-model:
    contextWindowTokens: 100000
    maxOutputTokens: 4096
    capabilities:
      imageInput: true
      toolCalls: true
    defaultReasoningMode: medium
    reasoning:
      medium:
        reasoning:
          effort: medium
"#,
        )
        .unwrap();
        std::fs::write(
            root.path().join("profile.yaml"),
            r#"
policyMode: confirm
model:
  defaultModelName: custom
  providers:
    relay:
      type: newapi
      apiKeyEnv: NEW_API_KEY
  models:
    - modelName: custom
      provider: relay
      modelId: custom-model
"#,
        )
        .unwrap();

        let loaded = load_profile(root.path()).unwrap();
        assert_eq!(loaded.models.default_model_name, "custom");
        assert!(loaded.models.models["custom"].capabilities.image_input);
        assert_eq!(
            loaded.models.providers["relay"].endpoint,
            "https://gateway.example.com/v1/responses"
        );
    }

    #[test]
    fn profile_parses_external_skills_dirs_and_resolves_relative_paths() {
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
policyMode: confirm
externalSkillsDirs:
  - C:/Users/example/shared-skills
  - team-skills
model:
  defaultModelName: deepseek-v4-pro
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
        assert_eq!(loaded.external_skill_dirs.len(), 2);
        assert_eq!(
            loaded.external_skill_dirs[0],
            PathBuf::from("C:/Users/example/shared-skills")
        );
        assert!(loaded.external_skill_dirs[1].is_absolute());
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        assert!(loaded.external_skill_dirs[1].starts_with(&canonical_root));
        assert!(loaded.external_skill_dirs[1].ends_with("team-skills"));
    }
}

impl WebsocketConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.port == 0 {
            return Err("websocket.port must be greater than 0".to_string());
        }
        self.bind
            .parse::<std::net::IpAddr>()
            .map_err(|_| "websocket.bind must be an IP address".to_string())?;
        Ok(())
    }
}
