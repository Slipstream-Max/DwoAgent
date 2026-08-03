use std::fmt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use dwo_context::{ContentBlock, MessageContent, SessionContext};
use dwo_tools::SessionMode;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! string_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new() -> Self {
                Self(format!(concat!($prefix, "{}"), Uuid::new_v4()))
            }

            pub fn parse(value: impl Into<String>) -> Result<Self, String> {
                let value = value.into();
                if value.is_empty()
                    || !value.chars().all(|character| {
                        character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                    })
                {
                    return Err(format!("invalid {}", stringify!($name)));
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

string_id!(SessionId, "session-");

pub const DEFAULT_MAX_MODEL_STEPS: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub info: SessionInfo,
    pub llm: SessionLlmSettings,
    pub context: SessionContext,
    #[serde(default, skip_serializing_if = "is_false")]
    auto_title_pending: bool,
    #[serde(default = "default_max_model_steps")]
    max_model_steps: usize,
}

fn default_max_model_steps() -> usize {
    DEFAULT_MAX_MODEL_STEPS
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,
    pub title: String,
    pub cwd: PathBuf,
    pub mode: SessionMode,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLlmSettings {
    pub model: String,
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfig {
    pub mode: SessionMode,
    pub model: String,
    pub reasoning: Option<String>,
    pub max_model_steps: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "key", content = "value", rename_all = "snake_case")]
pub enum SessionConfigUpdate {
    Mode(SessionMode),
    Model(String),
    Reasoning(Option<String>),
}

impl Default for SessionLlmSettings {
    fn default() -> Self {
        Self {
            model: "scripted-test-model".to_string(),
            reasoning: None,
        }
    }
}

impl SessionRecord {
    pub(crate) fn from_persisted_parts(
        info: SessionInfo,
        llm: SessionLlmSettings,
        context: SessionContext,
        auto_title_pending: bool,
    ) -> Self {
        Self {
            info,
            llm,
            context,
            auto_title_pending,
            max_model_steps: DEFAULT_MAX_MODEL_STEPS,
        }
    }

    pub fn new(
        id: SessionId,
        title: String,
        cwd: PathBuf,
        mode: SessionMode,
        llm: SessionLlmSettings,
        max_model_steps: usize,
    ) -> Self {
        let now = unix_time_ms();
        Self {
            info: SessionInfo {
                id,
                parent_session_id: None,
                title,
                cwd,
                mode,
                created_at_ms: now,
                updated_at_ms: now,
            },
            llm,
            context: SessionContext::default(),
            auto_title_pending: false,
            max_model_steps,
        }
    }

    pub(crate) fn set_parent_session_id(&mut self, parent_session_id: Option<SessionId>) {
        self.info.parent_session_id = parent_session_id;
    }

    pub(crate) fn enable_auto_title(&mut self) {
        self.auto_title_pending = true;
    }

    pub(crate) fn auto_title_pending(&self) -> bool {
        self.auto_title_pending
    }

    pub(crate) fn set_automatic_title(&mut self, title: String) {
        self.info.title = title;
        self.auto_title_pending = false;
        self.touch();
    }

    pub(crate) fn touch(&mut self) {
        self.info.updated_at_ms = unix_time_ms();
    }

    pub fn config(&self) -> SessionConfig {
        SessionConfig {
            mode: self.info.mode,
            model: self.llm.model.clone(),
            reasoning: self.llm.reasoning.clone(),
            max_model_steps: self.max_model_steps,
        }
    }

    pub(crate) fn apply_config(&mut self, update: SessionConfigUpdate) -> Result<(), String> {
        match update {
            SessionConfigUpdate::Mode(mode) => self.info.mode = mode,
            SessionConfigUpdate::Model(model) => {
                let model = model.trim();
                if model.is_empty() {
                    return Err("model must not be empty".to_string());
                }
                self.llm.model = model.to_string();
                // Reasoning modes belong to the selected model. Resolve the new
                // model's default when config options are rebuilt.
                self.llm.reasoning = None;
            }
            SessionConfigUpdate::Reasoning(reasoning) => {
                self.llm.reasoning = reasoning
                    .map(|reasoning| {
                        let reasoning = reasoning.trim();
                        if reasoning.is_empty() {
                            Err("reasoning must not be empty; use null to disable it".to_string())
                        } else {
                            Ok(reasoning.to_string())
                        }
                    })
                    .transpose()?;
            }
        }
        Ok(())
    }
}

pub(crate) fn title_from_user_content(content: &MessageContent) -> Option<String> {
    content.as_blocks().iter().find_map(|block| {
        let ContentBlock::Text { text, .. } = block else {
            return None;
        };
        let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
        (!normalized.is_empty()).then(|| normalized.chars().take(10).collect())
    })
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
