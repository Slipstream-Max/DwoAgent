//! Build the `<rule>` context block.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::xml::{block, tag, text_block};
use crate::utils::files::read_utf8_text;

const WORKSPACE_RULE_FILENAMES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

pub fn build_rule(resources_dir: &Path, agent_id: &str, cwd: &str) -> Result<String> {
    let mut chunks: Vec<String> = Vec::new();

    let agent_rule_path = resources_dir
        .join("agents")
        .join(format!("{agent_id}.rule.md"));
    if let Some(text) = read_optional_trimmed(&agent_rule_path)? {
        chunks.push(rule_source_block("agent_rule", &agent_rule_path, &text));
    }

    let workspace_root = resolve_or_noop(Path::new(cwd));
    for filename in WORKSPACE_RULE_FILENAMES {
        let rule_path = workspace_root.join(filename);
        if let Some(text) = read_optional_trimmed(&rule_path)? {
            chunks.push(rule_source_block("workspace_rule", &rule_path, &text));
        }
    }

    Ok(block("rule", &chunks.join("\n\n")))
}

fn rule_source_block(name: &str, path: &Path, content: &str) -> String {
    let resolved = resolve_or_noop(path);
    let body = [
        tag("source", &resolved.display().to_string()),
        text_block("content", content),
    ]
    .join("\n");
    block(name, &body)
}

fn read_optional_trimmed(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = read_utf8_text(path)?;
    let trimmed = text.trim();
    Ok(if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    })
}

/// `Path::canonicalize` requires the path to exist; mirror Python's
/// `Path(...).resolve()` which falls back to a best-effort absolute form.
fn resolve_or_noop(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
