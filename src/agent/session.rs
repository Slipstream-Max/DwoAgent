//! Session data model and persistence.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::loader::write_json_utf8;
use crate::config::models::{
    AgentState, AgentTools, PolicyMode, ReasoningMode, SessionMetaPayload,
    SessionModelContextPayload,
};
use crate::utils::strings::{normalize_optional_str, normalize_required_str};

pub const SESSION_META_FILE: &str = "session.json";
pub const SESSION_MODEL_CONTEXT_FILE: &str = "model_context.json";
pub const SESSION_CLIENT_TRANSCRIPT_FILE: &str = "client_transcript.jsonl";
pub const SESSION_TITLE_LENGTH: usize = 10;

/// Serializable session state — pure data, no runtime objects.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub session_id: String,
    pub cwd: String,
    pub session_dir: PathBuf,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_running_turn: Option<u32>,
    pub runtime_tools: AgentTools,
    pub tool_schemas: Vec<Value>,
    #[serde(default = "default_mode")]
    pub mode_id: PolicyMode,
    #[serde(default = "default_state")]
    pub state: AgentState,
    #[serde(default)]
    pub stop_reason: Option<crate::config::models::StopReason>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub pending_model_id: Option<String>,
    #[serde(default = "default_reasoning")]
    pub reasoning_mode: ReasoningMode,
    #[serde(default)]
    pub pending_reasoning_mode: Option<ReasoningMode>,
}

fn default_mode() -> PolicyMode {
    PolicyMode::Confirm
}
fn default_state() -> AgentState {
    AgentState::Idle
}
fn default_reasoning() -> ReasoningMode {
    ReasoningMode::Auto
}

impl Session {
    pub fn validate(&mut self) -> Result<()> {
        self.session_id = normalize_required_str(&self.session_id, "session_id")?;
        self.cwd = normalize_required_str(&self.cwd, "cwd")?;
        self.model_id = normalize_required_str(&self.model_id, "model_id")?;
        self.title = normalize_optional_str(self.title.as_deref(), "title")?;
        self.updated_at = normalize_optional_str(self.updated_at.as_deref(), "updated_at")?;
        self.pending_model_id =
            normalize_optional_str(self.pending_model_id.as_deref(), "pending_model_id")?;
        Ok(())
    }

    pub fn to_meta_payload(&self) -> SessionMetaPayload {
        SessionMetaPayload {
            session_id: self.session_id.clone(),
            cwd: self.cwd.clone(),
            title: self.title.clone(),
            model_id: self.model_id.clone(),
            mode_id: self.mode_id,
            state: self.state,
            stop_reason: self.stop_reason,
            updated_at: self.updated_at.clone(),
            max_running_turn: self.max_running_turn,
            runtime_tools: self.runtime_tools,
            tool_schemas: self.tool_schemas.clone(),
            pending_model_id: self.pending_model_id.clone(),
            reasoning_mode: self.reasoning_mode,
            pending_reasoning_mode: self.pending_reasoning_mode,
        }
    }
}

/// Handles reading / writing session state to disk.
pub struct SessionPersistence {
    session_dir: PathBuf,
}

impl SessionPersistence {
    pub fn new(session_dir: PathBuf) -> Self {
        Self { session_dir }
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    /// Write session metadata and context to disk.
    pub fn save_session(&self, session: &Session, messages: &[Value]) -> Result<()> {
        std::fs::create_dir_all(&self.session_dir)
            .with_context(|| format!("create session dir {}", self.session_dir.display()))?;

        self.save_session_meta(session)?;
        let context_payload = SessionModelContextPayload {
            messages: messages.to_vec(),
        };

        write_json_utf8(
            &self.session_dir.join(SESSION_MODEL_CONTEXT_FILE),
            &serde_json::to_value(&context_payload)?,
        )?;
        let transcript_path = self.session_dir.join(SESSION_CLIENT_TRANSCRIPT_FILE);
        if !transcript_path.exists() {
            std::fs::write(&transcript_path, "")
                .with_context(|| format!("create transcript {}", transcript_path.display()))?;
        }
        Ok(())
    }

    /// Write only session metadata. This is used for live config changes so
    /// they do not wait on the in-flight turn's runtime/context lock.
    pub fn save_session_meta(&self, session: &Session) -> Result<()> {
        std::fs::create_dir_all(&self.session_dir)
            .with_context(|| format!("create session dir {}", self.session_dir.display()))?;

        let meta_payload = session.to_meta_payload();

        write_json_utf8(
            &self.session_dir.join(SESSION_META_FILE),
            &serde_json::to_value(&meta_payload)?,
        )?;
        let transcript_path = self.session_dir.join(SESSION_CLIENT_TRANSCRIPT_FILE);
        if !transcript_path.exists() {
            std::fs::write(&transcript_path, "")
                .with_context(|| format!("create transcript {}", transcript_path.display()))?;
        }
        Ok(())
    }

    /// Append a single transcript event to the JSONL log.
    pub fn append_transcript_event(&self, event: &Value) -> Result<()> {
        std::fs::create_dir_all(&self.session_dir)
            .with_context(|| format!("create session dir {}", self.session_dir.display()))?;
        let path = self.session_dir.join(SESSION_CLIENT_TRANSCRIPT_FILE);
        let mut handle = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open transcript {}", path.display()))?;
        let line = serde_json::to_string(event)?;
        handle.write_all(line.as_bytes())?;
        handle.write_all(b"\n")?;
        Ok(())
    }
}
