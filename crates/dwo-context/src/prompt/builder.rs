use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::env_watcher::DynamicEnvironmentSnapshot;

use super::environment::EnvironmentSnapshot;
use super::mcp::McpSnapshot;
use super::skills::{self, SkillSnapshot};
use super::{ChannelCapabilitySnapshot, xml_block};

const FALLBACK_AGENT_PROMPT: &str =
    "You are an agent. Follow the user request and use the available tools when needed.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfilePaths {
    pub root: PathBuf,
    pub resource: PathBuf,
    pub system_prompt: PathBuf,
    pub agents_rules: PathBuf,
    pub skills: PathBuf,
    pub mcp: PathBuf,
}

impl AgentProfilePaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let resource = root.join("resource");
        Self {
            system_prompt: resource.join("prompts").join("System.md"),
            agents_rules: resource.join("prompts").join("AGENTS.md"),
            skills: resource.join("skills"),
            mcp: resource.join("mcp.json"),
            root,
            resource,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleSnapshot {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSnapshot {
    pub agent_prompt: String,
    pub rules: Vec<RuleSnapshot>,
    pub skills: Vec<SkillSnapshot>,
    #[serde(default)]
    pub channels: Vec<ChannelCapabilitySnapshot>,
    pub mcp: Option<McpSnapshot>,
    pub environment: EnvironmentSnapshot,
}

impl PromptSnapshot {
    pub fn dynamic(&self) -> DynamicEnvironmentSnapshot {
        DynamicEnvironmentSnapshot {
            agent_prompt: self.agent_prompt.clone(),
            rules: self.rules.clone(),
            skills: self.skills.clone(),
            channels: self.channels.clone(),
            mcp: self.mcp.clone(),
            environment: self.environment.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemPromptBlock {
    pub content: String,
    pub snapshot: Option<PromptSnapshot>,
}

impl SystemPromptBlock {
    pub fn is_initialized(&self) -> bool {
        self.snapshot.is_some() && !self.content.trim().is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct SystemPromptBuilder {
    profile: Option<AgentProfilePaths>,
    cwd: PathBuf,
    tool_prompt: Option<String>,
}

impl SystemPromptBuilder {
    pub fn new(profile_root: Option<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            profile: profile_root.map(AgentProfilePaths::new),
            cwd: cwd.into(),
            tool_prompt: None,
        }
    }

    pub fn with_tool_prompt(mut self, tool_prompt: impl Into<String>) -> Self {
        self.tool_prompt = Some(tool_prompt.into());
        self
    }

    pub fn profile(&self) -> Option<&AgentProfilePaths> {
        self.profile.as_ref()
    }

    pub fn build_initial(&self) -> Result<SystemPromptBlock, PromptBuildError> {
        self.build_current()
    }

    pub fn rebuild(&self) -> Result<SystemPromptBlock, PromptBuildError> {
        self.build_current()
    }

    pub fn scan_dynamic(&self) -> Result<DynamicEnvironmentSnapshot, PromptBuildError> {
        Ok(DynamicEnvironmentSnapshot {
            agent_prompt: self.read_agent_prompt()?,
            rules: self.read_agents_rules()?,
            skills: self.read_skills()?,
            channels: self.read_channels(),
            mcp: self.read_mcp()?,
            environment: EnvironmentSnapshot::capture(&self.cwd),
        })
    }

    fn build_current(&self) -> Result<SystemPromptBlock, PromptBuildError> {
        let dynamic = self.scan_dynamic()?;
        let snapshot = PromptSnapshot {
            agent_prompt: dynamic.agent_prompt,
            rules: dynamic.rules,
            skills: dynamic.skills,
            channels: dynamic.channels,
            mcp: dynamic.mcp,
            environment: dynamic.environment,
        };
        let content = render_prompt(&snapshot, self.tool_prompt.as_deref());
        Ok(SystemPromptBlock {
            content,
            snapshot: Some(snapshot),
        })
    }

    fn read_agent_prompt(&self) -> Result<String, PromptBuildError> {
        let Some(profile) = &self.profile else {
            return Ok(FALLBACK_AGENT_PROMPT.to_string());
        };
        read_required_nonempty(&profile.system_prompt)
    }

    fn read_agents_rules(&self) -> Result<Vec<RuleSnapshot>, PromptBuildError> {
        let mut rules = Vec::new();
        if let Some(profile) = &self.profile
            && let Some(content) = read_optional_nonempty(&profile.agents_rules)?
        {
            rules.push(RuleSnapshot {
                path: resolve_or_original(&profile.agents_rules),
                content,
            });
        }
        let cwd_rules = self.cwd.join("AGENTS.md");
        if let Some(content) = read_optional_nonempty(&cwd_rules)? {
            rules.push(RuleSnapshot {
                path: resolve_or_original(&cwd_rules),
                content,
            });
        }
        Ok(rules)
    }

    fn read_skills(&self) -> Result<Vec<SkillSnapshot>, PromptBuildError> {
        let Some(profile) = &self.profile else {
            return Ok(Vec::new());
        };
        skills::scan(&profile.skills).map_err(|error| PromptBuildError::Skills(error.to_string()))
    }

    fn read_channels(&self) -> Vec<ChannelCapabilitySnapshot> {
        self.profile.as_ref().map_or_else(Vec::new, |profile| {
            ChannelCapabilitySnapshot::scan(&profile.root)
        })
    }

    fn read_mcp(&self) -> Result<Option<McpSnapshot>, PromptBuildError> {
        let Some(profile) = &self.profile else {
            return Ok(None);
        };
        McpSnapshot::read(&profile.mcp).map_err(|source| PromptBuildError::Read {
            path: profile.mcp.clone(),
            source,
        })
    }
}

fn render_prompt(snapshot: &PromptSnapshot, tool_prompt: Option<&str>) -> String {
    let mut blocks = vec![xml_block("agent_prompt", &snapshot.agent_prompt)];
    if !snapshot.rules.is_empty() {
        let rules = snapshot
            .rules
            .iter()
            .map(|rule| format!("source: {}\n{}", rule.path.display(), rule.content))
            .collect::<Vec<_>>()
            .join("\n\n");
        blocks.push(xml_block("rules", &rules));
    }
    if let Some(tool_prompt) = tool_prompt.filter(|prompt| !prompt.trim().is_empty()) {
        blocks.push(format!("<tools>\n{}\n</tools>", tool_prompt.trim()));
    }
    let skills = skills::render_catalog(&snapshot.skills);
    if !skills.is_empty() {
        blocks.push(skills);
    }
    if let Some(mcp) = &snapshot.mcp {
        blocks.push(mcp.render());
    }
    blocks.extend(
        snapshot
            .channels
            .iter()
            .map(ChannelCapabilitySnapshot::render),
    );
    blocks.push(snapshot.environment.render());
    format!("<agent_context>\n{}\n</agent_context>", blocks.join("\n\n"))
}

fn read_required_nonempty(path: &Path) -> Result<String, PromptBuildError> {
    if !path.is_file() {
        return Err(PromptBuildError::Missing(path.to_path_buf()));
    }
    let content = read_utf8(path)?;
    let content = content.trim();
    if content.is_empty() {
        return Err(PromptBuildError::Empty(path.to_path_buf()));
    }
    Ok(content.to_string())
}

fn read_optional_nonempty(path: &Path) -> Result<Option<String>, PromptBuildError> {
    if !path.is_file() {
        return Ok(None);
    }
    let content = read_utf8(path)?;
    let content = content.trim();
    Ok((!content.is_empty()).then(|| content.to_string()))
}

fn read_utf8(path: &Path) -> Result<String, PromptBuildError> {
    let bytes = std::fs::read(path).map_err(|source| PromptBuildError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    String::from_utf8(bytes).map_err(|source| PromptBuildError::Utf8 {
        path: path.to_path_buf(),
        source,
    })
}

fn resolve_or_original(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[derive(Debug, thiserror::Error)]
pub enum PromptBuildError {
    #[error("missing agent prompt: {0}")]
    Missing(PathBuf),
    #[error("agent prompt is empty: {0}")]
    Empty(PathBuf),
    #[error("read prompt resource {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("prompt resource is not UTF-8: {path}")]
    Utf8 {
        path: PathBuf,
        #[source]
        source: std::string::FromUtf8Error,
    },
    #[error("scan skills: {0}")]
    Skills(String),
}
