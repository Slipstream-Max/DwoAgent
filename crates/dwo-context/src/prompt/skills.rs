use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{stable_fingerprint, xml_escape};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSnapshot {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub fingerprint: String,
}

#[derive(Debug, Default, Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
}

pub(crate) fn scan(skills_dir: &Path) -> Result<Vec<SkillSnapshot>, SkillScanError> {
    if !skills_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries = std::fs::read_dir(skills_dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut skills = Vec::new();
    for entry in entries {
        let path = entry.path().join("SKILL.md");
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path)?;
        let text = std::str::from_utf8(&bytes).map_err(|source| SkillScanError::Utf8 {
            path: path.clone(),
            source,
        })?;
        let metadata = parse_frontmatter(text).map_err(|source| SkillScanError::Frontmatter {
            path: path.clone(),
            source,
        })?;
        let fallback_name = entry.file_name().to_string_lossy().into_owned();
        skills.push(SkillSnapshot {
            name: metadata.name.unwrap_or(fallback_name),
            description: metadata.description.unwrap_or_default(),
            path: std::fs::canonicalize(&path).unwrap_or(path),
            fingerprint: stable_fingerprint(&bytes),
        });
    }
    skills.sort_by(|left, right| left.name.cmp(&right.name).then(left.path.cmp(&right.path)));
    Ok(skills)
}

fn parse_frontmatter(text: &str) -> Result<SkillFrontmatter, serde_yaml::Error> {
    let Some(rest) = text.strip_prefix("---") else {
        return Ok(SkillFrontmatter::default());
    };
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'));
    let Some(rest) = rest else {
        return Ok(SkillFrontmatter::default());
    };
    let end = rest.find("\n---\n").or_else(|| rest.find("\r\n---\r\n"));
    let Some(end) = end else {
        return Ok(SkillFrontmatter::default());
    };
    serde_yaml::from_str(&rest[..end])
}

pub(crate) fn render_catalog(skills: &[SkillSnapshot]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let entries = skills
        .iter()
        .map(|skill| {
            format!(
                "<skill name=\"{}\" path=\"{}\">{}</skill>",
                xml_escape(&skill.name),
                xml_escape(&skill.path.display().to_string()),
                xml_escape(&skill.description)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "<skills>\nRead the matching SKILL.md before using a listed skill.\n{entries}\n</skills>"
    )
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SkillScanError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("skill file is not UTF-8: {path}")]
    Utf8 {
        path: PathBuf,
        #[source]
        source: std::str::Utf8Error,
    },
    #[error("invalid skill frontmatter: {path}")]
    Frontmatter {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
}
