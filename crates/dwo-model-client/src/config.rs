use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Duration;

use indexmap::IndexMap;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::ModelClientError;

const BUILTIN_MODEL_YAMLS: &[(&str, &str)] = &[
    (
        "deepseek",
        include_str!("../resources/models/deepseek.yaml"),
    ),
    ("grok", include_str!("../resources/models/grok.yaml")),
    ("openai", include_str!("../resources/models/openai.yaml")),
    ("qwen", include_str!("../resources/models/qwen.yaml")),
    ("zhipu", include_str!("../resources/models/zhipu.yaml")),
];
const RESERVED_BODY_FIELDS: &[&str] = &[
    "model",
    "input",
    "instructions",
    "previous_response_id",
    "tools",
    "stream",
    "stream_options",
    "max_tokens",
    "max_completion_tokens",
    "max_output_tokens",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Off,
    Auto,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ReasoningEffort {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Auto => "Auto",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::XHigh => "XHigh",
            Self::Max => "Max",
        }
    }

    pub(crate) const fn wire_value(self) -> Option<&'static str> {
        match self {
            Self::Off => Some("none"),
            Self::Auto => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::XHigh => Some("xhigh"),
            Self::Max => Some("max"),
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "nonthink" => Some(Self::Off),
            "auto" => Some(Self::Auto),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningSummary {
    Auto,
}

impl ReasoningSummary {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestPolicy {
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_stream_idle_timeout_ms")]
    pub stream_idle_timeout_ms: u64,
}

impl Default for RequestPolicy {
    fn default() -> Self {
        Self {
            request_timeout_ms: default_request_timeout_ms(),
            stream_idle_timeout_ms: default_stream_idle_timeout_ms(),
        }
    }
}

impl RequestPolicy {
    pub(crate) fn request_timeout(self) -> Duration {
        Duration::from_millis(self.request_timeout_ms)
    }

    pub(crate) fn stream_idle_timeout(self) -> Duration {
        Duration::from_millis(self.stream_idle_timeout_ms)
    }

    fn validate(self, source: &str) -> Result<(), ModelClientError> {
        if self.request_timeout_ms == 0 || self.stream_idle_timeout_ms == 0 {
            return Err(ModelClientError::config(format!(
                "{source} timeout values must be positive"
            )));
        }
        Ok(())
    }
}

fn default_request_timeout_ms() -> u64 {
    300_000
}

fn default_stream_idle_timeout_ms() -> u64 {
    300_000
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCatalog {
    pub families: BTreeMap<String, ModelFamilySpec>,
}

impl ModelCatalog {
    pub fn builtin() -> Result<Self, ModelClientError> {
        let mut families = BTreeMap::new();
        for (family, source) in BUILTIN_MODEL_YAMLS {
            let spec: ModelFamilySpec = serde_yaml::from_str(source).map_err(|error| {
                ModelClientError::config(format!("parse built-in model family {family}: {error}"))
            })?;
            families.insert((*family).to_string(), spec);
        }
        let catalog = Self { families };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn from_yaml(source: &str) -> Result<Self, ModelClientError> {
        let catalog: Self = serde_yaml::from_str(source)
            .map_err(|error| ModelClientError::config(error.to_string()))?;
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn merge_model_directory(
        &mut self,
        directory: impl AsRef<Path>,
    ) -> Result<(), ModelClientError> {
        let directory = directory.as_ref();
        if !directory.exists() {
            return Ok(());
        }
        if !directory.is_dir() {
            return Err(ModelClientError::config(format!(
                "model catalog path is not a directory: {}",
                directory.display()
            )));
        }

        let mut paths = std::fs::read_dir(directory)
            .map_err(|error| ModelClientError::config(format!("read model catalog: {error}")))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ModelClientError::config(format!("read model catalog: {error}")))?;
        paths.sort();

        for path in paths {
            if !path.is_file()
                || !path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| matches!(extension, "yaml" | "yml"))
            {
                continue;
            }
            let family = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| {
                    ModelClientError::config(format!(
                        "model catalog filename is not valid UTF-8: {}",
                        path.display()
                    ))
                })?
                .to_string();
            validate_identifier(&family, "model family")?;
            let source = std::fs::read_to_string(&path).map_err(|error| {
                ModelClientError::config(format!("read model catalog {}: {error}", path.display()))
            })?;
            let spec: ModelFamilySpec = serde_yaml::from_str(&source).map_err(|error| {
                ModelClientError::config(format!("parse model catalog {}: {error}", path.display()))
            })?;
            self.merge_family(family, spec)?;
        }

        self.validate()
    }

    pub fn merge_family(
        &mut self,
        family: String,
        spec: ModelFamilySpec,
    ) -> Result<(), ModelClientError> {
        validate_identifier(&family, "model family")?;
        spec.validate(&family)?;
        if let Some(existing) = self.families.get_mut(&family) {
            if spec.base_url.is_some() {
                existing.base_url = spec.base_url;
            }
            existing.models.extend(spec.models);
        } else {
            self.families.insert(family, spec);
        }
        self.validate()
    }

    pub fn validate(&self) -> Result<(), ModelClientError> {
        if self.families.is_empty() {
            return Err(ModelClientError::config(
                "model catalog families must not be empty",
            ));
        }
        for (family, spec) in &self.families {
            validate_identifier(family, "model family")?;
            spec.validate(family)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelFamilySpec {
    #[serde(default)]
    pub base_url: Option<String>,
    pub models: BTreeMap<String, ModelSpec>,
}

impl ModelFamilySpec {
    fn validate(&self, family: &str) -> Result<(), ModelClientError> {
        if let Some(base_url) = &self.base_url {
            validate_base_url(base_url, &format!("model family {family} baseUrl"))?;
        }
        if self.models.is_empty() {
            return Err(ModelClientError::config(format!(
                "model family {family} models must not be empty"
            )));
        }
        for (model_id, model) in &self.models {
            validate_identifier(model_id, "catalog model id")?;
            model.validate(family, model_id)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSpec {
    pub context_window_tokens: u64,
    pub max_output_tokens: u32,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub extra_body: Map<String, Value>,
    #[serde(default)]
    pub hosted_tools: Vec<String>,
    #[serde(default)]
    pub reasoning_efforts: Vec<ReasoningEffort>,
    #[serde(default = "default_reasoning_effort")]
    pub default_reasoning_effort: ReasoningEffort,
    #[serde(default)]
    pub reasoning_summary: Option<ReasoningSummary>,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
}

impl ModelSpec {
    fn validate(&self, family: &str, model_id: &str) -> Result<(), ModelClientError> {
        let source = format!("model catalog {family}/{model_id}");
        let _ =
            available_input_tokens(self.context_window_tokens, self.max_output_tokens, &source)?;
        validate_body(&self.extra_body, &format!("{source} extraBody"))?;
        let mut hosted_tools = HashSet::new();
        for tool_type in &self.hosted_tools {
            validate_identifier(tool_type, &format!("{source} hosted tool type"))?;
            if !hosted_tools.insert(tool_type) {
                return Err(ModelClientError::config(format!(
                    "{source} repeats hosted tool type {tool_type}"
                )));
            }
        }
        validate_reasoning(
            &self.reasoning_efforts,
            self.default_reasoning_effort,
            &source,
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCapabilities {
    #[serde(default)]
    pub image_input: bool,
    #[serde(default)]
    pub tool_calls: bool,
}

fn default_reasoning_effort() -> ReasoningEffort {
    ReasoningEffort::Auto
}

fn default_compaction_trigger_ratio() -> f64 {
    0.8
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DefaultModelConfig {
    pub model: String,
    #[serde(default)]
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentModelConfig {
    pub default: DefaultModelConfig,
    #[serde(default = "default_compaction_trigger_ratio")]
    pub compaction_trigger_ratio: f64,
    pub providers: IndexMap<String, AgentProviderConfig>,
}

impl AgentModelConfig {
    pub fn from_yaml(source: &str) -> Result<Self, ModelClientError> {
        let config: Self = serde_yaml::from_str(source)
            .map_err(|error| ModelClientError::config(error.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ModelClientError> {
        let (default_provider, default_model_id) = split_model_ref(&self.default.model)?;
        if let Some(reasoning) = &self.default.reasoning {
            validate_identifier(reasoning, "model.default.reasoning")?;
        }
        validate_compaction_trigger_ratio(
            self.compaction_trigger_ratio,
            "model.compactionTriggerRatio",
        )?;
        if self.providers.is_empty() {
            return Err(ModelClientError::config(
                "model.providers must not be empty",
            ));
        }
        for (provider_id, provider) in &self.providers {
            validate_identifier(provider_id, "provider id")?;
            provider.validate(provider_id)?;
        }
        if !self.providers.contains_key(default_provider) {
            return Err(ModelClientError::config(format!(
                "model.default.model references unknown provider {default_provider}"
            )));
        }
        validate_identifier(default_model_id, "default model id")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentProviderConfig {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub request: RequestPolicy,
    #[serde(default)]
    pub extra_body: Map<String, Value>,
    #[serde(default)]
    pub models: Option<IndexMap<String, AgentModelEntry>>,
}

impl AgentProviderConfig {
    fn validate(&self, provider_id: &str) -> Result<(), ModelClientError> {
        if let Some(base_url) = &self.base_url {
            validate_base_url(base_url, &format!("provider {provider_id} baseUrl"))?;
        }
        validate_optional_string(
            &self.api_key_env,
            &format!("provider {provider_id} apiKeyEnv"),
        )?;
        validate_optional_string(&self.api_key, &format!("provider {provider_id} apiKey"))?;
        self.request
            .validate(&format!("provider {provider_id} request"))?;
        validate_body(
            &self.extra_body,
            &format!("provider {provider_id} extraBody"),
        )?;
        if self.models.as_ref().is_some_and(IndexMap::is_empty) {
            return Err(ModelClientError::config(format!(
                "provider {provider_id} models must not be empty when configured"
            )));
        }
        if let Some(models) = &self.models {
            for (model_name, model) in models {
                validate_identifier(model_name, "model display name")?;
                model.validate(provider_id, model_name)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentModelEntry {
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub context_window_tokens: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub default_reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub capabilities: Option<ModelCapabilities>,
    #[serde(default)]
    pub reasoning_efforts: Option<Vec<ReasoningEffort>>,
    #[serde(default)]
    pub reasoning_summary: Option<ReasoningSummary>,
    #[serde(default)]
    pub hosted_tools: Option<Vec<String>>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub compaction_trigger_ratio: Option<f64>,
    #[serde(default)]
    pub extra_body: Map<String, Value>,
}

impl AgentModelEntry {
    fn validate(&self, provider_id: &str, model_name: &str) -> Result<(), ModelClientError> {
        if let Some(model_id) = &self.model_id {
            validate_identifier(model_id, "modelId")?;
        }
        if let Some(profile) = &self.profile {
            split_model_ref(profile).map_err(|_| {
                ModelClientError::config(format!(
                    "provider {provider_id} model {model_name} profile must be family/modelId"
                ))
            })?;
        }
        if let Some(efforts) = &self.reasoning_efforts {
            let mut unique = HashSet::new();
            for effort in efforts {
                if !unique.insert(*effort) {
                    return Err(ModelClientError::config(format!(
                        "provider {provider_id} model {model_name} repeats reasoning effort {}",
                        effort.as_str()
                    )));
                }
            }
        }
        if let Some(hosted_tools) = &self.hosted_tools {
            let mut unique = HashSet::new();
            for name in hosted_tools {
                validate_identifier(name, "hosted tool name")?;
                if !unique.insert(name) {
                    return Err(ModelClientError::config(format!(
                        "provider {provider_id} model {model_name} repeats hosted tool {name}"
                    )));
                }
            }
        }
        if let Some(ratio) = self.compaction_trigger_ratio {
            validate_compaction_trigger_ratio(
                ratio,
                &format!("provider {provider_id} model {model_name} compactionTriggerRatio"),
            )?;
        }
        validate_body(&self.extra_body, &format!("model {model_name} extraBody"))
    }

    pub fn effective_model_id<'a>(&'a self, model_name: &'a str) -> &'a str {
        self.model_id.as_deref().unwrap_or(model_name)
    }
}

#[derive(Debug, Clone)]
pub struct ModelClientConfig {
    pub default_model: String,
    pub default_reasoning: Option<String>,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub models: IndexMap<String, ModelConfig>,
}

impl ModelClientConfig {
    pub fn resolve(
        catalog: &ModelCatalog,
        agent: &AgentModelConfig,
    ) -> Result<Self, ModelClientError> {
        catalog.validate()?;
        agent.validate()?;

        let mut providers = BTreeMap::new();
        let mut models = IndexMap::new();
        for (provider_id, agent_provider) in &agent.providers {
            let official_family = catalog.families.get(provider_id);
            let base_url = agent_provider
                .base_url
                .clone()
                .or_else(|| official_family.and_then(|family| family.base_url.clone()))
                .ok_or_else(|| {
                    ModelClientError::config(format!(
                        "custom provider {provider_id} must configure baseUrl"
                    ))
                })?;
            let provider = ProviderConfig {
                base_url,
                api_key_env: agent_provider.api_key_env.clone(),
                api_key: agent_provider.api_key.clone(),
                headers: agent_provider.headers.clone(),
                request: agent_provider.request,
                extra_body: agent_provider.extra_body.clone(),
            };
            provider.validate(provider_id)?;
            providers.insert(provider_id.clone(), provider);

            match &agent_provider.models {
                None => {
                    let family = official_family.ok_or_else(|| {
                        ModelClientError::config(format!(
                            "custom provider {provider_id} must configure models"
                        ))
                    })?;
                    for (model_id, spec) in &family.models {
                        let id = model_ref(provider_id, model_id);
                        models.insert(
                            id,
                            resolved_model(
                                provider_id,
                                provider_id,
                                model_id,
                                model_id,
                                spec,
                                None,
                                agent.compaction_trigger_ratio,
                                &providers,
                            )?,
                        );
                    }
                }
                Some(entries) => {
                    let mut provider_model_ids = HashSet::new();
                    for (model_name, entry) in entries {
                        let model_id = entry.effective_model_id(model_name);
                        if !provider_model_ids.insert(model_id) {
                            return Err(ModelClientError::config(format!(
                                "provider {provider_id} has duplicate modelId {model_id}"
                            )));
                        }
                        let (family, profile_model_id) = match &entry.profile {
                            Some(profile) => split_model_ref(profile)?,
                            None if official_family.is_some() => (provider_id.as_str(), model_id),
                            None => {
                                return Err(ModelClientError::config(format!(
                                    "custom provider {provider_id} model {model_name} must configure profile"
                                )));
                            }
                        };
                        let spec = catalog
                            .families
                            .get(family)
                            .and_then(|family| family.models.get(profile_model_id))
                            .ok_or_else(|| {
                                ModelClientError::config(format!(
                                    "provider {provider_id} model {model_name} references unknown profile {family}/{profile_model_id}"
                                ))
                            })?;
                        let id = model_ref(provider_id, model_id);
                        models.insert(
                            id,
                            resolved_model(
                                provider_id,
                                family,
                                model_name,
                                model_id,
                                spec,
                                Some(entry),
                                agent.compaction_trigger_ratio,
                                &providers,
                            )?,
                        );
                    }
                }
            }
        }

        let default_model = agent.default.model.clone();
        let default_config = models.get(&default_model).ok_or_else(|| {
            ModelClientError::config(format!(
                "model.default.model references unavailable model {default_model}"
            ))
        })?;
        let default_reasoning = agent
            .default
            .reasoning
            .as_deref()
            .map(|reasoning| {
                let effort = ReasoningEffort::parse(reasoning).ok_or_else(|| {
                    ModelClientError::config(format!("unknown reasoning effort {reasoning}"))
                })?;
                if !default_config.reasoning_efforts.contains(&effort) {
                    return Err(ModelClientError::config(format!(
                        "default model {default_model} does not support reasoning effort {reasoning}"
                    )));
                }
                Ok(effort.as_str().to_string())
            })
            .transpose()?;

        Ok(Self {
            default_model,
            default_reasoning,
            providers,
            models,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub request: RequestPolicy,
    pub extra_body: Map<String, Value>,
}

impl ProviderConfig {
    fn validate(&self, id: &str) -> Result<(), ModelClientError> {
        validate_base_url(&self.base_url, &format!("provider {id} baseUrl"))?;
        self.request.validate(&format!("provider {id} request"))?;
        validate_body(&self.extra_body, &format!("provider {id} extraBody"))
    }

    pub(crate) fn resolve_api_key(&self) -> Result<Option<String>, ModelClientError> {
        if let Some(key) = self
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
        {
            return Ok(Some(key.to_string()));
        }
        let Some(name) = self
            .api_key_env
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            return Ok(None);
        };
        std::env::var(name)
            .ok()
            .filter(|key| !key.is_empty())
            .map(Some)
            .ok_or_else(|| ModelClientError::MissingApiKey(name.to_string()))
    }

    pub(crate) fn responses_endpoint(&self) -> Result<Url, ModelClientError> {
        let mut base = self.base_url.trim().trim_end_matches('/').to_string();
        base.push_str("/responses");
        Url::parse(&base).map_err(|error| ModelClientError::config(error.to_string()))
    }
}

#[derive(Debug, Clone)]
pub struct ModelConfig {
    pub provider: String,
    pub family: String,
    pub model_name: String,
    pub model_id: String,
    pub context_window_tokens: u64,
    pub max_output_tokens: u32,
    pub compaction_trigger_ratio: f64,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub extra_body: Map<String, Value>,
    pub hosted_tools: Vec<Value>,
    pub reasoning_efforts: Vec<ReasoningEffort>,
    pub default_reasoning_effort: ReasoningEffort,
    pub reasoning_summary: Option<ReasoningSummary>,
    pub capabilities: ModelCapabilities,
}

impl ModelConfig {
    fn validate(
        &self,
        id: &str,
        providers: &BTreeMap<String, ProviderConfig>,
    ) -> Result<(), ModelClientError> {
        if !providers.contains_key(&self.provider) {
            return Err(ModelClientError::config(format!(
                "model {id} references unknown provider {}",
                self.provider
            )));
        }
        validate_identifier(&self.model_id, "modelId")?;
        let _ = self.max_input_tokens()?;
        validate_compaction_trigger_ratio(
            self.compaction_trigger_ratio,
            &format!("model {id} compactionTriggerRatio"),
        )?;
        validate_body(&self.extra_body, &format!("model {id} extraBody"))?;
        validate_reasoning(
            &self.reasoning_efforts,
            self.default_reasoning_effort,
            &format!("model {id}"),
        )
    }

    pub fn max_input_tokens(&self) -> Result<u64, ModelClientError> {
        available_input_tokens(
            self.context_window_tokens,
            self.max_output_tokens,
            &format!("model {}", self.model_id),
        )
    }

    pub fn context_owner_id(&self) -> String {
        format!("{}/{}", self.provider, self.family)
    }
}

#[allow(clippy::too_many_arguments)]
fn resolved_model(
    provider_id: &str,
    family: &str,
    model_name: &str,
    model_id: &str,
    spec: &ModelSpec,
    entry: Option<&AgentModelEntry>,
    compaction_trigger_ratio: f64,
    providers: &BTreeMap<String, ProviderConfig>,
) -> Result<ModelConfig, ModelClientError> {
    let hosted_tools = match entry.and_then(|entry| entry.hosted_tools.as_ref()) {
        Some(tool_types) => tool_types
            .iter()
            .map(|tool_type| {
                if !spec.hosted_tools.contains(tool_type) {
                    return Err(ModelClientError::config(format!(
                        "model {provider_id}/{model_id} selects unknown hosted tool {tool_type}"
                    )));
                }
                Ok(json!({"type": tool_type}))
            })
            .collect::<Result<Vec<_>, ModelClientError>>()?,
        None => spec
            .hosted_tools
            .iter()
            .map(|tool_type| json!({"type": tool_type}))
            .collect(),
    };
    let reasoning_efforts = entry
        .and_then(|entry| entry.reasoning_efforts.clone())
        .unwrap_or_else(|| spec.reasoning_efforts.clone());
    let default_reasoning_effort = entry
        .and_then(|entry| entry.default_reasoning_effort)
        .unwrap_or(spec.default_reasoning_effort);
    let reasoning_summary = entry
        .and_then(|entry| entry.reasoning_summary)
        .or(spec.reasoning_summary);
    let mut extra_body = spec.extra_body.clone();
    if let Some(entry) = entry {
        merge_map(&mut extra_body, &entry.extra_body);
    }
    let model = ModelConfig {
        provider: provider_id.to_string(),
        family: family.to_string(),
        model_name: model_name.to_string(),
        model_id: model_id.to_string(),
        context_window_tokens: entry
            .and_then(|entry| entry.context_window_tokens)
            .unwrap_or(spec.context_window_tokens),
        max_output_tokens: entry
            .and_then(|entry| entry.max_output_tokens)
            .unwrap_or(spec.max_output_tokens),
        compaction_trigger_ratio: entry
            .and_then(|entry| entry.compaction_trigger_ratio)
            .unwrap_or(compaction_trigger_ratio),
        temperature: entry
            .and_then(|entry| entry.temperature)
            .or(spec.temperature),
        top_p: entry.and_then(|entry| entry.top_p).or(spec.top_p),
        extra_body,
        hosted_tools,
        reasoning_efforts,
        default_reasoning_effort,
        reasoning_summary,
        capabilities: entry
            .and_then(|entry| entry.capabilities)
            .unwrap_or(spec.capabilities),
    };
    model.validate(&model_ref(provider_id, model_id), providers)?;
    Ok(model)
}

fn model_ref(provider: &str, model_id: &str) -> String {
    format!("{provider}/{model_id}")
}

fn split_model_ref(value: &str) -> Result<(&str, &str), ModelClientError> {
    let (provider, model_id) = value.split_once('/').ok_or_else(|| {
        ModelClientError::config(format!("model reference {value} must be provider/modelId"))
    })?;
    validate_identifier(provider, "model reference provider")?;
    validate_identifier(model_id, "model reference modelId")?;
    Ok((provider, model_id))
}

fn merge_map(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    for (key, value) in source {
        match (target.get_mut(key), value) {
            (Some(Value::Object(target)), Value::Object(source)) => merge_map(target, source),
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

fn available_input_tokens(
    context_window_tokens: u64,
    max_output_tokens: u32,
    source: &str,
) -> Result<u64, ModelClientError> {
    if context_window_tokens == 0 {
        return Err(ModelClientError::config(format!(
            "{source} contextWindowTokens must be positive"
        )));
    }
    if max_output_tokens == 0 {
        return Err(ModelClientError::config(format!(
            "{source} maxOutputTokens must be positive"
        )));
    }
    context_window_tokens
        .checked_sub(u64::from(max_output_tokens))
        .filter(|tokens| *tokens > 0)
        .ok_or_else(|| {
            ModelClientError::config(format!(
                "{source} must leave input capacity after max output"
            ))
        })
}

fn validate_reasoning(
    efforts: &[ReasoningEffort],
    default_effort: ReasoningEffort,
    source: &str,
) -> Result<(), ModelClientError> {
    let mut unique = HashSet::new();
    for effort in efforts {
        if !unique.insert(*effort) {
            return Err(ModelClientError::config(format!(
                "{source} repeats reasoning effort {}",
                effort.as_str()
            )));
        }
    }
    if !efforts.is_empty() && !efforts.contains(&default_effort) {
        return Err(ModelClientError::config(format!(
            "{source} defaultReasoningEffort {} is not supported",
            default_effort.as_str()
        )));
    }
    Ok(())
}

fn validate_compaction_trigger_ratio(value: f64, source: &str) -> Result<(), ModelClientError> {
    if !value.is_finite() || value <= 0.0 || value > 1.0 {
        return Err(ModelClientError::config(format!(
            "{source} must be in (0, 1]"
        )));
    }
    Ok(())
}

fn validate_base_url(value: &str, source: &str) -> Result<(), ModelClientError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ModelClientError::config(format!(
            "{source} must not be empty"
        )));
    }
    let url = Url::parse(value)
        .map_err(|error| ModelClientError::config(format!("{source} is invalid: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ModelClientError::config(format!(
            "{source} must use http or https"
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ModelClientError::config(format!(
            "{source} must not contain a query or fragment"
        )));
    }
    Ok(())
}

fn validate_identifier(value: &str, kind: &str) -> Result<(), ModelClientError> {
    if value.trim().is_empty() {
        return Err(ModelClientError::config(format!(
            "{kind} must not be empty"
        )));
    }
    if value != value.trim() {
        return Err(ModelClientError::config(format!(
            "{kind} must not contain surrounding whitespace"
        )));
    }
    Ok(())
}

fn validate_optional_string(value: &Option<String>, source: &str) -> Result<(), ModelClientError> {
    if value.as_ref().is_some_and(|value| value.trim().is_empty()) {
        return Err(ModelClientError::config(format!(
            "{source} must not be empty"
        )));
    }
    Ok(())
}

fn validate_body(body: &Map<String, Value>, source: &str) -> Result<(), ModelClientError> {
    let reserved: HashSet<&str> = RESERVED_BODY_FIELDS.iter().copied().collect();
    if let Some(field) = body.keys().find(|field| reserved.contains(field.as_str())) {
        return Err(ModelClientError::config(format!(
            "{source} must not override reserved field {field}"
        )));
    }
    Ok(())
}
