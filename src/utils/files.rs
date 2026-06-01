//! Shared file and serialization helpers.

use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde_json::{Map, Value};

pub fn read_utf8_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let text = String::from_utf8(bytes).map_err(|_| {
        anyhow::anyhow!(
            "File is not UTF-8: {}. On Windows, save this file as UTF-8.",
            path.display()
        )
    })?;
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

/// Read a JSON object from UTF-8 text.
pub fn read_json_utf8(path: &Path) -> Result<Map<String, Value>> {
    let text = read_utf8_text(path)?;
    let loaded: Value =
        serde_json::from_str(&text).with_context(|| format!("parse JSON in {}", path.display()))?;
    match loaded {
        Value::Object(map) => Ok(map),
        _ => bail!("Invalid JSON root in {}: must be object", path.display()),
    }
}

/// Write a pretty-printed JSON object using 2-space indent.
pub fn write_json_utf8(path: &Path, payload: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(payload)
        .with_context(|| format!("serialize JSON for {}", path.display()))?;
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Read a top-level YAML mapping. Returns an empty map if the file is empty.
pub fn read_yaml_dict(path: &Path) -> Result<Map<String, Value>> {
    if !path.is_file() {
        bail!("Config file not found: {}", path.display());
    }
    let text = read_utf8_text(path)?;
    let loaded: Value = if text.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_yaml::from_str(&text).with_context(|| format!("parse YAML in {}", path.display()))?
    };
    match loaded {
        Value::Null => Ok(Map::new()),
        Value::Object(map) => Ok(map),
        _ => bail!("Invalid YAML root in {}: must be object", path.display()),
    }
}

/// UTC timestamp formatted as ISO-8601 with microsecond precision, matching
/// Python's `datetime.now(timezone.utc).isoformat()` output closely.
pub fn utc_iso() -> String {
    // Python emits `+00:00` for UTC; chrono's `%+` produces `+00:00` too.
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.6f%:z").to_string()
}
