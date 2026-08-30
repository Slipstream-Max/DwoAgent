use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use dwo_context::{ExternalRuleFile, SystemPromptBuilder};
use dwo_model_client::{AgentModelConfig, ModelCatalog, ModelClientConfig};
use dwo_tools::SessionMode;
use serde::{Deserialize, Serialize};

use crate::SessionServiceError;
use crate::session_record::DEFAULT_MAX_MODEL_STEPS;

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
    #[serde(default)]
    pub external_rule_files: Vec<PathBuf>,
    pub model: AgentModelConfig,
    #[serde(default)]
    pub channels: BTreeMap<String, serde_yaml::Value>,
    #[serde(default)]
    pub websocket: WebsocketConfig,
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
    pub external_rule_files: Vec<ExternalRuleFile>,
}

impl LoadedAgentProfile {
    pub fn load(root: impl AsRef<Path>) -> Result<Self, SessionServiceError> {
        let root = std::fs::canonicalize(root.as_ref()).map_err(anyhow::Error::from)?;
        let config = AgentProfileConfig::load(&root)?;
        let mut catalog = ModelCatalog::builtin()
            .map_err(|error| SessionServiceError::InvalidConfig(error.to_string()))?;
        catalog
            .merge_model_directory(root.join("resource/models"))
            .map_err(|error| SessionServiceError::InvalidConfig(error.to_string()))?;
        let models = config.resolve_models(&catalog)?;
        let external_skill_dirs: Vec<PathBuf> = config
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
        let external_rule_files: Vec<ExternalRuleFile> = config
            .external_rule_files
            .iter()
            .map(|path| ExternalRuleFile::new(resolve_profile_path(&root, path), root.clone()))
            .collect();
        SystemPromptBuilder::new(Some(root.clone()), root.clone())
            .with_external_skill_dirs(Arc::new(RwLock::new(external_skill_dirs.clone())))
            .with_external_rule_files(
                Arc::new(RwLock::new(external_rule_files.clone())),
                Arc::new(RwLock::new(Vec::new())),
            )
            .build_initial()
            .map_err(anyhow::Error::from)?;
        Ok(Self {
            root,
            config,
            models,
            external_skill_dirs,
            external_rule_files,
        })
    }
}

fn resolve_profile_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

pub fn load_profile(root: impl AsRef<Path>) -> Result<LoadedAgentProfile, SessionServiceError> {
    LoadedAgentProfile::load(root)
}

impl AgentProfileConfig {
    pub fn from_yaml(source: &str) -> Result<Self, SessionServiceError> {
        let profile: Self = serde_yaml::from_str(source)
            .map_err(|error| SessionServiceError::InvalidConfig(error.to_string()))?;
        profile.validate()?;
        Ok(profile)
    }

    pub fn load(root: impl AsRef<Path>) -> Result<Self, SessionServiceError> {
        let path = root.as_ref().join("profile.yaml");
        let source = std::fs::read_to_string(&path).map_err(anyhow::Error::from)?;
        Self::from_yaml(&source)
    }

    pub fn validate(&self) -> Result<(), SessionServiceError> {
        if !(1..=365).contains(&self.logging.retention_days) {
            return Err(SessionServiceError::InvalidConfig(
                "logging.retentionDays must be between 1 and 365".to_string(),
            ));
        }
        if self.max_model_steps != 0 && !(5..=200).contains(&self.max_model_steps) {
            return Err(SessionServiceError::InvalidConfig(
                "maxModelSteps must be 0 (unlimited) or between 5 and 200".to_string(),
            ));
        }
        self.websocket
            .validate()
            .map_err(SessionServiceError::InvalidConfig)?;
        self.model
            .validate()
            .map_err(|error| SessionServiceError::InvalidConfig(error.to_string()))
    }

    pub fn resolve_models(
        &self,
        catalog: &ModelCatalog,
    ) -> Result<ModelClientConfig, SessionServiceError> {
        ModelClientConfig::resolve(catalog, &self.model)
            .map_err(|error| SessionServiceError::InvalidConfig(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_yaml(model: &str) -> String {
        format!(
            r#"
policyMode: confirm
model:
  default:
    model: {model}
  providers:
    deepseek:
"#
        )
    }

    #[test]
    fn profile_rejects_invalid_log_retention() {
        let error = AgentProfileConfig::from_yaml(
            r#"
policyMode: confirm
logging:
  retentionDays: 0
model:
  default:
    model: deepseek/deepseek-v4-pro
  providers:
    deepseek:
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("logging.retentionDays"));
    }

    #[test]
    fn profile_max_model_steps_defaults_and_accepts_bounds() {
        let default =
            AgentProfileConfig::from_yaml(&profile_yaml("deepseek/deepseek-v4-pro")).unwrap();
        assert_eq!(default.max_model_steps, 100);

        for value in [0, 5, 200] {
            let source = profile_yaml("deepseek/deepseek-v4-pro").replacen(
                "policyMode: confirm",
                &format!("policyMode: confirm\nmaxModelSteps: {value}"),
                1,
            );
            let profile = AgentProfileConfig::from_yaml(&source).unwrap();
            assert_eq!(profile.max_model_steps, value);
        }
    }

    #[test]
    fn profile_rejects_out_of_range_max_model_steps() {
        for value in [4, 201] {
            let source = profile_yaml("deepseek/deepseek-v4-pro").replacen(
                "policyMode: confirm",
                &format!("policyMode: confirm\nmaxModelSteps: {value}"),
                1,
            );
            let error = AgentProfileConfig::from_yaml(&source).unwrap_err();
            assert!(error.to_string().contains("maxModelSteps"));
        }
    }

    #[test]
    fn load_profile_merges_custom_model_families() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("resource/prompts")).unwrap();
        std::fs::create_dir_all(root.path().join("resource/models")).unwrap();
        std::fs::write(
            root.path().join("resource/prompts/System.md"),
            "You are a coding agent.",
        )
        .unwrap();
        std::fs::write(
            root.path().join("resource/models/minimax.yaml"),
            r#"
models:
  minimax-m2.5:
    contextWindowTokens: 200000
    maxOutputTokens: 32000
"#,
        )
        .unwrap();
        std::fs::write(
            root.path().join("profile.yaml"),
            r#"
policyMode: confirm
model:
  default:
    model: gateway/minimax-m2.5
  providers:
    gateway:
      baseUrl: https://gateway.example.com/v1
      models:
        "MiniMax M2.5":
          modelId: minimax-m2.5
          profile: minimax/minimax-m2.5
"#,
        )
        .unwrap();

        let loaded = load_profile(root.path()).unwrap();
        assert_eq!(loaded.models.default_model, "gateway/minimax-m2.5");
        assert_eq!(
            loaded.models.models["gateway/minimax-m2.5"].model_name,
            "MiniMax M2.5"
        );
    }

    #[test]
    fn profile_resolves_external_skill_dirs_and_rule_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("resource/prompts")).unwrap();
        std::fs::write(
            root.path().join("resource/prompts/System.md"),
            "You are a coding agent.",
        )
        .unwrap();
        std::fs::write(root.path().join("team-rules.md"), "Team rules").unwrap();
        std::fs::write(
            root.path().join("profile.yaml"),
            r#"
policyMode: confirm
externalSkillsDirs:
  - C:/Users/example/shared-skills
  - team-skills
externalRuleFiles:
  - team-rules.md
model:
  default:
    model: deepseek/deepseek-v4-pro
  providers:
    deepseek:
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
        assert_eq!(loaded.external_rule_files.len(), 1);
        assert_eq!(
            std::fs::canonicalize(&loaded.external_rule_files[0].path).unwrap(),
            std::fs::canonicalize(root.path().join("team-rules.md")).unwrap()
        );
        assert_eq!(
            std::fs::canonicalize(&loaded.external_rule_files[0].pwd).unwrap(),
            std::fs::canonicalize(root.path()).unwrap()
        );
    }
}

#[allow(clippy::items_after_test_module)]
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
