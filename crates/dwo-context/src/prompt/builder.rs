use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::env_watcher::DynamicEnvironmentSnapshot;

use super::environment::EnvironmentSnapshot;
use super::mcp::McpSnapshot;
use super::skills::{self, SkillSnapshot};
use super::{ChannelCapabilitySnapshot, xml_block, xml_escape};

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
            mcp: resource.join("mcp/mcp.json"),
            root,
            resource,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalRuleFile {
    pub path: PathBuf,
    pub pwd: PathBuf,
}

impl ExternalRuleFile {
    pub fn new(path: impl Into<PathBuf>, pwd: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            pwd: pwd.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleSnapshot {
    pub path: PathBuf,
    #[serde(default)]
    pub pwd: PathBuf,
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
    external_skill_dirs: Arc<RwLock<Vec<PathBuf>>>,
    profile_rule_files: Arc<RwLock<Vec<ExternalRuleFile>>>,
    session_rule_files: Arc<RwLock<Vec<ExternalRuleFile>>>,
    tool_prompt: Option<String>,
    subsession_prompt: Option<String>,
    automation_prompt: Option<String>,
    channel_prompt: Option<String>,
}

impl SystemPromptBuilder {
    pub fn new(profile_root: Option<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            profile: profile_root.map(AgentProfilePaths::new),
            cwd: cwd.into(),
            external_skill_dirs: Arc::new(RwLock::new(Vec::new())),
            profile_rule_files: Arc::new(RwLock::new(Vec::new())),
            session_rule_files: Arc::new(RwLock::new(Vec::new())),
            tool_prompt: None,
            subsession_prompt: None,
            automation_prompt: None,
            channel_prompt: None,
        }
    }

    pub fn with_tool_prompt(mut self, tool_prompt: impl Into<String>) -> Self {
        self.tool_prompt = Some(tool_prompt.into());
        self
    }

    pub fn with_external_skill_dirs(mut self, dirs: Arc<RwLock<Vec<PathBuf>>>) -> Self {
        self.external_skill_dirs = dirs;
        self
    }

    pub fn with_external_rule_files(
        mut self,
        profile: Arc<RwLock<Vec<ExternalRuleFile>>>,
        session: Arc<RwLock<Vec<ExternalRuleFile>>>,
    ) -> Self {
        self.profile_rule_files = profile;
        self.session_rule_files = session;
        self
    }

    pub fn with_subsession_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.subsession_prompt = Some(prompt.into());
        self
    }

    pub fn with_automation_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.automation_prompt = Some(prompt.into());
        self
    }

    pub fn with_channel_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.channel_prompt = Some(prompt.into());
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

    pub fn scan_skills(&self) -> Result<Vec<SkillSnapshot>, PromptBuildError> {
        self.read_skills()
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
        let content = render_prompt(
            &snapshot,
            self.tool_prompt.as_deref(),
            self.subsession_prompt.as_deref(),
            self.automation_prompt.as_deref(),
            self.channel_prompt.as_deref(),
        );
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
                pwd: resolve_or_original(&profile.root),
                content,
            });
        }
        let cwd_rules = self.cwd.join("AGENTS.md");
        if let Some(content) = read_optional_nonempty(&cwd_rules)? {
            rules.push(RuleSnapshot {
                path: resolve_or_original(&cwd_rules),
                pwd: resolve_or_original(&self.cwd),
                content,
            });
        }
        let project_rules = self.cwd.join(".agents").join("AGENTS.md");
        if let Some(content) = read_optional_nonempty(&project_rules)? {
            rules.push(RuleSnapshot {
                path: resolve_or_original(&project_rules),
                pwd: resolve_or_original(&self.cwd),
                content,
            });
        }
        let profile_rule_files = self
            .profile_rule_files
            .read()
            .expect("external rule files lock poisoned")
            .clone();
        let session_rule_files = self
            .session_rule_files
            .read()
            .expect("session external rule files lock poisoned")
            .clone();
        for source in profile_rule_files.iter().chain(&session_rule_files) {
            if let Some(content) = read_optional_nonempty(&source.path)? {
                let path = resolve_or_original(&source.path);
                if let Some(existing) = rules.iter_mut().find(|rule| rule.path == path) {
                    existing.pwd = resolve_or_original(&source.pwd);
                    existing.content = content;
                } else {
                    rules.push(RuleSnapshot {
                        path,
                        pwd: resolve_or_original(&source.pwd),
                        content,
                    });
                }
            }
        }
        Ok(rules)
    }

    fn read_skills(&self) -> Result<Vec<SkillSnapshot>, PromptBuildError> {
        let mut skills = Vec::new();
        if let Some(profile) = &self.profile {
            skills.extend(
                skills::scan(&profile.skills)
                    .map_err(|error| PromptBuildError::Skills(error.to_string()))?,
            );
        }
        let external = self
            .external_skill_dirs
            .read()
            .expect("external skill dirs lock poisoned")
            .clone();
        for dir in external {
            for skill in
                skills::scan(&dir).map_err(|error| PromptBuildError::Skills(error.to_string()))?
            {
                skills.retain(|existing| existing.name != skill.name);
                skills.push(skill);
            }
        }
        let project_skills = self.cwd.join(".agents").join("skills");
        for project in skills::scan(&project_skills)
            .map_err(|error| PromptBuildError::Skills(error.to_string()))?
        {
            skills.retain(|existing| existing.name != project.name);
            skills.push(project);
        }
        skills.sort_by(|left, right| left.name.cmp(&right.name).then(left.path.cmp(&right.path)));
        Ok(skills)
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

fn render_prompt(
    snapshot: &PromptSnapshot,
    tool_prompt: Option<&str>,
    subsession_prompt: Option<&str>,
    automation_prompt: Option<&str>,
    channel_prompt: Option<&str>,
) -> String {
    let mut blocks = vec![xml_block("agent_prompt", &snapshot.agent_prompt)];
    if !snapshot.rules.is_empty() {
        let rules = snapshot
            .rules
            .iter()
            .map(|rule| {
                format!(
                    "source: {}\npwd: {}\n{}",
                    rule.path.display(),
                    rule.pwd.display(),
                    rule.content
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        blocks.push(xml_block("rules", &rules));
    }
    if let Some(tool_prompt) = tool_prompt.filter(|prompt| !prompt.trim().is_empty()) {
        blocks.push(xml_block("tools", tool_prompt));
    }
    if let Some(prompt) = subsession_prompt.filter(|prompt| !prompt.trim().is_empty()) {
        blocks.push(xml_block("subsession", prompt));
    }
    if let Some(prompt) = automation_prompt.filter(|prompt| !prompt.trim().is_empty()) {
        blocks.push(xml_block("automation", prompt));
    }
    if channel_prompt.is_some_and(|prompt| !prompt.trim().is_empty())
        || !snapshot.channels.is_empty()
    {
        let mut channel_blocks = Vec::new();
        if let Some(prompt) = channel_prompt.filter(|prompt| !prompt.trim().is_empty()) {
            channel_blocks.push(xml_escape(prompt.trim()));
        }
        channel_blocks.extend(
            snapshot
                .channels
                .iter()
                .map(ChannelCapabilitySnapshot::render),
        );
        blocks.push(format!(
            "<channels>\n{}\n</channels>",
            channel_blocks.join("\n\n")
        ));
    }
    let skills = skills::render_catalog(&snapshot.skills);
    if !skills.is_empty() {
        blocks.push(skills);
    }
    if let Some(mcp) = &snapshot.mcp {
        blocks.push(mcp.render());
    }
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
