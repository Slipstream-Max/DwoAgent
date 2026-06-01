//! Build and inject agent context from `agent-structure`.

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use crate::config::models::AgentTools;

use super::contextblock::{
    agent_prompt::build_agent_prompt,
    env_context::build_env_context,
    mcp::build_available_mcp_servers,
    rule::build_rule,
    skills::prompt::{discover_skills, to_prompt as skills_to_prompt},
    tools::build_tools,
    xml::join_agent_context,
};

/// Load and merge system prompt blocks for agent context. Returns the system
/// message as a JSON object matching Python's `{"role": "system", "content": str}`.
pub fn build_agent_system_context(
    agent_structure_dir: &Path,
    agent_id: &str,
    cwd: &str,
    mcp_server_names: &[String],
    tools: &AgentTools,
) -> Result<Value> {
    let structure_dir = std::fs::canonicalize(agent_structure_dir)
        .unwrap_or_else(|_| agent_structure_dir.to_path_buf());
    let resources_dir = structure_dir.join("resources");
    let skill_dirs = discover_skills(&resources_dir.join("skills"));

    let blocks: Vec<String> = vec![
        build_agent_prompt(&resources_dir, agent_id)?,
        build_rule(&resources_dir, agent_id, cwd)?,
        build_tools(tools),
        build_available_mcp_servers(mcp_server_names),
        skills_to_prompt(&skill_dirs)?,
        build_env_context(cwd),
    ];

    Ok(json!({
        "role": "system",
        "content": join_agent_context(&blocks),
    }))
}
