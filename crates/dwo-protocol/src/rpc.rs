use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RpcRoute {
    Acp,
    Dwo,
}

impl RpcRoute {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Acp => "acp",
            Self::Dwo => "dwo",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: String,
    pub route: RpcRoute,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl RpcRequest {
    pub fn new(route: RpcRoute, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: uuid::Uuid::new_v4().to_string(),
            route,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn success(id: String, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(id: String, error: RpcError) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    fn with_code(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::with_code("invalid_request", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::with_code("internal_error", message)
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::with_code("invalid_params", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::with_code("not_found", message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::with_code("conflict", message)
    }

    pub fn permission_denied(message: impl Into<String>) -> Self {
        Self::with_code("permission_denied", message)
    }

    pub fn auth_required(message: impl Into<String>) -> Self {
        Self::with_code("auth_required", message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::with_code("unavailable", message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::with_code("timeout", message)
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::with_code("method_not_found", format!("unknown RPC method: {method}"))
    }

    pub fn from_anyhow(error: &anyhow::Error) -> Self {
        let message = format!("{error:#}");
        if error.downcast_ref::<serde_json::Error>().is_some() {
            return Self::invalid_params(message);
        }
        if message.starts_with("unknown RPC method:") {
            return Self::method_not_found(
                message.trim_start_matches("unknown RPC method: ").trim(),
            );
        }
        if message.starts_with("request id ") && message.contains("was reused") {
            return Self::conflict(message);
        }
        let normalized = message.to_ascii_lowercase();
        if normalized.contains("not found")
            || normalized.contains("does not exist")
            || normalized.contains("unknown session")
        {
            return Self::not_found(message);
        }
        if normalized.contains("permission denied")
            || normalized.contains("not permitted")
            || normalized.contains("access denied")
        {
            return Self::permission_denied(message);
        }
        if normalized.contains("authentication required")
            || normalized.contains("not authenticated")
            || normalized.contains("oauth login required")
        {
            return Self::auth_required(message);
        }
        if normalized.contains("timed out") || normalized.contains("timeout") {
            return Self::timeout(message);
        }
        if normalized.contains("unavailable") || normalized.contains("connection refused") {
            return Self::unavailable(message);
        }
        if normalized.contains("must be")
            || normalized.contains("is required")
            || normalized.starts_with("invalid ")
        {
            return Self::invalid_params(message);
        }
        Self::internal(message)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcEvent {
    pub jsonrpc: String,
    pub route: RpcRoute,
    pub method: String,
    pub params: Value,
}

impl RpcEvent {
    pub fn new(route: RpcRoute, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            route,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SessionRecord {
    pub info: SessionInfo,
}

#[derive(Debug, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub updated_at_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct SessionSnapshot {
    pub record: SessionRecord,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptDirectiveOptions {
    #[serde(default)]
    pub skills: Vec<PromptDirectiveOption>,
    #[serde(default)]
    pub mcp_servers: Vec<PromptDirectiveOption>,
}

#[derive(Debug, Deserialize)]
pub struct PromptDirectiveOption {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOptions {
    pub config: SessionConfig,
    pub models: Vec<SessionModelOption>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfig {
    pub mode: SessionMode,
    pub model: String,
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    FullAccess,
    Confirm,
    Watch,
}

impl SessionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FullAccess => "full_access",
            Self::Confirm => "confirm",
            Self::Watch => "watch",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModelOption {
    pub id: String,
    pub provider: String,
    pub reasoning: Vec<String>,
    pub default_reasoning: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_preserve_client_actionable_categories() {
        assert_eq!(
            RpcError::from_anyhow(&anyhow::anyhow!(
                "request id x was reused with different method or params"
            ))
            .code,
            "conflict"
        );
        assert_eq!(
            RpcError::from_anyhow(&anyhow::anyhow!("automation job not found: nightly")).code,
            "not_found"
        );
        assert_eq!(
            RpcError::from_anyhow(&anyhow::anyhow!("OAuth login required")).code,
            "auth_required"
        );
    }
}
