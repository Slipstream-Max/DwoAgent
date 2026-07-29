use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use dwo_context::{ContentBlock, MessageContent};
use serde_json::{Map, Value, json};

use crate::call::ReadFileArgs;

const PAGE_LINES: usize = 500;

pub(crate) struct ReadFileOutput {
    pub output: Value,
    pub model_context: Vec<MessageContent>,
}

pub(crate) async fn execute(
    args: ReadFileArgs,
    cwd: &Path,
    allow_image_input: bool,
) -> Result<ReadFileOutput> {
    let path = resolve_path(cwd, &args.path);
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("read {}", path.display()))?;

    if let Some(mime_type) = image_mime_type(&bytes) {
        if !allow_image_input {
            bail!("the selected model does not support image input");
        }
        if args.cursor != 1 {
            bail!("cursor is only valid when reading text files");
        }
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        return Ok(ReadFileOutput {
            output: json!({"status": "completed"}),
            model_context: vec![MessageContent::blocks(vec![ContentBlock::image(
                mime_type, data,
            )])],
        });
    }

    let text = String::from_utf8(bytes)
        .with_context(|| format!("{} is not UTF-8 text or a supported image", path.display()))?;
    Ok(ReadFileOutput {
        output: text_page(&text, args.cursor)?,
        model_context: Vec::new(),
    })
}

fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn text_page(text: &str, cursor: usize) -> Result<Value> {
    let lines = text.lines().collect::<Vec<_>>();
    let total_lines = lines.len();
    if cursor > total_lines && !(cursor == 1 && total_lines == 0) {
        bail!("cursor {cursor} is past the end of the file ({total_lines} lines)");
    }

    let start = cursor.saturating_sub(1);
    let end = start.saturating_add(PAGE_LINES).min(total_lines);
    let mut output = Map::new();
    output.insert("content".to_string(), json!(lines[start..end].join("\n")));
    output.insert("start_line".to_string(), json!(cursor));
    output.insert("end_line".to_string(), json!(end));
    if total_lines > PAGE_LINES {
        output.insert("total_lines".to_string(), json!(total_lines));
    }
    Ok(Value::Object(output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_text_and_only_reports_total_for_long_files() {
        let short = text_page("one\ntwo\n", 1).unwrap();
        assert_eq!(
            short,
            json!({"content":"one\ntwo", "start_line":1, "end_line":2})
        );

        let long = (1..=1203)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let first = text_page(&long, 1).unwrap();
        assert_eq!(first["start_line"], 1);
        assert_eq!(first["end_line"], 500);
        assert_eq!(first["total_lines"], 1203);
        assert!(first.get("line_count").is_none());
        assert!(first.get("next_cursor").is_none());

        let next = text_page(&long, 501).unwrap();
        assert_eq!(next["start_line"], 501);
        assert_eq!(next["end_line"], 1000);
    }

    #[test]
    fn detects_supported_images_by_content() {
        assert_eq!(image_mime_type(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(image_mime_type(b"GIF89arest"), Some("image/gif"));
        assert_eq!(image_mime_type(b"RIFFxxxxWEBPrest"), Some("image/webp"));
    }
}
