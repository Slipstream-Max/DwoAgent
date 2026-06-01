//! Shared string normalization helpers.

use anyhow::{Result, bail};

pub fn normalize_required_str(value: &str, field_name: &str) -> Result<String> {
    let parsed = value.trim();
    if parsed.is_empty() {
        bail!("{field_name} cannot be empty");
    }
    Ok(parsed.to_string())
}

pub fn normalize_optional_str(value: Option<&str>, field_name: &str) -> Result<Option<String>> {
    match value {
        None => Ok(None),
        Some(s) => normalize_required_str(s, field_name).map(Some),
    }
}
