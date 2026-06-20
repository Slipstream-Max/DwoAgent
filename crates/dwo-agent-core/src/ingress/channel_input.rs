//! Shared helpers for turning channel payloads into ACP content blocks.

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::{ContentBlock, ImageContent, ResourceLink, TextContent};
use anyhow::{Context, Result, bail};
use base64::Engine;

pub fn resolve_config_path(agent_structure_dir: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    let resolved = if path.is_absolute() {
        path
    } else {
        agent_structure_dir.join(path)
    };
    std::fs::canonicalize(&resolved).unwrap_or(resolved)
}

pub fn sanitize_filename(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('.')
        .trim()
        .to_string()
}

pub fn sanitize_filename_or(raw: &str, fallback: &str) -> String {
    let sanitized = sanitize_filename(raw);
    if sanitized.is_empty() {
        fallback.to_string()
    } else {
        sanitized
    }
}

pub fn file_uri_from_path(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let encoded = percent_encode_file_path(&normalized);
    if encoded.starts_with("//") {
        format!("file:{encoded}")
    } else if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

pub fn image_mime_type_for_path(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("png") => Some("image/png"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        Some("bmp") => Some("image/bmp"),
        Some("ico") => Some("image/x-icon"),
        Some("tiff") | Some("tif") => Some("image/tiff"),
        Some("heic") => Some("image/heic"),
        _ => None,
    }
}

pub fn image_url_block_from_path(path: &Path) -> Result<Option<ContentBlock>> {
    let Some(mime_type) = image_mime_type_for_path(path) else {
        return Ok(None);
    };
    image_url_block_from_file(path, mime_type)
}

pub fn image_url_block_from_file(path: &Path, mime_type: &str) -> Result<Option<ContentBlock>> {
    if !mime_type.starts_with("image/") {
        return Ok(None);
    }
    let data = std::fs::read(path).with_context(|| format!("read image {}", path.display()))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    Ok(Some(ContentBlock::Image(ImageContent::new(
        encoded, mime_type,
    ))))
}

pub fn resource_link_block(
    uri: &str,
    name: Option<&str>,
    mime_type: Option<&str>,
) -> Result<ContentBlock> {
    let uri = uri.trim();
    if uri.is_empty() {
        bail!("resource_link block must provide uri");
    }
    let name = name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("attachment");
    let mut link = ResourceLink::new(name, uri);
    if let Some(mime_type) = mime_type.map(str::trim).filter(|value| !value.is_empty()) {
        link = link.mime_type(mime_type.to_string());
    }
    Ok(ContentBlock::ResourceLink(link))
}

pub fn append_channel_context(
    blocks: &mut Vec<ContentBlock>,
    channel: &str,
    tool_instructions: &[&str],
) {
    let mut lines = vec![
        "<channel_context>".to_string(),
        format!("当前消息来自 {channel} 频道。"),
    ];
    lines.extend(tool_instructions.iter().map(|line| line.to_string()));
    lines.push("</channel_context>".to_string());
    blocks.push(ContentBlock::Text(TextContent::new(lines.join("\n"))));
}

pub fn extension_for_mime_type(mime_type: &str) -> Option<&'static str> {
    match mime_type {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/bmp" => Some("bmp"),
        "text/plain" => Some("txt"),
        "text/markdown" => Some("md"),
        "application/json" => Some("json"),
        "application/pdf" => Some("pdf"),
        "application/zip" => Some("zip"),
        "audio/mpeg" => Some("mp3"),
        "audio/wav" => Some("wav"),
        "audio/ogg" => Some("ogg"),
        "video/mp4" => Some("mp4"),
        "video/quicktime" => Some("mov"),
        "video/webm" => Some("webm"),
        _ => None,
    }
}

fn percent_encode_file_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_from_path_encodes_spaces() {
        let path = PathBuf::from("C:/tmp/hello world.txt");

        assert_eq!(
            file_uri_from_path(&path),
            "file:///C:/tmp/hello%20world.txt"
        );
    }

    #[test]
    fn file_uri_from_path_formats_windows_drive_paths() {
        let path = PathBuf::from(r"C:\tmp\image.png");

        assert_eq!(file_uri_from_path(&path), "file:///C:/tmp/image.png");
    }

    #[test]
    fn sanitize_filename_can_fallback() {
        assert_eq!(sanitize_filename_or("...", "unknown"), "unknown");
        assert_eq!(
            sanitize_filename_or("hello world.txt", "unknown"),
            "hello_world.txt"
        );
    }

    #[test]
    fn image_url_block_from_path_uses_data_url() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        std::fs::write(&path, b"abc").unwrap();

        let block = image_url_block_from_path(&path).unwrap().unwrap();

        match block {
            ContentBlock::Image(image) => {
                assert_eq!(image.mime_type, "image/png");
                assert_eq!(image.data, "YWJj");
            }
            other => panic!("expected image content block, got {other:?}"),
        }
    }
}
