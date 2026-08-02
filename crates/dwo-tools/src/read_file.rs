use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use dwo_context::{ContentBlock, MessageContent};
use serde_json::{Map, Value, json};

use crate::call::{DEFAULT_READ_FILE_LINES, ReadFileArgs};

const MAX_TEXT_OUTPUT_BYTES: usize = 20_000;

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
        if args.offset != 0 {
            bail!("offset is only valid when reading text files");
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
    text_page_with_budget(text, cursor, line_count, offset, MAX_TEXT_OUTPUT_BYTES)
}

fn text_page_with_budget(
    text: &str,
    cursor: usize,
    line_count: usize,
    offset: usize,
    byte_budget: usize,
) -> Result<Value> {
    if byte_budget == 0 {
        bail!("text output byte budget must be positive");
    }
    let lines = text.lines().collect::<Vec<_>>();
    let total_lines = lines.len();
    if cursor > total_lines && !(cursor == 1 && total_lines == 0) {
        bail!("cursor {cursor} is past the end of the file ({total_lines} lines)");
    }
    if total_lines == 0 {
        if offset != 0 {
            bail!("offset {offset} is past the end of an empty file");
        }
        return Ok(json!({
            "content": "",
            "start_line": 1,
            "start_offset": 0,
            "end_line": 0,
            "end_offset": 0,
            "total_lines": 0,
        }));
    }

    let start_line = cursor;
    let start_offset = offset;
    let mut line_index = cursor - 1;
    let mut current_offset = offset;
    let mut lines_read = 1usize;
    let mut content = String::new();

    let (end_line, end_offset, next) = loop {
        let line = lines[line_index];
        let line_chars = line.chars().count();
        if current_offset > line_chars {
            bail!(
                "offset {current_offset} is past the end of line {} ({line_chars} chars)",
                line_index + 1
            );
        }

        if current_offset == line_chars {
            if line_index + 1 == total_lines {
                break (line_index + 1, line_chars, None);
            }
            if content.len() == byte_budget {
                break (
                    line_index + 1,
                    line_chars,
                    Some((line_index + 1, line_chars)),
                );
            }
            content.push('\n');
            let completed_line = line_index + 1;
            line_index += 1;
            current_offset = 0;
            if content.len() == byte_budget {
                break (completed_line, line_chars, Some((line_index + 1, 0)));
            }
            if lines_read == line_count {
                break (completed_line, line_chars, Some((line_index + 1, 0)));
            }
            lines_read += 1;
            continue;
        }

        let byte_offset = char_offset_to_byte(line, current_offset);
        let suffix = &line[byte_offset..];
        let available = byte_budget - content.len();
        let chunk = utf8_prefix(suffix, available);
        content.push_str(chunk);
        let consumed_chars = chunk.chars().count();
        current_offset += consumed_chars;

        if current_offset < line_chars {
            break (
                line_index + 1,
                current_offset,
                Some((line_index + 1, current_offset)),
            );
        }

        if line_index + 1 == total_lines {
            break (line_index + 1, current_offset, None);
        }
        if lines_read == line_count || content.len() == byte_budget {
            break (
                line_index + 1,
                current_offset,
                Some((line_index + 1, current_offset)),
            );
        }
        content.push('\n');
        let completed_line = line_index + 1;
        line_index += 1;
        current_offset = 0;
        if content.len() == byte_budget {
            break (completed_line, line_chars, Some((line_index + 1, 0)));
        }
        lines_read += 1;
    };

    let mut output = Map::new();
    output.insert("content".to_string(), json!(content));
    output.insert("start_line".to_string(), json!(start_line));
    output.insert("start_offset".to_string(), json!(start_offset));
    output.insert("end_line".to_string(), json!(end_line));
    output.insert("end_offset".to_string(), json!(end_offset));
    output.insert("total_lines".to_string(), json!(total_lines));
    if let Some((next_cursor, next_offset)) = next {
        output.insert("next_cursor".to_string(), json!(next_cursor));
        output.insert("next_offset".to_string(), json!(next_offset));
    }
    Ok(Value::Object(output))
}

fn char_offset_to_byte(text: &str, offset: usize) -> usize {
    text.char_indices()
        .nth(offset)
        .map_or(text.len(), |(index, _)| index)
}

fn utf8_prefix(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_text_by_line_count_with_explicit_next_position() {
        let short = text_page("one\ntwo\n", 1, 500, 0).unwrap();
        assert_eq!(
            short,
            json!({
                "content":"one\ntwo",
                "start_line":1,
                "start_offset":0,
                "end_line":2,
                "end_offset":3,
                "total_lines":2,
            })
        );

        let long = (1..=6)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let first = text_page(&long, 1, 3, 0).unwrap();
        assert_eq!(first["content"], "line 1\nline 2\nline 3");
        assert_eq!(first["start_line"], 1);
        assert_eq!(first["end_line"], 3);
        assert_eq!(first["next_cursor"], 3);
        assert_eq!(first["next_offset"], 6);

        let next = text_page(&long, 3, 3, 6).unwrap();
        assert_eq!(next["content"], "\nline 4\nline 5");
        assert_eq!(next["next_cursor"], 5);
        assert_eq!(next["next_offset"], 6);
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
        assert_eq!(page["end_offset"], 7);
        assert_eq!(page["total_lines"], 100);
    }

    #[test]
    fn byte_budget_is_global_and_pages_a_unicode_line_without_loss() {
        let text = format!("prefix:{}:suffix", "界".repeat(30));
        let mut cursor = 1usize;
        let mut offset = 0usize;
        let mut rebuilt = String::new();
        loop {
            let page = text_page_with_budget(&text, cursor, 500, offset, 19).unwrap();
            let content = page["content"].as_str().unwrap();
            assert!(content.len() <= 19);
            rebuilt.push_str(content);
            let Some(next_cursor) = page.get("next_cursor").and_then(Value::as_u64) else {
                break;
            };
            cursor = next_cursor as usize;
            offset = page["next_offset"].as_u64().unwrap() as usize;
        }
        assert_eq!(rebuilt, text);
    }

    #[test]
    fn starts_at_a_character_offset_and_can_continue_across_lines() {
        let page = text_page_with_budget("alpha\n世界\nomega", 1, 3, 2, 11).unwrap();
        assert_eq!(page["content"], "pha\n世界\n");
        assert_eq!(page["start_line"], 1);
        assert_eq!(page["start_offset"], 2);
        assert_eq!(page["next_cursor"], 3);
        assert_eq!(page["next_offset"], 0);
    }

    #[test]
    fn rejects_offsets_past_the_selected_line() {
        assert!(text_page("short", 1, 1, 6).is_err());
        assert!(text_page("", 1, 1, 1).is_err());
    }

    #[test]
    fn line_count_includes_empty_and_boundary_start_lines() {
        let empty_start = text_page("\none\ntwo", 1, 1, 0).unwrap();
        assert_eq!(empty_start["content"], "\n");
        assert_eq!(empty_start["next_cursor"], 2);
        assert_eq!(empty_start["next_offset"], 0);

        let boundary_start = text_page("one\ntwo\nthree", 1, 2, 3).unwrap();
        assert_eq!(boundary_start["content"], "\ntwo");
        assert_eq!(boundary_start["next_cursor"], 2);
        assert_eq!(boundary_start["next_offset"], 3);
    }

    #[test]
    fn detects_supported_images_by_content() {
        assert_eq!(image_mime_type(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(image_mime_type(b"GIF89arest"), Some("image/gif"));
        assert_eq!(image_mime_type(b"RIFFxxxxWEBPrest"), Some("image/webp"));
    }

    #[tokio::test]
    async fn images_reject_text_paging_arguments() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("image.bin"),
            b"\x89PNG\r\n\x1a\nimage",
        )
        .unwrap();
        let error = execute(
            ReadFileArgs {
                path: PathBuf::from("image.bin"),
                cursor: 1,
                line_count: DEFAULT_READ_FILE_LINES,
                offset: 1,
            },
            directory.path(),
            true,
        )
        .await
        .err()
        .expect("image offset should fail");
        assert!(error.to_string().contains("offset is only valid"));
    }
}
