//! Data models for Agent Skills.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Properties parsed from a skill's SKILL.md frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillProperties {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub compatibility: Option<String>,
    #[serde(default, rename = "allowed-tools")]
    pub allowed_tools: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl SkillProperties {
    /// Convert to a JSON object, mirroring Python's `to_dict(by_alias=True,
    /// exclude_none=True)` + the `metadata` omit-when-empty rule.
    pub fn to_dict(&self) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("name".to_string(), Value::String(self.name.clone()));
        out.insert(
            "description".to_string(),
            Value::String(self.description.clone()),
        );
        if let Some(v) = &self.license {
            out.insert("license".to_string(), Value::String(v.clone()));
        }
        if let Some(v) = &self.compatibility {
            out.insert("compatibility".to_string(), Value::String(v.clone()));
        }
        if let Some(v) = &self.allowed_tools {
            out.insert("allowed-tools".to_string(), Value::String(v.clone()));
        }
        if !self.metadata.is_empty() {
            let mut meta = Map::new();
            for (k, v) in &self.metadata {
                meta.insert(k.clone(), Value::String(v.clone()));
            }
            out.insert("metadata".to_string(), Value::Object(meta));
        }
        out
    }
}
