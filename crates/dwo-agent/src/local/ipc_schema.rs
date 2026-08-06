use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RpcRequest {
    pub(crate) id: u64,
    pub(crate) method: String,
    #[serde(default)]
    pub(crate) params: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RpcResponse {
    pub(crate) id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SessionRecord {
    pub(crate) info: SessionInfo,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SessionInfo {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) cwd: PathBuf,
    pub(crate) updated_at_ms: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SessionSnapshot {
    pub(crate) record: SessionRecord,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionOptions {
    pub(crate) config: SessionConfig,
    pub(crate) models: Vec<SessionModelOption>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionConfig {
    pub(crate) mode: SessionMode,
    pub(crate) model: String,
    pub(crate) reasoning: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionMode {
    FullAccess,
    Confirm,
    Watch,
}

impl SessionMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::FullAccess => "full_access",
            Self::Confirm => "confirm",
            Self::Watch => "watch",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionModelOption {
    pub(crate) id: String,
    pub(crate) reasoning: Vec<String>,
    pub(crate) default_reasoning: String,
}
