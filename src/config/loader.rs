//! Helpers for setup and persistence.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::models::{
    AgentMeta, ModelConfig, ModelProfile, ModelRegistry, deserialize_agent_meta,
    deserialize_model_registry,
};
use super::policy::ToolPolicyConfig;
use crate::llm::provider::load_default_catalog;
use crate::utils::files::{read_json_utf8, read_yaml_dict};

// Re-exports so callers can match the Python `from ..config.loader import …`.
pub use crate::utils::files::{utc_iso, write_json_utf8};

/// Resolve the agent structure directory for *agent_folder*, checking both
/// the folder itself and a nested `agent-structure/` layout.
pub fn resolve_agent_structure_dir(agent_folder: &Path) -> Result<PathBuf> {
    let root = std::fs::canonicalize(agent_folder)
        .with_context(|| format!("resolve agent folder {}", agent_folder.display()))?;
    if looks_like_agent_structure_dir(&root) {
        return Ok(root);
    }
    let nested = root.join("agent-structure");
    if looks_like_agent_structure_dir(&nested) {
        return Ok(nested);
    }
    bail!(
        "Cannot find agent structure directory. \
         Expected `<agent_folder>/agent.yaml` + `<agent_folder>/model.yaml` + \
         `<agent_folder>/resources/agents`, or the same under \
         `<agent_folder>/agent-structure`."
    );
}

/// Resolve the session store directory, defaulting to a path relative to the
/// agent structure directory.
pub fn resolve_session_store_dir(store_dir: &str, agent_structure_dir: &Path) -> Result<PathBuf> {
    resolve_agent_profile_dir(store_dir, agent_structure_dir)
}

/// Resolve the channel session directory, defaulting to a path relative to the
/// agent structure directory.
pub fn resolve_channel_session_dir(
    session_dir: &str,
    agent_structure_dir: &Path,
) -> Result<PathBuf> {
    resolve_agent_profile_dir(session_dir, agent_structure_dir)
}

fn resolve_agent_profile_dir(store_dir: &str, agent_structure_dir: &Path) -> Result<PathBuf> {
    let path = PathBuf::from(store_dir);
    if path.is_absolute() {
        Ok(path)
    } else {
        let joined = agent_structure_dir.join(path);
        // `canonicalize` requires the path to exist; fall back to the
        // non-canonical form when it does not.
        Ok(std::fs::canonicalize(&joined).unwrap_or(joined))
    }
}

fn looks_like_agent_structure_dir(path: &Path) -> bool {
    path.is_dir()
        && path.join("agent.yaml").is_file()
        && path.join("model.yaml").is_file()
        && path.join("resources").is_dir()
        && path.join("resources").join("agents").is_dir()
}

/// Read and validate the agent metadata YAML.
pub fn read_agent_meta(agent_yaml_path: &Path) -> Result<AgentMeta> {
    let payload = Value::Object(read_yaml_dict(agent_yaml_path)?);
    deserialize_agent_meta(payload)
        .with_context(|| format!("Invalid YAML config in {}", agent_yaml_path.display()))
}

/// Read optional `policy.yaml` from an agent structure directory.
pub fn read_tool_policy(agent_structure_dir: &Path) -> Result<ToolPolicyConfig> {
    let path = agent_structure_dir.join("policy.yaml");
    if !path.is_file() {
        return Ok(ToolPolicyConfig::default());
    }
    ToolPolicyConfig::from_value(Value::Object(read_yaml_dict(&path)?))
        .with_context(|| format!("Invalid YAML config in {}", path.display()))
}

/// Read and merge `model.yaml` with the built-in provider catalog, returning
/// the default model id and a name → profile lookup map.
pub fn read_model_registry(
    model_yaml_path: &Path,
) -> Result<(String, HashMap<String, ModelProfile>)> {
    let raw = read_yaml_dict(model_yaml_path)?;
    let payload = build_model_registry_payload(raw)?;
    let registry: ModelRegistry = deserialize_model_registry(Value::Object(payload))
        .with_context(|| format!("Invalid YAML config in {}", model_yaml_path.display()))?;
    let default_id = registry.default_model_id.clone();
    let profiles: HashMap<String, ModelProfile> = registry
        .models
        .into_iter()
        .map(|p| (p.model_name.clone(), p))
        .collect();
    Ok((default_id, profiles))
}

/// Validate arbitrary JSON payload against type `T`.
pub fn read_json_model<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let payload = Value::Object(read_json_utf8(path)?);
    serde_json::from_value(payload)
        .with_context(|| format!("Invalid JSON config in {}", path.display()))
}

// ── Private: build the merged registry payload ────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredModel {
    model_name: String,
    provider: String,
    model_id: String,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    api_base: Option<String>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    timeout_seconds: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    default_reasoning_mode: Option<String>,
    #[serde(default = "default_compact_threshold")]
    compact_threshold: f64,
}

fn default_compact_threshold() -> f64 {
    0.8
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredModelRegistry {
    default_model_id: String,
    models: Vec<ConfiguredModel>,
}

fn build_model_registry_payload(raw: Map<String, Value>) -> Result<Map<String, Value>> {
    let configured: ConfiguredModelRegistry =
        serde_json::from_value(Value::Object(raw)).context("parse model.yaml shape")?;
    let catalog = load_default_catalog()?;

    let mut models_out: Vec<Value> = Vec::with_capacity(configured.models.len());
    for item in configured.models {
        let provider = catalog
            .providers
            .get(&item.provider)
            .ok_or_else(|| anyhow::anyhow!("Unknown provider in model.yaml: {}", item.provider))?;
        let model_spec = provider.models.get(&item.model_id).ok_or_else(|| {
            anyhow::anyhow!(
                "Unknown model `{}` for provider `{}`",
                item.model_id,
                item.provider
            )
        })?;

        let default_reasoning_mode = item
            .default_reasoning_mode
            .unwrap_or_else(|| model_spec.default_reasoning_mode.as_str().to_string());
        let api_base = match item.api_base {
            Some(v) => Some(v),
            None => provider.api_base.clone(),
        };

        let mut config = ModelConfig {
            provider: item.provider.clone(),
            model_id: item.model_id.clone(),
            api_key_env: item.api_key_env,
            api_key: item.api_key,
            api_base,
            temperature: item.temperature,
            top_p: item.top_p,
            timeout_seconds: item.timeout_seconds,
            max_tokens: Some(
                item.max_tokens
                    .filter(|v| *v != 0)
                    .unwrap_or(model_spec.max_output_tokens),
            ),
        };
        config.validate()?;

        let reasoning_modes: Vec<Value> = model_spec
            .reasoning_modes
            .iter()
            .map(|m| Value::String(m.as_str().to_string()))
            .collect();

        models_out.push(json!({
            "model_name": item.model_name,
            "config": serde_json::to_value(&config)?,
            "capabilities": serde_json::to_value(&model_spec.capabilities)?,
            "context_window": model_spec.context_window,
            "max_output_tokens": model_spec.max_output_tokens,
            "compact_threshold": item.compact_threshold,
            "reasoning_modes": reasoning_modes,
            "default_reasoning_mode": default_reasoning_mode,
        }));
    }

    let mut out = Map::new();
    out.insert(
        "default_model_id".to_string(),
        Value::String(configured.default_model_id),
    );
    out.insert("models".to_string(), Value::Array(models_out));
    Ok(out)
}
