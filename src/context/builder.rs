//! Build and inject agent context from `agent-structure`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};

use crate::config::models::AgentTools;

use super::contextblock::{
    agent_prompt::build_agent_prompt, env_context::build_env_context, mcp::build_mcp,
    rule::build_rule, skills::prompt::build_available_skills, tools::build_tools,
    xml::join_agent_context,
};

/// Load and merge system prompt blocks for agent context. Returns the system
/// message as a JSON object matching Python's `{"role": "system", "content": str}`.
pub fn build_agent_system_context(
    agent_structure_dir: &Path,
    agent_id: &str,
    cwd: &str,
    tools: &AgentTools,
    external_skills_dirs: &[PathBuf],
    external_rule_files: &[PathBuf],
) -> Result<Value> {
    let structure_dir = std::fs::canonicalize(agent_structure_dir)
        .unwrap_or_else(|_| agent_structure_dir.to_path_buf());
    let resources_dir = structure_dir.join("resources");

    let blocks: Vec<String> = vec![
        build_agent_prompt(&resources_dir, agent_id)?,
        build_rule(&resources_dir, agent_id, cwd, external_rule_files)?,
        build_tools(tools),
        build_mcp(&resources_dir),
        build_available_skills(&resources_dir, cwd, external_skills_dirs)?,
        build_env_context(cwd),
    ];

    Ok(json!({
        "role": "system",
        "content": join_agent_context(&blocks),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::AgentTools;

    #[test]
    fn system_context_includes_agent_and_workspace_skills_and_rules() {
        let temp = tempfile::tempdir().unwrap();
        let agent_dir = temp.path().join("agent");
        let workspace_dir = temp.path().join("workspace");
        let external_dir = temp.path().join("external");
        let agent_resources = agent_dir.join("resources");

        std::fs::create_dir_all(agent_resources.join("prompt")).unwrap();
        std::fs::create_dir_all(agent_resources.join("skills").join("agent-skill")).unwrap();
        std::fs::create_dir_all(external_dir.join("skills").join("external-skill")).unwrap();
        std::fs::create_dir_all(
            workspace_dir
                .join(".agent")
                .join("skills")
                .join("workspace-skill"),
        )
        .unwrap();
        std::fs::write(
            agent_resources.join("prompt").join("system.md"),
            "Agent prompt",
        )
        .unwrap();
        std::fs::write(
            agent_resources.join("prompt").join("AGENTS.md"),
            "Agent rule",
        )
        .unwrap();
        std::fs::write(
            agent_resources
                .join("skills")
                .join("agent-skill")
                .join("SKILL.md"),
            "---\nname: agent-skill\ndescription: Agent folder skill\n---\n",
        )
        .unwrap();
        std::fs::write(
            external_dir
                .join("skills")
                .join("external-skill")
                .join("SKILL.md"),
            "---\nname: external-skill\ndescription: External folder skill\n---\n",
        )
        .unwrap();
        std::fs::write(
            workspace_dir
                .join(".agent")
                .join("skills")
                .join("workspace-skill")
                .join("SKILL.md"),
            "---\nname: workspace-skill\ndescription: Workspace folder skill\n---\n",
        )
        .unwrap();
        let external_rule = external_dir.join("external.rule.md");
        std::fs::write(&external_rule, "External rule").unwrap();
        std::fs::write(
            workspace_dir.join(".agent").join("AGENTS.md"),
            "Dot agent rule",
        )
        .unwrap();
        std::fs::write(workspace_dir.join("AGENTS.md"), "Root agent rule").unwrap();
        std::fs::write(agent_resources.join("mcp.json"), "{}").unwrap();

        let message = build_agent_system_context(
            &agent_dir,
            "test-agent",
            &workspace_dir.to_string_lossy(),
            &AgentTools::default(),
            &[external_dir.join("skills")],
            &[external_rule],
        )
        .unwrap();
        let content = message["content"].as_str().unwrap();

        assert!(content.contains("Agent rule"));
        assert!(content.contains("External rule"));
        assert!(content.contains("Dot agent rule"));
        assert!(content.contains("Root agent rule"));
        assert!(content.contains("<name>\nagent-skill\n</name>"));
        assert!(content.contains("<name>\nexternal-skill\n</name>"));
        assert!(content.contains("<name>\nworkspace-skill\n</name>"));
        assert!(content.contains("<mcp>"));
        assert!(content.contains("<config>"));
        assert!(content.contains("mcp.json"));
        assert!(content.contains("mcporter --version"));
        assert!(content.contains("mcporter --config"));
        assert!(content.contains("--schema --json"));
    }
}
