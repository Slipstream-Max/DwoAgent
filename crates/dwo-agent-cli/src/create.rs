//! Interactive creation helpers for agent and supervisor configuration.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_yaml::{Mapping, Value};

use dwo_agent_supervisor::{SupervisorConfig, default_supervisor_config_path};

#[derive(Debug, Clone)]
pub struct CreateAgentOptions {
    pub name: String,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct CreateSupervisorOptions {
    pub default: bool,
    pub path: Option<PathBuf>,
}

pub fn create_agent_profile(options: CreateAgentOptions) -> Result<PathBuf> {
    let name = normalize_name(&options.name)?;
    let profile_dir = match options.path {
        Some(path) => path,
        None => default_agent_profile_root()?.join("profiles").join(&name),
    };

    if profile_dir.exists() {
        bail!("agent profile already exists: {}", profile_dir.display());
    }

    let description = prompt_default("Description", &format!("{name} Dwo Agent profile"))?;
    let provider = prompt_default("Model provider", "deepseek")?;
    let model_name = prompt_default("Model name", "deepseek-v4-pro")?;
    let model_id = prompt_default("Provider model id", &model_name)?;
    let api_key_env = prompt_default("API key env var", "DEEPSEEK_API_KEY")?;

    fs::create_dir_all(profile_dir.join("resources").join("prompt"))
        .with_context(|| format!("create {}", profile_dir.display()))?;
    fs::create_dir_all(profile_dir.join("resources").join("skills"))
        .with_context(|| format!("create {}", profile_dir.display()))?;
    fs::create_dir_all(profile_dir.join("runtime"))
        .with_context(|| format!("create {}", profile_dir.display()))?;

    write_yaml(
        &profile_dir.join("agent.yaml"),
        agent_yaml(
            &name,
            &description,
            &provider,
            &model_name,
            &model_id,
            &api_key_env,
        ),
    )?;
    fs::write(
        profile_dir
            .join("resources")
            .join("prompt")
            .join("system.md"),
        format!("You are {name}, a helpful Dwo Agent profile.\n"),
    )
    .context("write system prompt")?;
    fs::write(
        profile_dir
            .join("resources")
            .join("prompt")
            .join("AGENTS.md"),
        "# Agent Instructions\n\nAdd profile-specific instructions here.\n",
    )
    .context("write AGENTS.md")?;

    println!("Created agent profile: {}", profile_dir.display());
    Ok(profile_dir)
}

pub fn create_supervisor_config(options: CreateSupervisorOptions) -> Result<PathBuf> {
    let path = options.path.unwrap_or_else(default_supervisor_config_path);
    if path.exists() {
        bail!("supervisor config already exists: {}", path.display());
    }

    let mut config = SupervisorConfig::default();
    if !options.default {
        config.endpoint.websocket_bind_addr = prompt_default(
            "Supervisor WebSocket bind address",
            &config.endpoint.websocket_bind_addr,
        )?;
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    write_yaml(
        &path,
        serde_yaml::to_value(&config).context("serialize supervisor config")?,
    )?;
    println!("Created supervisor config: {}", path.display());
    Ok(path)
}

fn agent_yaml(
    name: &str,
    description: &str,
    provider: &str,
    model_name: &str,
    model_id: &str,
    api_key_env: &str,
) -> Value {
    let mut root = Mapping::new();
    root.insert(Value::from("agentId"), Value::from(name));
    root.insert(Value::from("name"), Value::from(name));
    root.insert(Value::from("description"), Value::from(description));
    root.insert(Value::from("policyMode"), Value::from("confirm"));
    root.insert(Value::from("externalSkillsDirs"), Value::Sequence(vec![]));
    root.insert(Value::from("externalRuleFiles"), Value::Sequence(vec![]));
    root.insert(Value::from("tools"), tools_yaml());
    root.insert(
        Value::from("model"),
        model_yaml(provider, model_name, model_id, api_key_env),
    );
    root.insert(Value::from("channels"), channels_yaml());
    Value::Mapping(root)
}

fn tools_yaml() -> Value {
    let mut tools = Mapping::new();
    tools.insert(Value::from("fileEdit"), Value::from("enable"));
    tools.insert(Value::from("terminal"), Value::from("enable"));
    tools.insert(Value::from("subagent"), Value::from("enable"));
    Value::Mapping(tools)
}

fn model_yaml(provider: &str, model_name: &str, model_id: &str, api_key_env: &str) -> Value {
    let mut model = Mapping::new();
    model.insert(Value::from("defaultModelId"), Value::from(model_name));

    let mut entry = Mapping::new();
    entry.insert(Value::from("modelName"), Value::from(model_name));
    entry.insert(Value::from("provider"), Value::from(provider));
    entry.insert(Value::from("modelId"), Value::from(model_id));
    entry.insert(Value::from("apiKeyEnv"), Value::from(api_key_env));
    entry.insert(Value::from("compactThreshold"), Value::from(0.8));

    model.insert(
        Value::from("models"),
        Value::Sequence(vec![Value::Mapping(entry)]),
    );
    Value::Mapping(model)
}

fn channels_yaml() -> Value {
    let mut channels = Mapping::new();
    let mut weixin = Mapping::new();
    weixin.insert(Value::from("enabled"), Value::from(false));
    let mut feishu = Mapping::new();
    feishu.insert(Value::from("enabled"), Value::from(false));
    channels.insert(Value::from("weixin"), Value::Mapping(weixin));
    channels.insert(Value::from("feishu"), Value::Mapping(feishu));
    Value::Mapping(channels)
}

fn write_yaml(path: &Path, value: Value) -> Result<()> {
    let text = serde_yaml::to_string(&value).context("serialize yaml")?;
    fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

fn prompt_default(prompt: &str, default: &str) -> Result<String> {
    print!("{prompt} [{default}]: ");
    io::stdout().flush().context("flush prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read prompt answer")?;
    let trimmed = answer.trim();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn normalize_name(name: &str) -> Result<String> {
    let trimmed = name.trim().to_ascii_lowercase();
    if trimmed.is_empty()
        || !trimmed
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
    {
        bail!("agent name must contain only lowercase letters, digits, `-`, or `_`");
    }
    Ok(trimmed)
}

fn default_agent_profile_root() -> Result<PathBuf> {
    Ok(home_dir()?.join(".dwoagent"))
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .context("cannot resolve home directory")
}
