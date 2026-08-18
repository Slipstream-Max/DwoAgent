use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[cfg(windows)]
use std::collections::HashSet;
#[cfg(windows)]
use winreg::{
    RegKey,
    enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfig {
    pub servers: BTreeMap<String, McpServerConfig>,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdioConfig {
    pub command: String,
    pub args: Vec<String>,
    /// Expanded values explicitly configured for this server.
    pub env: BTreeMap<String, String>,
    /// Complete environment passed to the spawned process. This stays internal so credentials
    /// from the parent environment can never be rendered into the MCP catalog.
    pub(crate) process_env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
        Self::from_slice_with_environment(bytes, base_dir, Environment::from_current())
    }

    fn from_slice_with_environment(
        bytes: &[u8],
        base_dir: Option<&Path>,
        environment: Environment,
    ) -> Result<Self> {
        let root: Value =
            serde_json::from_slice(bytes).map_err(|e| Error::InvalidConfig(e.to_string()))?;
        let raw = root
            .get("mcpServers")
            .and_then(Value::as_object)
            .ok_or_else(|| Error::InvalidConfig("missing object mcpServers".into()))?;
        let mut servers = BTreeMap::new();
        for (name, value) in raw {
            if value.get("enabled").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            servers.insert(
                name.clone(),
                parse_server(name, value, base_dir, &environment)?,
            );
        }
        Ok(Self {
            servers,
            fingerprint: fingerprint(bytes),
        })
    }
}

fn parse_server(
    name: &str,
    value: &Value,
    base_dir: Option<&Path>,
    environment: &Environment,
) -> Result<McpServerConfig> {
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
        let raw_env = obj
            .get("env")
            .map(|v| string_map(v, name, "env"))
            .transpose()?
            .unwrap_or_default();
        let mut resolver = EnvironmentResolver::new(environment.clone(), raw_env);
        let env = resolver.resolve_configured()?;
        let cwd = optional_string(obj.get("cwd"), name, "cwd")?
            .map(|v| {
                let p = PathBuf::from(resolver.expand(&v)?);
                Ok::<_, Error>(if p.is_relative() {
                    base_dir.unwrap_or(Path::new(".")).join(p)
                } else {
                    p
                })
            })
            .transpose()?;
        let mut process_env = environment.clone();
        for (key, value) in &env {
            process_env.set(key, value.clone());
        }
        return Ok(McpServerConfig::Stdio(StdioConfig {
            command: resolver.expand(&command)?,
            args: args
                .into_iter()
                .map(|value| resolver.expand(&value))
                .collect::<Result<_>>()?,
            env,
            process_env: process_env.into_values(),
            cwd,
            description,
        }));
    }
    if !matches!(kind, Some("streamableHttp" | "http")) {
        return Err(Error::InvalidConfig(format!(
            "mcpServers.{name}.type is unsupported"
        )));
    }
    let mut resolver = EnvironmentResolver::new(environment.clone(), BTreeMap::new());
    let url = resolver.expand(&required_string(obj.get("url"), name, "url")?)?;
    let headers = expand_map_with(
        &mut resolver,
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

fn expand_map_with(
    resolver: &mut EnvironmentResolver,
    values: BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    values
        .into_iter()
        .map(|(key, value)| Ok((key, resolver.expand(&value)?)))
        .collect()
}

#[derive(Debug, Clone)]
struct Environment {
    values: BTreeMap<String, String>,
}

impl Environment {
    fn from_current() -> Self {
        let mut environment = Self {
            values: std::env::vars()
                .map(|(key, value)| (environment_key(&key), value))
                .collect(),
        };
        #[cfg(windows)]
        environment.merge_windows_path();
        environment
    }

    #[cfg(test)]
    fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            values: pairs
                .into_iter()
                .map(|(key, value)| (environment_key(&key), value))
                .collect(),
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.values.get(&environment_key(key)).map(String::as_str)
    }

    fn set(&mut self, key: &str, value: String) {
        self.values.insert(environment_key(key), value);
    }

    fn into_values(self) -> BTreeMap<String, String> {
        self.values
    }

    #[cfg(windows)]
    fn merge_windows_path(&mut self) {
        let current = self.get("PATH").map(str::to_owned);
        let user = registry_path(RegKey::predef(HKEY_CURRENT_USER))
            .map(|path| expand_windows_percent_variables(&path, self));
        let machine = registry_path(RegKey::predef(HKEY_LOCAL_MACHINE))
            .map(|path| expand_windows_percent_variables(&path, self));
        if let Some(path) = merge_path_values([current, user, machine]) {
            self.set("PATH", path);
        }
    }
}

struct EnvironmentResolver {
    base: Environment,
    configured: BTreeMap<String, String>,
    resolved: BTreeMap<String, String>,
    resolving: Vec<String>,
}

impl EnvironmentResolver {
    fn new(base: Environment, configured: BTreeMap<String, String>) -> Self {
        Self {
            base,
            configured: configured
                .into_iter()
                .map(|(key, value)| (environment_key(&key), value))
                .collect(),
            resolved: BTreeMap::new(),
            resolving: Vec::new(),
        }
    }

    fn resolve_configured(&mut self) -> Result<BTreeMap<String, String>> {
        let keys = self.configured.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            self.resolve_variable(&key)?;
        }
        Ok(self.resolved.clone())
    }

    fn expand(&mut self, input: &str) -> Result<String> {
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
            output.push_str(&self.resolve_variable(name)?);
            rest = &after[end + 1..];
        }
        output.push_str(rest);
        Ok(output)
    }

    fn resolve_variable(&mut self, name: &str) -> Result<String> {
        let key = environment_key(name);
        if let Some(value) = self.resolved.get(&key) {
            return Ok(value.clone());
        }
        let Some(raw) = self.configured.get(&key).cloned() else {
            return self
                .base
                .get(name)
                .map(str::to_owned)
                .ok_or_else(|| Error::MissingEnvironment(name.into()));
        };
        if self.resolving.contains(&key) {
            // PATH-style assignments commonly extend the inherited value with `${PATH}`.
            if self.resolving.last() == Some(&key) {
                return self
                    .base
                    .get(name)
                    .map(str::to_owned)
                    .ok_or_else(|| Error::MissingEnvironment(name.into()));
            }
            return Err(Error::InvalidConfig(format!(
                "cyclic environment reference involving {name}"
            )));
        }
        self.resolving.push(key.clone());
        let value = self.expand(&raw);
        self.resolving.pop();
        let value = value?;
        self.resolved.insert(key, value.clone());
        Ok(value)
    }
}

fn environment_key(key: &str) -> String {
    #[cfg(windows)]
    {
        key.to_ascii_uppercase()
    }
    #[cfg(not(windows))]
    {
        key.to_string()
    }
}

#[cfg(windows)]
fn registry_path(hive: RegKey) -> Option<String> {
    hive.open_subkey("Environment")
        .or_else(|_| {
            hive.open_subkey("SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment")
        })
        .ok()?
        .get_value("Path")
        .ok()
}

#[cfg(windows)]
fn merge_path_values(values: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for value in values.into_iter().flatten() {
        for entry in std::env::split_paths(&value) {
            if entry.as_os_str().is_empty() {
                continue;
            }
            let key = entry.to_string_lossy().to_ascii_lowercase();
            if seen.insert(key) {
                entries.push(entry);
            }
        }
    }
    std::env::join_paths(entries)
        .ok()
        .map(|value| value.to_string_lossy().into_owned())
}

#[cfg(windows)]
fn expand_windows_percent_variables(input: &str, environment: &Environment) -> String {
    let mut output = String::new();
    let mut rest = input;
    while let Some(start) = rest.find('%') {
        output.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            output.push('%');
            output.push_str(after);
            return output;
        };
        let name = &after[..end];
        if name.is_empty() {
            output.push_str("%%");
        } else if let Some(value) = environment.get(name) {
            output.push_str(value);
        } else {
            output.push('%');
            output.push_str(name);
            output.push('%');
        }
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    output
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

    #[test]
    fn stdio_environment_is_expanded_before_command_resolution() {
        let environment = Environment::from_pairs([
            ("PATH".to_string(), "inherited-path".to_string()),
            ("HOME".to_string(), "inherited-home".to_string()),
        ]);
        let config = McpConfig::from_slice_with_environment(
            br#"{"mcpServers":{"local":{"command":"${RUNNER}","args":["${TOKEN}"],"cwd":"${WORKSPACE}","env":{"RUNNER":"uvx","TOKEN":"secret","WORKSPACE":"tools","PATH":"${PATH};extra-path"}}}}"#,
            Some(Path::new("profile")),
            environment,
        )
        .unwrap();
        let McpServerConfig::Stdio(local) = &config.servers["local"] else {
            panic!()
        };
        assert_eq!(local.command, "uvx");
        assert_eq!(local.args, ["secret"]);
        assert_eq!(local.cwd.as_deref(), Some(Path::new("profile/tools")));
        assert_eq!(local.process_env["PATH"], "inherited-path;extra-path");
        assert_eq!(local.process_env["RUNNER"], "uvx");
    }
}
