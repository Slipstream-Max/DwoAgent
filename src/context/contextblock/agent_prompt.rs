//! Build the `<agent_prompt>` context block.

use std::path::Path;

use anyhow::{Result, bail};

use super::xml::text_block;
use crate::utils::files::read_utf8_text;

pub fn build_agent_prompt(resources_dir: &Path, _agent_id: &str) -> Result<String> {
    let prompt_path = resources_dir.join("prompt").join("system.md");
    let system_prompt = read_required_text(&prompt_path)?;
    let trimmed = system_prompt.trim();
    if trimmed.is_empty() {
        bail!("Agent prompt file is empty: {}", prompt_path.display());
    }
    Ok(text_block("agent_prompt", trimmed))
}

fn read_required_text(path: &Path) -> Result<String> {
    if !path.is_file() {
        bail!("Missing agent prompt file. Expected: {}", path.display());
    }
    read_utf8_text(path)
}
