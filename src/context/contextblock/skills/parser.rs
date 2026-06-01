//! Parse SKILL.md frontmatter and discover skill paths.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use super::errors::SkillError;
use super::models::SkillProperties;
use crate::utils::files::read_utf8_text;

static KEBAB_CASE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[a-z0-9]+(?:-[a-z0-9]+)*$").unwrap());

/// Return the SKILL.md path inside a skill directory.
pub fn find_skill_md(skill_dir: &Path) -> Result<PathBuf, SkillError> {
    let resolved = std::fs::canonicalize(skill_dir).unwrap_or_else(|_| skill_dir.to_path_buf());
    let skill_md = resolved.join("SKILL.md");
    if skill_md.is_file() {
        Ok(skill_md)
    } else {
        Err(SkillError::parse(format!(
            "Missing SKILL.md in skill directory: {}",
            resolved.display()
        )))
    }
}

/// Read and validate a skill's frontmatter.
pub fn read_properties(skill_dir: &Path) -> Result<SkillProperties, SkillError> {
    let skill_md_path = find_skill_md(skill_dir)?;
    let content = read_utf8_text(&skill_md_path).map_err(|e| {
        SkillError::parse(format!("Failed to read {}: {e}", skill_md_path.display()))
    })?;
    let frontmatter = extract_frontmatter(&content, &skill_md_path)?;
    let raw = parse_frontmatter(&frontmatter, &skill_md_path)?;
    build_skill_properties(raw, &skill_md_path)
}

fn extract_frontmatter(content: &str, skill_md_path: &Path) -> Result<String, SkillError> {
    let text = content.strip_prefix('\u{feff}').unwrap_or(content);
    let lines: Vec<&str> = text.lines().collect();
    if lines.first().map(|l| l.trim()) != Some("---") {
        return Err(SkillError::parse(format!(
            "SKILL.md must start with YAML frontmatter in {}",
            skill_md_path.display()
        )));
    }

    let mut end_line: Option<usize> = None;
    for (index, line) in lines.iter().enumerate().skip(1) {
        if line.trim() == "---" {
            end_line = Some(index);
            break;
        }
    }

    let end_line = end_line.ok_or_else(|| {
        SkillError::parse(format!(
            "Unclosed YAML frontmatter in {}",
            skill_md_path.display()
        ))
    })?;

    Ok(lines[1..end_line].join("\n"))
}

fn parse_frontmatter(
    frontmatter: &str,
    skill_md_path: &Path,
) -> Result<BTreeMap<String, Value>, SkillError> {
    let parsed: Value = serde_yaml::from_str(frontmatter).map_err(|exc| {
        SkillError::parse(format!(
            "Invalid YAML frontmatter in {}: {exc}",
            skill_md_path.display()
        ))
    })?;

    match parsed {
        Value::Null => Ok(BTreeMap::new()),
        Value::Object(map) => {
            let mut out = BTreeMap::new();
            for (k, v) in map {
                out.insert(k, v);
            }
            Ok(out)
        }
        _ => Err(SkillError::parse(format!(
            "Frontmatter must be a key-value map in {}",
            skill_md_path.display()
        ))),
    }
}

fn build_skill_properties(
    raw: BTreeMap<String, Value>,
    skill_md_path: &Path,
) -> Result<SkillProperties, SkillError> {
    let mut errors: Vec<String> = Vec::new();

    let name = clean_string(raw.get("name"));
    let description = clean_string(raw.get("description"));
    let license = clean_string(raw.get("license"));
    let compatibility = clean_string(raw.get("compatibility"));
    let allowed_tools = clean_string(raw.get("allowed-tools"));

    match name.as_deref() {
        None => errors.push("Missing required field: name".to_string()),
        Some(n) if !KEBAB_CASE.is_match(n) => {
            errors.push("Field `name` must be kebab-case, for example: my-skill".to_string())
        }
        _ => {}
    }

    if description.is_none() {
        errors.push("Missing required field: description".to_string());
    }

    let metadata_raw = raw.get("metadata");
    let metadata: BTreeMap<String, String> = match metadata_raw {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(k, v)| (k.clone(), value_to_display_string(v)))
            .collect(),
        Some(_) => {
            errors.push("Field `metadata` must be an object".to_string());
            BTreeMap::new()
        }
    };

    if !errors.is_empty() {
        return Err(SkillError::validation(
            format!("Invalid skill frontmatter in {}", skill_md_path.display()),
            errors,
        ));
    }

    Ok(SkillProperties {
        name: name.unwrap_or_default(),
        description: description.unwrap_or_default(),
        license,
        compatibility,
        allowed_tools,
        metadata,
    })
}

fn clean_string(value: Option<&Value>) -> Option<String> {
    let v = value?;
    if v.is_null() {
        return None;
    }
    let text = value_to_display_string(v);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn value_to_display_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
