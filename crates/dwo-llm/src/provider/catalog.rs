//! Built-in provider model catalog.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ModelCapabilities, ReasoningMode};

const DEFAULT_CATALOG_YAML: &str = include_str!("providers.yaml");

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelSpec {
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub capabilities: ModelCapabilities,
    #[serde(default)]
    pub reasoning_modes: Vec<ReasoningMode>,
    #[serde(default = "default_reasoning_mode")]
    pub default_reasoning_mode: ReasoningMode,
}

fn default_reasoning_mode() -> ReasoningMode {
    ReasoningMode::Auto
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSpec {
    #[serde(default)]
    pub api_base: Option<String>,
    pub models: HashMap<String, ProviderModelSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCatalog {
    pub providers: HashMap<String, ProviderSpec>,
}

/// Load the embedded default catalog.
pub fn load_default_catalog() -> Result<ProviderCatalog> {
    let loaded: Value =
        serde_yaml::from_str(DEFAULT_CATALOG_YAML).context("parse embedded providers.yaml")?;
    catalog_from_value(loaded, "<embedded providers.yaml>")
}

fn catalog_from_value(loaded: Value, source: &str) -> Result<ProviderCatalog> {
    if !loaded.is_object() {
        bail!(
            "Invalid provider catalog in {}: root must be object",
            source
        );
    }
    let catalog: ProviderCatalog = serde_json::from_value(loaded)
        .with_context(|| format!("validate provider catalog in {}", source))?;
    Ok(catalog)
}
