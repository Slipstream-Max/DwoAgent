use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Duration;

use indexmap::IndexMap;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ModelClientError;

const BUILTIN_PROVIDER_YAMLS: &[(&str, &str)] = &[
    (
        "deepseek",
        include_str!("../resources/providers/deepseek.yaml"),
    ),
    ("openai", include_str!("../resources/providers/openai.yaml")),
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    #[default]
    OpenAiResponses,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestPolicy {
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_stream_idle_timeout_ms")]
    pub stream_idle_timeout_ms: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_retry_base_delay_ms")]
    pub retry_base_delay_ms: u64,
}

impl Default for RequestPolicy {
    fn default() -> Self {
        Self {
            request_timeout_ms: default_request_timeout_ms(),
            stream_idle_timeout_ms: default_stream_idle_timeout_ms(),
            max_retries: default_max_retries(),
            retry_base_delay_ms: default_retry_base_delay_ms(),
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

    pub(crate) fn retry_base_delay(self) -> Duration {
        Duration::from_millis(self.retry_base_delay_ms)
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

fn default_max_retries() -> u32 {
    4
}

fn default_retry_base_delay_ms() -> u64 {
    200
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCatalog {
    pub providers: BTreeMap<String, ProviderSpec>,
}

impl ModelCatalog {
    pub fn builtin() -> Result<Self, ModelClientError> {
        let mut providers = BTreeMap::new();
        for (provider_type, source) in BUILTIN_PROVIDER_YAMLS {
            let provider: ProviderSpec = serde_yaml::from_str(source).map_err(|error| {
                ModelClientError::config(format!(
                    "parse built-in provider {provider_type}: {error}"
                ))
            })?;
            providers.insert((*provider_type).to_string(), provider);
        }
        let catalog = Self { providers };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn from_yaml(source: &str) -> Result<Self, ModelClientError> {
        let catalog: Self = serde_yaml::from_str(source)
            .map_err(|error| ModelClientError::config(error.to_string()))?;
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn merge_provider_directory(
        &mut self,
        directory: impl AsRef<Path>,
    ) -> Result<(), ModelClientError> {
        let directory = directory.as_ref();
        if !directory.exists() {
            return Ok(());
        }
        if !directory.is_dir() {
            return Err(ModelClientError::config(format!(
                "provider catalog path is not a directory: {}",
                directory.display()
            )));
        }

        let mut paths = std::fs::read_dir(directory)
            .map_err(|error| ModelClientError::config(format!("read provider catalog: {error}")))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ModelClientError::config(format!("read provider catalog: {error}")))?;
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
            let provider_type = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| {
                    ModelClientError::config(format!(
                        "provider catalog filename is not valid UTF-8: {}",
                        path.display()
                    ))
                })?
                .to_string();
            validate_identifier(&provider_type, "provider type")?;
            if self.providers.contains_key(&provider_type) {
                return Err(ModelClientError::config(format!(
                    "provider type {provider_type} from {} conflicts with a built-in provider",
                    path.display()
                )));
            }
            let source = std::fs::read_to_string(&path).map_err(|error| {
                ModelClientError::config(format!(
                    "read provider catalog {}: {error}",
                    path.display()
                ))
            })?;
            let provider: ProviderSpec = serde_yaml::from_str(&source).map_err(|error| {
                ModelClientError::config(format!(
                    "parse provider catalog {}: {error}",
                    path.display()
                ))
            })?;
            provider.validate(&provider_type)?;
            self.providers.insert(provider_type, provider);
        }

        self.validate()
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ModelClientError> {
        let source = std::fs::read_to_string(path.as_ref()).map_err(|error| {
            ModelClientError::config(format!(
                "read model catalog {}: {error}",
                path.as_ref().display()
            ))
        })?;
        Self::from_yaml(&source)
    }

    pub fn validate(&self) -> Result<(), ModelClientError> {
        if self.providers.is_empty() {
            return Err(ModelClientError::config(
                "model catalog providers must not be empty",
            ));
        }
        for (provider_type, provider) in &self.providers {
            validate_identifier(provider_type, "provider type")?;
            provider.validate(provider_type)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderSpec {
    #[serde(default)]
    pub protocol: ProviderProtocol,
    pub endpoint: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub request: RequestPolicy,
    #[serde(default)]
    pub body: Map<String, Value>,
    pub models: BTreeMap<String, ModelSpec>,
}

impl ProviderSpec {
    fn validate(&self, provider_type: &str) -> Result<(), ModelClientError> {
        validate_endpoint(
            &self.endpoint,
            &format!("provider type {provider_type} endpoint"),
        )?;
        self.request
            .validate(&format!("provider type {provider_type} request"))?;
        validate_body(&self.body, &format!("provider type {provider_type} body"))?;
        if self.models.is_empty() {
            return Err(ModelClientError::config(format!(
                "provider type {provider_type} models must not be empty"
            )));
        }
        for (model_id, model) in &self.models {
            validate_identifier(model_id, "provider model id")?;
            model.validate(provider_type, model_id)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSpec {
    pub context_window_tokens: u64,
    pub max_output_tokens: u32,
    #[serde(default = "default_compact_threshold")]
    pub compact_threshold: f64,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub body: Map<String, Value>,
    #[serde(default)]
    pub hosted_tools: Vec<Value>,
    #[serde(default)]
    pub reasoning: IndexMap<String, Map<String, Value>>,
    #[serde(default = "default_reasoning_mode")]
    pub default_reasoning_mode: String,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
}

impl ModelSpec {
    fn validate(&self, provider_type: &str, model_id: &str) -> Result<(), ModelClientError> {
        let source = format!("model catalog {provider_type}/{model_id}");
        let _ =
            available_input_tokens(self.context_window_tokens, self.max_output_tokens, &source)?;
        validate_compact_threshold(self.compact_threshold, &source)?;
        validate_body(&self.body, &format!("{source} body"))?;
        validate_reasoning(&self.reasoning, &self.default_reasoning_mode, &source)
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

fn default_compact_threshold() -> f64 {
    0.8
}

fn default_reasoning_mode() -> String {
    "auto".to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentModelConfig {
    pub default_model_id: String,
    pub providers: BTreeMap<String, AgentProviderConfig>,
    pub models: Vec<AgentModelEntry>,
}

impl AgentModelConfig {
    pub fn from_yaml(source: &str) -> Result<Self, ModelClientError> {
        let config: Self = serde_yaml::from_str(source)
            .map_err(|error| ModelClientError::config(error.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ModelClientError> {
        validate_identifier(&self.default_model_id, "defaultModelId")?;
        if self.providers.is_empty() {
            return Err(ModelClientError::config(
                "agent model providers must not be empty",
            ));
        }
        for (provider_id, provider) in &self.providers {
            validate_identifier(provider_id, "provider id")?;
            provider.validate(provider_id)?;
        }
        if self.models.is_empty() {
            return Err(ModelClientError::config(
                "agent model models must not be empty",
            ));
        }
        let mut aliases = HashSet::new();
        for model in &self.models {
            model.validate(&self.providers)?;
            if !aliases.insert(model.model_name.as_str()) {
                return Err(ModelClientError::config(format!(
                    "duplicate modelName: {}",
                    model.model_name
                )));
            }
        }
        if !aliases.contains(self.default_model_id.as_str()) {
            return Err(ModelClientError::config(format!(
                "defaultModelId {} is not listed in models",
                self.default_model_id
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
}

impl AgentProviderConfig {
    fn validate(&self, provider_id: &str) -> Result<(), ModelClientError> {
        validate_identifier(&self.provider_type, "provider type")?;
        if let Some(base_url) = &self.base_url {
            validate_endpoint(base_url, &format!("provider {provider_id} baseUrl"))?;
        }
        validate_optional_string(
            &self.api_key_env,
            &format!("provider {provider_id} apiKeyEnv"),
        )?;
        validate_optional_string(&self.api_key, &format!("provider {provider_id} apiKey"))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentModelEntry {
    pub model_name: String,
    pub provider: String,
    pub model_id: String,
    #[serde(default)]
    pub context_window_tokens: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub compact_threshold: Option<f64>,
    #[serde(default)]
    pub default_reasoning_mode: Option<String>,
}

impl AgentModelEntry {
    fn validate(
        &self,
        providers: &BTreeMap<String, AgentProviderConfig>,
    ) -> Result<(), ModelClientError> {
        validate_identifier(&self.model_name, "modelName")?;
        validate_identifier(&self.provider, "model provider")?;
        validate_identifier(&self.model_id, "modelId")?;
        if !providers.contains_key(&self.provider) {
            return Err(ModelClientError::config(format!(
                "model {} references unknown provider {}",
                self.model_name, self.provider
            )));
        }
        if let Some(default_mode) = &self.default_reasoning_mode {
            validate_identifier(default_mode, "defaultReasoningMode")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ModelClientConfig {
    pub default_model_id: String,
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
        for (provider_id, agent_provider) in &agent.providers {
            let spec = catalog
                .providers
                .get(&agent_provider.provider_type)
                .ok_or_else(|| {
                    ModelClientError::config(format!(
                        "provider {provider_id} references unknown type {}",
                        agent_provider.provider_type
                    ))
                })?;
            let provider = ProviderConfig {
                protocol: spec.protocol,
                endpoint: agent_provider
                    .base_url
                    .clone()
                    .unwrap_or_else(|| spec.endpoint.clone()),
                api_key_env: agent_provider.api_key_env.clone(),
                api_key: agent_provider.api_key.clone(),
                headers: spec.headers.clone(),
                request: spec.request,
                body: spec.body.clone(),
            };
            provider.validate(provider_id)?;
            providers.insert(provider_id.clone(), provider);
        }

        let mut models = IndexMap::new();
        for entry in &agent.models {
            let agent_provider = &agent.providers[&entry.provider];
            let provider_spec = &catalog.providers[&agent_provider.provider_type];
            let spec = provider_spec.models.get(&entry.model_id).ok_or_else(|| {
                ModelClientError::config(format!(
                    "model {} references unknown catalog model {}/{}",
                    entry.model_name, agent_provider.provider_type, entry.model_id
                ))
            })?;
            let model = ModelConfig {
                provider: entry.provider.clone(),
                model_id: entry.model_id.clone(),
                context_window_tokens: entry
                    .context_window_tokens
                    .unwrap_or(spec.context_window_tokens),
                max_output_tokens: entry.max_output_tokens.unwrap_or(spec.max_output_tokens),
                compact_threshold: entry.compact_threshold.unwrap_or(spec.compact_threshold),
                temperature: spec.temperature,
                top_p: spec.top_p,
                body: spec.body.clone(),
                hosted_tools: spec.hosted_tools.clone(),
                reasoning: spec.reasoning.clone(),
                default_reasoning_mode: entry
                    .default_reasoning_mode
                    .clone()
                    .unwrap_or_else(|| spec.default_reasoning_mode.clone()),
                capabilities: spec.capabilities,
            };
            model.validate(&entry.model_name, &providers)?;
            models.insert(entry.model_name.clone(), model);
        }

        Ok(Self {
            default_model_id: agent.default_model_id.clone(),
            providers,
            models,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub protocol: ProviderProtocol,
    pub endpoint: String,
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub request: RequestPolicy,
    pub body: Map<String, Value>,
}

impl ProviderConfig {
    fn validate(&self, id: &str) -> Result<(), ModelClientError> {
        validate_endpoint(&self.endpoint, &format!("provider {id} endpoint"))?;
        self.request.validate(&format!("provider {id} request"))?;
        validate_body(&self.body, &format!("provider {id} body"))
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

    pub(crate) fn endpoint(&self) -> Result<Url, ModelClientError> {
        Url::parse(self.endpoint.trim())
            .map_err(|error| ModelClientError::config(error.to_string()))
    }
}

#[derive(Debug, Clone)]
pub struct ModelConfig {
    pub provider: String,
    pub model_id: String,
    pub context_window_tokens: u64,
    pub max_output_tokens: u32,
    pub compact_threshold: f64,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub body: Map<String, Value>,
    pub hosted_tools: Vec<Value>,
    pub reasoning: IndexMap<String, Map<String, Value>>,
    pub default_reasoning_mode: String,
    pub capabilities: ModelCapabilities,
}

impl ModelConfig {
    fn validate(
        &self,
        alias: &str,
        providers: &BTreeMap<String, ProviderConfig>,
    ) -> Result<(), ModelClientError> {
        if !providers.contains_key(&self.provider) {
            return Err(ModelClientError::config(format!(
                "model {alias} references unknown provider {}",
                self.provider
            )));
        }
        validate_identifier(&self.model_id, "modelId")?;
        let _ = self.max_input_tokens()?;
        validate_compact_threshold(self.compact_threshold, &format!("model {alias}"))?;
        validate_body(&self.body, &format!("model {alias} body"))?;
        validate_reasoning(
            &self.reasoning,
            &self.default_reasoning_mode,
            &format!("model {alias}"),
        )
    }

    pub fn max_input_tokens(&self) -> Result<u64, ModelClientError> {
        available_input_tokens(
            self.context_window_tokens,
            self.max_output_tokens,
            &format!("model {}", self.model_id),
        )
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
    reasoning: &IndexMap<String, Map<String, Value>>,
    default_mode: &str,
    source: &str,
) -> Result<(), ModelClientError> {
    validate_identifier(default_mode, &format!("{source} defaultReasoningMode"))?;
    for (mode, body) in reasoning {
        validate_identifier(mode, &format!("{source} reasoning mode"))?;
        validate_body(body, &format!("{source} reasoning.{mode}"))?;
    }
    if default_mode != "auto" && !reasoning.contains_key(default_mode) {
        return Err(ModelClientError::config(format!(
            "{source} defaultReasoningMode {default_mode} is not configured"
        )));
    }
    Ok(())
}

fn validate_compact_threshold(value: f64, source: &str) -> Result<(), ModelClientError> {
    if !value.is_finite() || value <= 0.0 || value > 1.0 {
        return Err(ModelClientError::config(format!(
            "{source} compactThreshold must be in (0, 1]"
        )));
    }
    Ok(())
}

fn validate_endpoint(value: &str, source: &str) -> Result<(), ModelClientError> {
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
