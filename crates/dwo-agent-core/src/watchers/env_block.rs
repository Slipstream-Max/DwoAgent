//! Env block watcher content builder.

use std::path::PathBuf;

use anyhow::Result;

use crate::context::contextblock::{
    env_context::build_env_context, mcp::build_mcp, rule::build_rule,
    skills::prompt::build_available_skills, xml::block,
};

#[derive(Debug, Clone)]
pub struct EnvBlockWatcher {
    agent_structure_dir: PathBuf,
    agent_id: String,
    cwd: String,
    external_skills_dirs: Vec<PathBuf>,
    external_rule_files: Vec<PathBuf>,
}

impl EnvBlockWatcher {
    pub fn new(
        agent_structure_dir: PathBuf,
        agent_id: impl Into<String>,
        cwd: impl Into<String>,
        external_skills_dirs: Vec<PathBuf>,
        external_rule_files: Vec<PathBuf>,
    ) -> Self {
        Self {
            agent_structure_dir,
            agent_id: agent_id.into(),
            cwd: cwd.into(),
            external_skills_dirs,
            external_rule_files,
        }
    }

    pub fn build_content(&self) -> Result<String> {
        let resources_dir = self.agent_structure_dir.join("resources");
        let blocks = vec![
            build_rule(
                &resources_dir,
                &self.agent_id,
                &self.cwd,
                &self.external_rule_files,
            )?,
            build_mcp(&resources_dir),
            build_available_skills(&resources_dir, &self.cwd, &self.external_skills_dirs)?,
            build_env_context(&self.cwd),
        ];
        Ok(block("env_block", &blocks.join("\n\n")))
    }
}
