//! Generate `<available_skills>` XML prompt block for agent system prompts.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::errors::SkillError;
use super::parser::{find_skill_md, read_properties};
use crate::context::contextblock::xml::html_escape;

pub const DEFAULT_DESCRIPTION_MAX_CHARS: usize = 500;
const TRUNCATION_MARKER: &str = "... [truncated]";

/// Discover skill directories under *skills_root*.
pub fn discover_skills(skills_root: &Path) -> Vec<PathBuf> {
    if !skills_root.exists() {
        return Vec::new();
    }

    let mut dirs: Vec<PathBuf> = Vec::new();
    if skills_root.join("SKILL.md").is_file() {
        dirs.push(skills_root.to_path_buf());
        return dirs;
    }

    let Ok(entries) = std::fs::read_dir(skills_root) else {
        return dirs;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && path.join("SKILL.md").is_file() {
            dirs.push(path);
        }
    }
    dirs
}

/// Generate the `<available_skills>` XML block for inclusion in agent prompts.
pub fn to_prompt(skill_dirs: &[PathBuf]) -> Result<String> {
    to_prompt_with_limit(skill_dirs, DEFAULT_DESCRIPTION_MAX_CHARS)
}

pub fn to_prompt_with_limit(
    skill_dirs: &[PathBuf],
    max_description_chars: usize,
) -> Result<String> {
    if max_description_chars == 0 {
        bail!("max_description_chars must be positive");
    }

    if skill_dirs.is_empty() {
        return Ok("<available_skills>\n</available_skills>".to_string());
    }

    let mut lines: Vec<String> = vec!["<available_skills>".to_string()];

    for skill_dir in skill_dirs {
        let resolved = std::fs::canonicalize(skill_dir).unwrap_or_else(|_| skill_dir.clone());
        let props = read_properties(&resolved).map_err(anyhow_from_skill)?;

        lines.push("<skill>".to_string());
        lines.push("<name>".to_string());
        lines.push(html_escape(&props.name));
        lines.push("</name>".to_string());
        lines.push("<description>".to_string());
        let description = truncate_description(&props.description, max_description_chars);
        lines.push(html_escape(&description));
        lines.push("</description>".to_string());

        let skill_md_path = find_skill_md(&resolved).map_err(anyhow_from_skill)?;
        lines.push("<location>".to_string());
        lines.push(skill_md_path.display().to_string());
        lines.push("</location>".to_string());

        lines.push("</skill>".to_string());
    }

    lines.push("</available_skills>".to_string());
    Ok(lines.join("\n"))
}

fn truncate_description(description: &str, max_chars: usize) -> String {
    let text = description.trim();
    let text_chars: Vec<char> = text.chars().collect();
    if text_chars.len() <= max_chars {
        return text.to_string();
    }

    let marker_chars: Vec<char> = TRUNCATION_MARKER.chars().collect();
    if max_chars <= marker_chars.len() {
        return marker_chars[..max_chars].iter().collect();
    }

    let prefix_len = max_chars - marker_chars.len();
    let prefix: String = text_chars[..prefix_len].iter().collect();
    format!("{}{}", prefix.trim_end(), TRUNCATION_MARKER)
}

fn anyhow_from_skill(err: SkillError) -> anyhow::Error {
    anyhow::Error::new(err)
}
