use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use dwo_context::{ContentBlock, MessageContent};
use serde_json::{Map, Value, json};

use crate::call::{DEFAULT_READ_FILE_LINES, ReadFileArgs};
use crate::terminal::{DEFAULT_MODEL_CAP_BYTES, render_capped};

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
        if args.line_count != DEFAULT_READ_FILE_LINES {
            bail!("line_count is only valid when reading text files");
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
        output: text_page(&text, args.cursor, args.line_count, args.offset)?,
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

fn text_page(text: &str, cursor: usize, line_count: usize, offset: usize) -> Result<Value> {
    let lines = text.lines().collect::<Vec<_>>();
    let total_lines = lines.len();
    if cursor > total_lines && !(cursor == 1 && total_lines == 0) {
        bail!("cursor {cursor} is past the end of the file ({total_lines} lines)");
    }
    let mut output = Map::new();
    output.insert("start_line".to_string(), json!(cursor));
    if total_lines > line_count {
        output.insert("total_lines".to_string(), json!(total_lines));
    }

    if offset > 0 {
        if line_count != 1 {
            bail!("offset is only valid with line_count 1");
        }
        if lines.is_empty() {
            bail!("cannot page an empty file with offset");
        }
        let line = lines[cursor - 1];
        let line_chars = line.chars().count();
        if offset >= line_chars {
            bail!("offset {offset} is past the end of the line ({line_chars} chars)");
        }
        let slice: String = line.chars().skip(offset).collect();
        output.insert(
            "content".to_string(),
            json!(render_capped(slice.as_bytes(), DEFAULT_MODEL_CAP_BYTES)),
        );
        output.insert("end_line".to_string(), json!(cursor));
        output.insert("offset".to_string(), json!(offset));
        output.insert("line_chars".to_string(), json!(line_chars));
        output.insert("remaining_chars".to_string(), json!(line_chars - offset));
        return Ok(Value::Object(output));
    }

    let start = cursor.saturating_sub(1);
    let end = start.saturating_add(line_count).min(total_lines);
    let mut truncated_lines = Vec::new();
    let content = lines[start..end]
        .iter()
        .enumerate()
        .map(|(index, line)| {
            if line.len() > DEFAULT_MODEL_CAP_BYTES {
                truncated_lines.push(json!({
                    "line": start + index + 1,
                    "chars": line.chars().count(),
                }));
            }
            render_capped(line.as_bytes(), DEFAULT_MODEL_CAP_BYTES)
        })
        .collect::<Vec<_>>()
        .join("\n");
    output.insert("content".to_string(), json!(content));
    output.insert("end_line".to_string(), json!(end));
    if !truncated_lines.is_empty() {
        output.insert("truncated_lines".to_string(), json!(truncated_lines));
    }
    Ok(Value::Object(output))
}

#[test]
fn offset_pages_through_a_long_line() {
    let text = "0123456789".repeat(10);
    let page = text_page(&text, 1, 1, 50).unwrap();
    assert!(page["content"].as_str().unwrap().starts_with("01234"));
    assert_eq!(page["offset"], 50);
    assert_eq!(page["line_chars"], 100);
    assert_eq!(page["remaining_chars"], 50);
}

#[test]
fn offset_requires_single_line_and_valid_range() {
    let text = "short\n";
    assert!(text_page(&text, 1, 2, 10).is_err());
    assert!(text_page(&text, 1, 1, 100).is_err());
    assert!(text_page("", 1, 1, 10).is_err());
}

#[test]
fn reports_truncated_lines_metadata() {
    let text = format!("{}short", "x".repeat(50_000));
    let page = text_page(&text, 1, 1, 0).unwrap();
    assert_eq!(
        page["truncated_lines"],
        json!([{"line": 1, "chars": 50_005}])
    );

    let short = text_page("fine\n", 1, 1, 0).unwrap();
    assert!(short.get("truncated_lines").is_none());
}

#[test]
fn truncates_long_lines_like_terminal_output() {
    let text = format!("HEAD{}TAIL", "x".repeat(50_000));
        let page = text_page(&text, 1, 1, 0).unwrap();
    let content = page["content"].as_str().unwrap();
    assert!(content.starts_with("HEAD"));
    assert!(content.ends_with("TAIL"));
    assert!(content.contains("output omitted"));
    assert!(content.len() <= DEFAULT_MODEL_CAP_BYTES);
}

#[test]
fn keeps_short_lines_untouched() {
        let page = text_page("short line\n", 1, 1, 0).unwrap();
    assert_eq!(page["content"], "short line");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_text_and_only_reports_total_for_long_files() {
        let short = text_page("one\ntwo\n", 1, 500, 0).unwrap();
        assert_eq!(
            short,
            json!({"content":"one\ntwo", "start_line":1, "end_line":2})
        );

        let long = (1..=1203)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let first = text_page(&long, 1, 500, 0).unwrap();
        assert_eq!(first["start_line"], 1);
        assert_eq!(first["end_line"], 500);
        assert_eq!(first["total_lines"], 1203);
        assert!(first.get("line_count").is_none());
        assert!(first.get("next_cursor").is_none());

        let next = text_page(&long, 501, 500, 0).unwrap();
        assert_eq!(next["start_line"], 501);
        assert_eq!(next["end_line"], 1000);
    }

    #[test]
    fn reads_the_requested_number_of_lines() {
        let text = (1..=100)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let page = text_page(&text, 20, 3, 0).unwrap();
        assert_eq!(page["content"], "line 20\nline 21\nline 22");
        assert_eq!(page["start_line"], 20);
        assert_eq!(page["end_line"], 22);
        assert_eq!(page["total_lines"], 100);
    }

    #[test]
    fn detects_supported_images_by_content() {
        assert_eq!(image_mime_type(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(image_mime_type(b"GIF89arest"), Some("image/gif"));
        assert_eq!(image_mime_type(b"RIFFxxxxWEBPrest"), Some("image/webp"));
    }
}
