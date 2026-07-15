use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

#[derive(Debug, Clone)]
pub struct McpConfig {
    pub servers: BTreeMap<String, McpServerConfig>,
    pub fingerprint: String,
}

#[derive(Debug, Clone)]
pub enum McpServerConfig {
    Stdio(StdioConfig),
    StreamableHttp(StreamableHttpConfig),
}

impl McpServerConfig {
    pub fn description(&self) -> Option<&str> {
        match self {
            Self::Stdio(v) => v.description.as_deref(),
            Self::StreamableHttp(v) => v.description.as_deref(),
        }
    }

    pub fn auth(&self) -> Option<&AuthConfig> {
        match self {
            Self::Stdio(_) => None,
            Self::StreamableHttp(v) => v.auth.as_ref(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StdioConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StreamableHttpConfig {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub auth: Option<AuthConfig>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthConfig {
    #[serde(rename = "type")]
    pub auth_type: AuthType,
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthType {
    OAuth,
}

impl McpConfig {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|source| Error::ReadConfig {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_slice(&bytes, path.parent())
    }

    pub fn from_slice(bytes: &[u8], base_dir: Option<&Path>) -> Result<Self> {
        let root: Value =
            serde_json::from_slice(bytes).map_err(|e| Error::InvalidConfig(e.to_string()))?;
        let raw = root
            .get("mcpServers")
            .and_then(Value::as_object)
            .ok_or_else(|| Error::InvalidConfig("missing object mcpServers".into()))?;
        let mut servers = BTreeMap::new();
        for (name, value) in raw {
            servers.insert(name.clone(), parse_server(name, value, base_dir)?);
        }
        Ok(Self {
            servers,
            fingerprint: fingerprint(bytes),
        })
    }
}

fn parse_server(name: &str, value: &Value, base_dir: Option<&Path>) -> Result<McpServerConfig> {
    let obj = value
        .as_object()
        .ok_or_else(|| Error::InvalidConfig(format!("mcpServers.{name} must be an object")))?;
    let kind = obj.get("type").and_then(Value::as_str);
    let description = optional_string(obj.get("description"), name, "description")?;
    if kind.is_none() || matches!(kind, Some("stdio")) {
        let command = required_string(obj.get("command"), name, "command")?;
        let args = obj
            .get("args")
            .map(|v| string_array(v, name, "args"))
            .transpose()?
            .unwrap_or_default();
        let env = obj
            .get("env")
            .map(|v| string_map(v, name, "env"))
            .transpose()?
            .unwrap_or_default();
        let cwd = optional_string(obj.get("cwd"), name, "cwd")?
            .map(|v| {
                let p = PathBuf::from(expand_env(&v)?);
                Ok::<_, Error>(if p.is_relative() {
                    base_dir.unwrap_or(Path::new(".")).join(p)
                } else {
                    p
                })
            })
            .transpose()?;
        return Ok(McpServerConfig::Stdio(StdioConfig {
            command: expand_env(&command)?,
            args: expand_values(args)?,
            env: expand_map(env)?,
            cwd,
            description,
        }));
    }
    if !matches!(kind, Some("streamableHttp" | "http")) {
        return Err(Error::InvalidConfig(format!(
            "mcpServers.{name}.type is unsupported"
        )));
    }
    let url = expand_env(&required_string(obj.get("url"), name, "url")?)?;
    let headers = expand_map(
        obj.get("headers")
            .map(|v| string_map(v, name, "headers"))
            .transpose()?
            .unwrap_or_default(),
    )?;
    let auth = obj
        .get("auth")
        .map(|v| {
            serde_json::from_value(v.clone())
                .map_err(|e| Error::InvalidConfig(format!("mcpServers.{name}.auth: {e}")))
        })
        .transpose()?;
    Ok(McpServerConfig::StreamableHttp(StreamableHttpConfig {
        url,
        headers,
        auth,
        description,
    }))
}

fn required_string(v: Option<&Value>, server: &str, field: &str) -> Result<String> {
    optional_string(v, server, field)?
        .ok_or_else(|| Error::InvalidConfig(format!("mcpServers.{server}.{field} is required")))
}

fn optional_string(v: Option<&Value>, server: &str, field: &str) -> Result<Option<String>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(Error::InvalidConfig(format!(
            "mcpServers.{server}.{field} must be a string"
        ))),
    }
}

fn string_array(v: &Value, server: &str, field: &str) -> Result<Vec<String>> {
    v.as_array()
        .ok_or_else(|| {
            Error::InvalidConfig(format!("mcpServers.{server}.{field} must be an array"))
        })?
        .iter()
        .map(|v| {
            v.as_str().map(str::to_owned).ok_or_else(|| {
                Error::InvalidConfig(format!(
                    "mcpServers.{server}.{field} entries must be strings"
                ))
            })
        })
        .collect()
}

fn string_map(v: &Value, server: &str, field: &str) -> Result<BTreeMap<String, String>> {
    v.as_object()
        .ok_or_else(|| {
            Error::InvalidConfig(format!("mcpServers.{server}.{field} must be an object"))
        })?
        .iter()
        .map(|(k, v)| {
            v.as_str()
                .map(|v| (k.clone(), v.to_owned()))
                .ok_or_else(|| {
                    Error::InvalidConfig(format!(
                        "mcpServers.{server}.{field}.{k} must be a string"
                    ))
                })
        })
        .collect()
}

fn expand_values(values: Vec<String>) -> Result<Vec<String>> {
    values.into_iter().map(|v| expand_env(&v)).collect()
}

fn expand_map(values: BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    values
        .into_iter()
        .map(|(k, v)| Ok((k, expand_env(&v)?)))
        .collect()
}

fn expand_env(input: &str) -> Result<String> {
    let mut output = String::new();
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| {
            Error::InvalidConfig(format!("unterminated environment reference in {input:?}"))
        })?;
        let name = &after[..end];
        if name.is_empty() {
            return Err(Error::InvalidConfig("empty environment reference".into()));
        }
        output.push_str(&std::env::var(name).map_err(|_| Error::MissingEnvironment(name.into()))?);
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn fingerprint(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stdio_and_http() {
        unsafe { std::env::set_var("DWO_MCP_TEST_TOKEN", "secret") };
        let config = McpConfig::from_slice(br#"{"mcpServers":{"local":{"command":"node","args":["server.js"],"env":{"X":"y"},"cwd":"tools"},"remote":{"type":"streamableHttp","url":"https://example.test/mcp","headers":{"Authorization":"Bearer ${DWO_MCP_TEST_TOKEN}"},"auth":{"type":"oauth"}}}}"#, Some(Path::new("C:/profile"))).unwrap();
        let McpServerConfig::Stdio(local) = &config.servers["local"] else {
            panic!()
        };
        assert_eq!(local.cwd.as_deref(), Some(Path::new("C:/profile/tools")));
        let McpServerConfig::StreamableHttp(remote) = &config.servers["remote"] else {
            panic!()
        };
        assert_eq!(remote.headers["Authorization"], "Bearer secret");
        assert_eq!(remote.auth.as_ref().unwrap().auth_type, AuthType::OAuth);
        assert_eq!(config.fingerprint.len(), 16);
    }

    #[test]
    fn rejects_unknown_transport() {
        let error =
            McpConfig::from_slice(br#"{"mcpServers":{"bad":{"type":"sse","url":"x"}}}"#, None)
                .unwrap_err();
        assert!(error.to_string().contains("unsupported"));
    }
}
