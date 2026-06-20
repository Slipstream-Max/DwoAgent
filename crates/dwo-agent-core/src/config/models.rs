//! Central data models.

use std::collections::HashSet;
use std::fmt;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::utils::policy::parse_policy_mode;
use crate::utils::strings::{normalize_optional_str, normalize_required_str};
pub use dwo_llm::{ModelCapabilities, ModelConfig, ReasoningMode};

pub const DEFAULT_SESSION_STORE_DIR: &str = "runtime/sessions";
pub const DEFAULT_CHANNEL_STATE_DIR: &str = "runtime/channel_state";

// ── Literal-style enums ────────────────────────────────────────────────────

macro_rules! str_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$var_meta:meta])*
                $variant:ident => $literal:literal
            ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis enum $name {
            $( $(#[$var_meta])* $variant ),+
        }

        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self {
                    $( Self::$variant => $literal ),+
                }
            }

            pub fn from_str(value: &str) -> Result<Self> {
                match value {
                    $( $literal => Ok(Self::$variant), )+
                    other => anyhow::bail!(concat!(
                        "invalid ",
                        stringify!($name),
                        ": {}"
                    ), other),
                }
            }

            pub const ALL: &'static [Self] = &[ $( Self::$variant ),+ ];
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = <String as Deserialize>::deserialize(d)?;
                Self::from_str(&raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyMode {
    FullAccess,
    Confirm,
    Watch,
}

impl PolicyMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FullAccess => "full_access",
            Self::Confirm => "confirm",
            Self::Watch => "watch",
        }
    }

    pub fn from_str(value: &str) -> Result<Self> {
        match parse_policy_mode(value)?.as_str() {
            "full_access" => Ok(Self::FullAccess),
            "confirm" => Ok(Self::Confirm),
            "watch" => Ok(Self::Watch),
            other => bail!("invalid PolicyMode: {other}"),
        }
    }

    pub const ALL: &'static [Self] = &[Self::FullAccess, Self::Confirm, Self::Watch];
}

impl fmt::Display for PolicyMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for PolicyMode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PolicyMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let raw = <String as Deserialize>::deserialize(d)?;
        Self::from_str(&raw).map_err(serde::de::Error::custom)
    }
}

str_enum! {
    pub enum AgentState {
        Idle => "idle",
        Running => "running",
        WaitingUserConfirm => "waiting_user_confirm",
        Cancelling => "cancelling",
        Stop => "stop",
    }
}

str_enum! {
    pub enum StopReason {
        Completed => "completed",
        Cancelled => "cancelled",
        MaxTurns => "max_turns",
    }
}

str_enum! {
    pub enum ToolSwitch {
        Enable => "enable",
        Disable => "disable",
    }
}

impl ToolSwitch {
    pub fn enabled(&self) -> bool {
        matches!(self, Self::Enable)
    }
}

fn default_tool_switch() -> ToolSwitch {
    ToolSwitch::Enable
}

fn default_session_store_dir() -> String {
    DEFAULT_SESSION_STORE_DIR.to_string()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentTools {
    #[serde(default = "default_tool_switch")]
    pub file_edit: ToolSwitch,
    #[serde(default = "default_tool_switch")]
    pub terminal: ToolSwitch,
    #[serde(default = "default_tool_switch")]
    pub subagent: ToolSwitch,
}

impl Default for AgentTools {
    fn default() -> Self {
        Self {
            file_edit: ToolSwitch::Enable,
            terminal: ToolSwitch::Enable,
            subagent: ToolSwitch::Enable,
        }
    }
}

impl AgentTools {
    pub fn file_edit_enabled(&self) -> bool {
        self.file_edit.enabled()
    }

    pub fn terminal_enabled(&self) -> bool {
        self.terminal.enabled()
    }

    pub fn subagent_enabled(&self) -> bool {
        self.subagent.enabled()
    }
}

// ── Model config + profile ────────────────────────────────────────────────

/// Registry entry for one model alias.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    pub model_name: String,
    pub config: ModelConfig,
    pub capabilities: ModelCapabilities,
    pub context_window: u32,
    pub max_output_tokens: u32,
    #[serde(default = "default_compact_threshold")]
    pub compact_threshold: f64,
    #[serde(default)]
    pub reasoning_modes: Vec<ReasoningMode>,
    #[serde(default = "default_reasoning_mode")]
    pub default_reasoning_mode: ReasoningMode,
}

fn default_compact_threshold() -> f64 {
    0.8
}
fn default_reasoning_mode() -> ReasoningMode {
    ReasoningMode::Auto
}

impl ModelProfile {
    pub fn validate(&mut self) -> Result<()> {
        self.model_name = normalize_required_str(&self.model_name, "model_name")?;
        self.config.validate()?;
        if self.context_window == 0 {
            bail!("context_window must be positive");
        }
        if self.max_output_tokens == 0 {
            bail!("max_output_tokens must be positive");
        }
        if !(self.compact_threshold > 0.0 && self.compact_threshold <= 1.0) {
            bail!("compact_threshold must be in (0, 1]");
        }
        if !self.reasoning_modes.contains(&self.default_reasoning_mode) {
            bail!("default_reasoning_mode must be listed in reasoning_modes");
        }
        Ok(())
    }
}

/// Top-level registry loaded from the `model` section in `agent.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRegistry {
    pub default_model_id: String,
    pub models: Vec<ModelProfile>,
}

impl ModelRegistry {
    pub fn validate(&mut self) -> Result<()> {
        self.default_model_id = normalize_required_str(&self.default_model_id, "default_model_id")?;
        if self.models.is_empty() {
            bail!("models cannot be empty");
        }
        for profile in &mut self.models {
            profile.validate()?;
        }
        let mut seen: HashSet<&str> = HashSet::new();
        for profile in &self.models {
            if !seen.insert(profile.model_name.as_str()) {
                bail!("Duplicate model_name found in agent.yaml model section");
            }
        }
        if !seen.contains(self.default_model_id.as_str()) {
            bail!(
                "default_model_id `{}` not found in agent.yaml model section",
                self.default_model_id
            );
        }
        Ok(())
    }
}

// ── Agent metadata ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMeta {
    pub agent_id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub max_running_turn: Option<u32>,
    #[serde(default)]
    pub external_skills_dirs: Vec<String>,
    #[serde(default)]
    pub external_rule_files: Vec<String>,
    #[serde(default)]
    pub tools: AgentTools,
    pub policy_mode: PolicyMode,
    #[serde(default = "default_session_store_dir")]
    pub session_store_dir: String,
}

impl AgentMeta {
    pub fn validate(&mut self) -> Result<()> {
        self.agent_id = normalize_required_str(&self.agent_id, "agent_id")?;
        self.name = normalize_required_str(&self.name, "name")?;
        self.description = normalize_required_str(&self.description, "description")?;
        self.session_store_dir =
            normalize_required_str(&self.session_store_dir, "session_store_dir")?;
        self.external_skills_dirs =
            normalize_string_list(&self.external_skills_dirs, "external_skills_dirs")?;
        self.external_rule_files =
            normalize_string_list(&self.external_rule_files, "external_rule_files")?;
        if let Some(max_running_turn) = self.max_running_turn
            && max_running_turn == 0
        {
            bail!("max_running_turn must be positive when set");
        }
        // PolicyMode deserializer already restricts allowed values; this
        // mirror exists so manual construction still validates.
        parse_policy_mode(self.policy_mode.as_str())?;
        Ok(())
    }
}

fn normalize_string_list(values: &[String], field: &str) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            bail!("{field} entries must not be empty");
        }
        out.push(trimmed.to_string());
    }
    Ok(out)
}

// ── Session payloads ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTranscriptEvent {
    pub updated_at: String,
    pub update: Map<String, Value>,
}

impl SessionTranscriptEvent {
    pub fn new(updated_at: impl Into<String>, update: Map<String, Value>) -> Result<Self> {
        let updated_at = normalize_required_str(&updated_at.into(), "updated_at")?;
        Ok(Self { updated_at, update })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionModelContextPayload {
    pub messages: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ContextUsageSnapshot>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContextUsageSnapshot {
    pub used: u64,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMetaPayload {
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub title: Option<String>,
    pub model_id: String,
    pub mode_id: PolicyMode,
    pub state: AgentState,
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_running_turn: Option<u32>,
    pub runtime_tools: AgentTools,
    pub tool_schemas: Vec<Value>,
    #[serde(default)]
    pub pending_model_id: Option<String>,
    #[serde(default = "default_reasoning_mode")]
    pub reasoning_mode: ReasoningMode,
    #[serde(default)]
    pub pending_reasoning_mode: Option<ReasoningMode>,
}

impl SessionMetaPayload {
    pub fn validate(&mut self) -> Result<()> {
        self.session_id = normalize_required_str(&self.session_id, "session_id")?;
        self.cwd = normalize_required_str(&self.cwd, "cwd")?;
        self.model_id = normalize_required_str(&self.model_id, "model_id")?;
        self.title = normalize_optional_str(self.title.as_deref(), "title")?;
        self.pending_model_id =
            normalize_optional_str(self.pending_model_id.as_deref(), "pending_model_id")?;
        self.updated_at = normalize_optional_str(self.updated_at.as_deref(), "updated_at")?;
        if let Some(max_running_turn) = self.max_running_turn
            && max_running_turn == 0
        {
            bail!("max_running_turn must be positive when set");
        }
        Ok(())
    }
}

/// Deserialize a validated [`AgentMeta`] from a JSON-like map.
pub fn deserialize_agent_meta(payload: Value) -> Result<AgentMeta> {
    let mut meta: AgentMeta = serde_json::from_value(payload).context("validate AgentMeta")?;
    meta.validate()?;
    Ok(meta)
}

pub fn deserialize_model_registry(payload: Value) -> Result<ModelRegistry> {
    let mut reg: ModelRegistry =
        serde_json::from_value(payload).context("validate ModelRegistry")?;
    reg.validate()?;
    Ok(reg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_meta_allows_missing_max_running_turn() {
        let meta = deserialize_agent_meta(serde_json::json!({
            "agent_id": "test-agent",
            "name": "Test Agent",
            "description": "test",
            "policy_mode": "confirm",
            "session_store_dir": ".sessions"
        }))
        .unwrap();

        assert_eq!(meta.max_running_turn, None);
        assert!(meta.tools.file_edit_enabled());
    }

    #[test]
    fn agent_meta_uses_default_session_dir() {
        let meta = deserialize_agent_meta(serde_json::json!({
            "agent_id": "test-agent",
            "name": "Test Agent",
            "description": "test",
            "policy_mode": "confirm"
        }))
        .unwrap();

        assert_eq!(meta.session_store_dir, "runtime/sessions");
    }

    #[test]
    fn agent_meta_rejects_channel_state_dir() {
        let err = deserialize_agent_meta(serde_json::json!({
            "agent_id": "test-agent",
            "name": "Test Agent",
            "description": "test",
            "policy_mode": "confirm",
            "session_store_dir": ".sessions",
            "channel_state_dir": " channel-state "
        }))
        .unwrap_err();

        assert!(format!("{err:#}").contains("channel_state_dir"));
    }

    #[test]
    fn agent_meta_reads_tool_switches() {
        let meta = deserialize_agent_meta(serde_json::json!({
            "agent_id": "test-agent",
            "name": "Test Agent",
            "description": "test",
            "tools": {
                "file_edit": "disable",
                "terminal": "enable",
                "subagent": "disable"
            },
            "policy_mode": "confirm",
            "session_store_dir": ".sessions"
        }))
        .unwrap();

        assert!(!meta.tools.file_edit_enabled());
        assert!(meta.tools.terminal_enabled());
        assert!(!meta.tools.subagent_enabled());
    }

    #[test]
    fn agent_meta_reads_external_context_paths() {
        let meta = deserialize_agent_meta(serde_json::json!({
            "agent_id": "test-agent",
            "name": "Test Agent",
            "description": "test",
            "policy_mode": "confirm",
            "session_store_dir": ".sessions",
            "external_skills_dirs": [" shared/skills "],
            "external_rule_files": [" shared/rules/common.md "]
        }))
        .unwrap();

        assert_eq!(meta.external_skills_dirs, vec!["shared/skills"]);
        assert_eq!(meta.external_rule_files, vec!["shared/rules/common.md"]);
    }

    #[test]
    fn agent_meta_rejects_zero_max_running_turn() {
        let err = deserialize_agent_meta(serde_json::json!({
            "agent_id": "test-agent",
            "name": "Test Agent",
            "description": "test",
            "max_running_turn": 0,
            "policy_mode": "confirm",
            "session_store_dir": ".sessions"
        }))
        .unwrap_err();

        assert!(err.to_string().contains("max_running_turn"));
    }
}
