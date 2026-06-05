//! Shared user message content block helpers.

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

pub const IMAGE_PLACEHOLDER_TEXT: &str = "该处为图片消息。";

pub fn text(text: &str) -> Result<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bail!("text block cannot be empty");
    }
    Ok(json!({"type": "text", "text": trimmed}))
}

pub fn resource_link(uri: &str, name: Option<&str>, mime_type: Option<&str>) -> Result<Value> {
    let uri = uri.trim();
    if uri.is_empty() {
        bail!("resource_link block must provide uri");
    }

    let mut out = Map::new();
    out.insert(
        "type".to_string(),
        Value::String("resource_link".to_string()),
    );
    out.insert("uri".to_string(), Value::String(uri.to_string()));

    if let Some(name) = name.map(str::trim).filter(|value| !value.is_empty()) {
        out.insert("name".to_string(), Value::String(name.to_string()));
    }
    if let Some(mime_type) = mime_type.map(str::trim).filter(|value| !value.is_empty()) {
        out.insert("mimeType".to_string(), Value::String(mime_type.to_string()));
    }

    Ok(Value::Object(out))
}

pub fn image_url(url: &str) -> Result<Value> {
    let url = url.trim();
    if url.is_empty() {
        bail!("image_url block must provide url");
    }
    Ok(json!({"type": "image_url", "image_url": {"url": url}}))
}

pub fn image_url_data(mime_type: &str, data: &str) -> Result<Value> {
    let mime_type = mime_type.trim();
    let data = data.trim();
    if mime_type.is_empty() {
        bail!("image block must provide mimeType");
    }
    if data.is_empty() {
        bail!("image block must provide data");
    }
    image_url(&format!("data:{mime_type};base64,{data}"))
}

pub fn image_placeholder() -> Value {
    json!({"type": "text", "text": IMAGE_PLACEHOLDER_TEXT})
}

pub fn is_image_part(part: &Value) -> bool {
    part.as_object()
        .and_then(|obj| obj.get("type"))
        .and_then(Value::as_str)
        .map(str::trim)
        .is_some_and(|ty| matches!(ty, "image" | "image_url" | "input_image"))
}

pub fn normalize_user_content(content: &Value) -> Result<Value> {
    match content {
        Value::String(s) => {
            let text = text(s)?;
            Ok(Value::Array(vec![text]))
        }
        Value::Array(parts) => {
            if parts.is_empty() {
                bail!("user_input content blocks cannot be empty");
            }
            let mut out: Vec<Value> = Vec::with_capacity(parts.len());
            for (index, part) in parts.iter().enumerate() {
                out.push(normalize_user_part(part, index)?);
            }
            Ok(Value::Array(out))
        }
        _ => bail!("user_input must be string or list of content blocks"),
    }
}

fn normalize_user_part(part: &Value, index: usize) -> Result<Value> {
    let obj = part.as_object().ok_or_else(|| {
        anyhow::anyhow!("user_input content block at index {index} must be object")
    })?;

    let part_type = obj
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");

    match part_type {
        "text" | "input_text" => normalize_text_part(obj, index),
        "image_url" | "image" | "input_image" => normalize_image_part(obj, index),
        "resource" => normalize_resource_part(obj, index),
        "resource_link" => normalize_resource_link_part(obj, index),
        other => bail!("Unsupported content block type at index {index}: {other}"),
    }
}

fn normalize_text_part(obj: &Map<String, Value>, index: usize) -> Result<Value> {
    let text_value = obj
        .get("text")
        .or_else(|| obj.get("input_text"))
        .cloned()
        .unwrap_or(Value::Null);
    let text_str = match text_value {
        Value::Null => String::new(),
        Value::String(s) => s,
        other => other.to_string(),
    };
    text(&text_str).map_err(|_| anyhow::anyhow!("text block at index {index} cannot be empty"))
}

fn normalize_image_part(obj: &Map<String, Value>, index: usize) -> Result<Value> {
    let part_type = obj
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");

    if part_type == "image" {
        let data = obj
            .get("data")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !data.is_empty() {
            let mime_type = obj
                .get("mimeType")
                .or_else(|| obj.get("mime_type"))
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if mime_type.is_empty() {
                bail!("image block at index {index} must provide mimeType");
            }
            return image_url_data(mime_type, data);
        }
    }

    let url = extract_image_url(obj, index)?;
    image_url(&url)
}

fn normalize_resource_part(obj: &Map<String, Value>, index: usize) -> Result<Value> {
    if !obj.get("resource").is_some_and(Value::is_object) {
        bail!("resource block at index {index} must provide resource");
    }
    Ok(Value::Object(obj.clone()))
}

fn normalize_resource_link_part(obj: &Map<String, Value>, index: usize) -> Result<Value> {
    let uri = obj
        .get("uri")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if uri.is_empty() {
        bail!("resource_link block at index {index} must provide uri");
    }
    Ok(Value::Object(obj.clone()))
}

fn extract_image_url(obj: &Map<String, Value>, index: usize) -> Result<String> {
    let url_value = match obj.get("image_url") {
        Some(Value::Object(m)) => m.get("url").cloned().unwrap_or(Value::Null),
        Some(Value::String(s)) => Value::String(s.clone()),
        _ => obj
            .get("url")
            .or_else(|| obj.get("uri"))
            .cloned()
            .unwrap_or(Value::Null),
    };
    let url = match url_value {
        Value::Null => String::new(),
        Value::String(s) => s,
        other => other.to_string(),
    };
    let trimmed = url.trim();
    if trimmed.is_empty() {
        bail!("image block at index {index} must provide url");
    }
    Ok(trimmed.to_string())
}
