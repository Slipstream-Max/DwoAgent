//! Small file edit runtime for text edits.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use thiserror::Error;

pub const BEGIN_PATCH: &str = "*** Begin Patch";
pub const END_PATCH: &str = "*** End Patch";
pub const WRITE_FILE: &str = "*** Write File: ";
pub const DELETE_FILE: &str = "*** Delete File: ";
pub const UPDATE_FILE: &str = "*** Update File: ";
pub const REPLACE_ALL: &str = "*** Replace All: ";
pub const MOVE_TO: &str = "*** Move to: ";
pub const CHANGE_CONTEXT: &str = "@@";
pub const END_OF_FILE: &str = "*** End of File";
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

#[derive(Debug, Error)]
#[error("{0}")]
pub struct FileEditError(pub String);

impl FileEditError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

#[derive(Debug, Clone)]
pub struct WriteFile {
    pub path: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DeleteFile {
    pub path: String,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateSegment {
    pub started: bool,
    pub anchors: Vec<String>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub eof: bool,
}

#[derive(Debug, Clone)]
pub struct UpdateFile {
    pub path: String,
    pub move_to: Option<String>,
    pub segments: Vec<UpdateSegment>,
}

#[derive(Debug, Clone)]
pub struct ReplaceAll {
    pub path: String,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum Operation {
    Write(WriteFile),
    Delete(DeleteFile),
    Update(UpdateFile),
    ReplaceAll(ReplaceAll),
}

struct TextFile {
    text: String,
    bom: bool,
}

pub fn execute_file_tool(name: &str, args: &Map<String, Value>, cwd: &Path) -> Result<Value> {
    match name {
        "file_edit" => {
            let patch = args
                .get("patch")
                .or_else(|| args.get("patchText"))
                .and_then(Value::as_str)
                .unwrap_or("");
            file_edit_text(patch, cwd)
        }
        other => Err(FileEditError::new(format!("Unknown file tool: {other}")).into()),
    }
}

/// Entry point — parses a patch and resolves relative paths against *cwd*.
pub fn file_edit_text(patch: &str, cwd: &Path) -> Result<Value> {
    let operations = parse_patch(patch)?;
    let root = std::fs::canonicalize(cwd)
        .with_context(|| format!("resolve workspace root {}", cwd.display()))?;

    let mut added: Vec<String> = Vec::new();
    let mut modified: Vec<String> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut replacements: Vec<Value> = Vec::new();

    for operation in operations {
        match operation {
            Operation::Write(write) => {
                let path = resolve_workspace_path(&root, &write.path)?;
                let existed = path.is_file();
                let bom = if existed {
                    read_text_file(&path)?.bom
                } else {
                    true
                };
                let content = patch_lines_to_file_content(&write.lines);
                write_text(&path, &content, bom)?;
                if existed {
                    modified.push(relative_display(&root, &path));
                } else {
                    added.push(relative_display(&root, &path));
                }
            }
            Operation::Delete(del) => {
                let path = resolve_workspace_path(&root, &del.path)?;
                if !path.is_file() {
                    return Err(FileEditError::new(format!(
                        "File to delete does not exist: {}",
                        del.path
                    ))
                    .into());
                }
                std::fs::remove_file(&path)
                    .with_context(|| format!("delete {}", path.display()))?;
                deleted.push(relative_display(&root, &path));
            }
            Operation::Update(update) => {
                let source_path = resolve_workspace_path(&root, &update.path)?;
                if !source_path.is_file() {
                    return Err(FileEditError::new(format!(
                        "File to update does not exist: {}",
                        update.path
                    ))
                    .into());
                }
                let original = read_text_file(&source_path)?;
                let line_ending = detect_line_ending(&original.text);
                let updated = convert_line_endings(
                    &normalize_line_endings(&apply_update_segments(
                        &original.text,
                        &update.segments,
                    )?),
                    line_ending,
                );

                if let Some(dest_raw) = update.move_to {
                    let dest_path = resolve_workspace_path(&root, &dest_raw)?;
                    write_text(&dest_path, &updated, original.bom)?;
                    std::fs::remove_file(&source_path)
                        .with_context(|| format!("remove {}", source_path.display()))?;
                    modified.push(relative_display(&root, &dest_path));
                    deleted.push(relative_display(&root, &source_path));
                } else {
                    write_text(&source_path, &updated, original.bom)?;
                    modified.push(relative_display(&root, &source_path));
                }
            }
            Operation::ReplaceAll(replace) => {
                let path = resolve_workspace_path(&root, &replace.path)?;
                if !path.is_file() {
                    return Err(FileEditError::new(format!(
                        "File to replace all does not exist: {}",
                        replace.path
                    ))
                    .into());
                }
                let original = read_text_file(&path)?;
                let line_ending = detect_line_ending(&original.text);
                let old = convert_line_endings(
                    &normalize_line_endings(&replace.old_lines.join("\n")),
                    line_ending,
                );
                let new = convert_line_endings(
                    &normalize_line_endings(&replace.new_lines.join("\n")),
                    line_ending,
                );
                let (updated, count) = replace_all_text(&original.text, &old, &new)?;
                write_text(&path, &updated, original.bom)?;
                let relative = relative_display(&root, &path);
                modified.push(relative.clone());
                replacements.push(json!({
                    "path": relative,
                    "count": count,
                }));
            }
        }
    }

    Ok(json!({
        "tool": "file_edit",
        "kind": "file_edit",
        "status": "completed",
        "added": added,
        "modified": modified,
        "deleted": deleted,
        "replacements": replacements,
    }))
}

/// Parse patch text into an ordered list of operations.
pub fn parse_patch(patch: &str) -> Result<Vec<Operation>> {
    let lines: Vec<&str> = patch.trim().lines().collect();
    if lines.len() < 2 || lines[0].trim() != BEGIN_PATCH {
        return Err(
            FileEditError::new("The first line of the patch must be '*** Begin Patch'.").into(),
        );
    }
    if lines.last().map(|l| l.trim()) != Some(END_PATCH) {
        return Err(
            FileEditError::new("The last line of the patch must be '*** End Patch'.").into(),
        );
    }

    let mut operations: Vec<Operation> = Vec::new();
    let mut index = 1usize;
    let end_index = lines.len() - 1;

    while index < end_index {
        let line = lines[index];
        if let Some(rest) = line.strip_prefix(WRITE_FILE) {
            let (op, next) = parse_write_file(&lines, index, end_index, rest)?;
            operations.push(Operation::Write(op));
            index = next;
        } else if let Some(rest) = line.strip_prefix(DELETE_FILE) {
            operations.push(Operation::Delete(DeleteFile {
                path: non_empty_path(rest)?,
            }));
            index += 1;
        } else if let Some(rest) = line.strip_prefix(UPDATE_FILE) {
            let (op, next) = parse_update_file(&lines, index, end_index, rest)?;
            operations.push(Operation::Update(op));
            index = next;
        } else if let Some(rest) = line.strip_prefix(REPLACE_ALL) {
            let (op, next) = parse_replace_all(&lines, index, end_index, rest)?;
            operations.push(Operation::ReplaceAll(op));
            index = next;
        } else if line.trim().is_empty() {
            index += 1;
        } else {
            return Err(FileEditError::new(format!("Invalid operation header: {line}")).into());
        }
    }

    if operations.is_empty() {
        return Err(FileEditError::new("Patch must contain at least one operation.").into());
    }
    Ok(operations)
}

fn parse_write_file(
    lines: &[&str],
    start: usize,
    end_index: usize,
    first_rest: &str,
) -> Result<(WriteFile, usize)> {
    let path = non_empty_path(first_rest)?;
    let mut index = start + 1;
    let mut write_lines: Vec<String> = Vec::new();
    while index < end_index && !is_operation_header(lines[index]) {
        let line = lines[index];
        if !line.starts_with('+') {
            return Err(FileEditError::new("Write File body lines must start with '+'.").into());
        }
        write_lines.push(line[1..].to_string());
        index += 1;
    }
    Ok((
        WriteFile {
            path,
            lines: write_lines,
        },
        index,
    ))
}

fn parse_replace_all(
    lines: &[&str],
    start: usize,
    end_index: usize,
    first_rest: &str,
) -> Result<(ReplaceAll, usize)> {
    let path = non_empty_path(first_rest)?;
    let mut index = start + 1;
    let mut old_lines: Vec<String> = Vec::new();
    let mut new_lines: Vec<String> = Vec::new();
    let mut saw_new = false;

    while index < end_index && !is_operation_header(lines[index]) {
        let line = lines[index];
        if line.is_empty() {
            return Err(FileEditError::new("Replace All lines must start with '-' or '+'.").into());
        }
        let (prefix, text) = line.split_at(1);
        match prefix {
            "-" if !saw_new => old_lines.push(text.to_string()),
            "-" => {
                return Err(FileEditError::new(
                    "Replace All old text lines must come before replacement lines.",
                )
                .into());
            }
            "+" => {
                saw_new = true;
                new_lines.push(text.to_string());
            }
            _ => {
                return Err(
                    FileEditError::new("Replace All lines must start with '-' or '+'.").into(),
                );
            }
        }
        index += 1;
    }

    if old_lines.is_empty() {
        return Err(FileEditError::new(format!("Replace All old text is empty: {path}")).into());
    }

    Ok((
        ReplaceAll {
            path,
            old_lines,
            new_lines,
        },
        index,
    ))
}

fn parse_update_file(
    lines: &[&str],
    start: usize,
    end_index: usize,
    first_rest: &str,
) -> Result<(UpdateFile, usize)> {
    let path = non_empty_path(first_rest)?;
    let mut index = start + 1;
    let mut move_to: Option<String> = None;
    if index < end_index
        && let Some(rest) = lines[index].strip_prefix(MOVE_TO)
    {
        move_to = Some(non_empty_path(rest)?);
        index += 1;
    }

    let mut segments: Vec<UpdateSegment> = Vec::new();
    let mut current = UpdateSegment::default();
    let mut saw_change_line = false;

    while index < end_index && !is_operation_header(lines[index]) {
        let line = lines[index];
        if line == END_OF_FILE {
            current.eof = true;
            index += 1;
            if index < end_index && !is_operation_header(lines[index]) {
                return Err(
                    FileEditError::new("'*** End of File' must end the Update File body.").into(),
                );
            }
            break;
        }

        if line == CHANGE_CONTEXT || line.starts_with(&format!("{CHANGE_CONTEXT} ")) {
            if !current.old_lines.is_empty() || !current.new_lines.is_empty() {
                segments.push(std::mem::take(&mut current));
            }
            current.started = true;
            let mut header = &line[CHANGE_CONTEXT.len()..];
            if let Some(rest) = header.strip_prefix(' ') {
                header = rest;
            }
            if !header.is_empty() {
                current.anchors.push(header.to_string());
            }
            index += 1;
            continue;
        }

        if !current.started {
            return Err(FileEditError::new("Update File hunk must start with '@@'.").into());
        }
        if line.is_empty() {
            return Err(FileEditError::new(
                "Update File lines must start with ' ', '-', '+', or '@@'.",
            )
            .into());
        }
        let (prefix, text) = line.split_at(1);
        match prefix {
            " " => {
                current.old_lines.push(text.to_string());
                current.new_lines.push(text.to_string());
            }
            "-" => current.old_lines.push(text.to_string()),
            "+" => current.new_lines.push(text.to_string()),
            _ => {
                return Err(FileEditError::new(
                    "Update File lines must start with ' ', '-', '+', or '@@'.",
                )
                .into());
            }
        }
        saw_change_line = true;
        index += 1;
    }

    if current.started
        || !current.anchors.is_empty()
        || !current.old_lines.is_empty()
        || !current.new_lines.is_empty()
        || current.eof
    {
        segments.push(current);
    }
    if !saw_change_line {
        return Err(
            FileEditError::new(format!("Update File body has no change lines: {path}")).into(),
        );
    }
    Ok((
        UpdateFile {
            path,
            move_to,
            segments,
        },
        index,
    ))
}

fn apply_update_segments(content: &str, segments: &[UpdateSegment]) -> Result<String> {
    // `str::lines` drops trailing newlines but that's what we want here —
    // Python splits the same way via `splitlines()`.
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    let trailing_newline = content.ends_with('\n');
    let mut cursor: usize = 0;

    for segment in segments {
        for anchor in &segment.anchors {
            let anchor_index = find_single_line(&lines, anchor, cursor)?;
            cursor = anchor_index + 1;
        }

        let match_index = if segment.eof {
            find_at_eof(&lines, &segment.old_lines)?
        } else if !segment.old_lines.is_empty() {
            find_sequence(
                &lines,
                &segment.old_lines,
                cursor,
                segment.anchors.is_empty(),
            )?
        } else {
            cursor
        };

        let end_index = match_index + segment.old_lines.len();
        let replacement: Vec<String> = segment.new_lines.clone();
        lines.splice(match_index..end_index, replacement);
        cursor = match_index + segment.new_lines.len();
    }

    if lines.is_empty() {
        return Ok(if trailing_newline {
            "\n".to_string()
        } else {
            String::new()
        });
    }
    let mut joined = lines.join("\n");
    if trailing_newline {
        joined.push('\n');
    }
    Ok(joined)
}

fn patch_lines_to_file_content(lines: &[String]) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn replace_all_text(content: &str, old: &str, new: &str) -> Result<(String, usize)> {
    if old.is_empty() {
        return Err(FileEditError::new("Replace All old text must not be empty.").into());
    }
    if old == new {
        return Err(FileEditError::new(
            "Replace All old text and replacement text must be different.",
        )
        .into());
    }

    let replacements = content.matches(old).count();
    if replacements == 0 {
        return Err(FileEditError::new("Replace All target not found.").into());
    }

    Ok((content.replace(old, new), replacements))
}

fn find_single_line(lines: &[String], needle: &str, start: usize) -> Result<usize> {
    for strategy in MatchStrategy::all() {
        let mut matches: Vec<usize> = Vec::new();
        for (index, line) in lines.iter().enumerate().skip(start) {
            if line_matches(line, needle, strategy) {
                matches.push(index);
            }
        }
        if matches.is_empty() {
            continue;
        }
        if matches.len() > 1 {
            return Err(FileEditError::new(format!(
                "Anchor is not unique after current position: {needle}"
            ))
            .into());
        }
        return Ok(matches[0]);
    }
    Err(FileEditError::new(format!("Anchor not found: {needle}")).into())
}

fn find_sequence(
    lines: &[String],
    needle: &[String],
    start: usize,
    require_unique: bool,
) -> Result<usize> {
    let mut matches: Vec<usize> = Vec::new();
    let length = needle.len();
    if length == 0 {
        return Ok(start);
    }
    if lines.len() < length {
        return Err(FileEditError::new("Update target not found.").into());
    }
    let upper = lines.len() - length;
    for strategy in MatchStrategy::all() {
        matches.clear();
        let mut index = start;
        while index <= upper {
            if sequence_matches_at(lines, needle, index, strategy) {
                matches.push(index);
            }
            index += 1;
        }
        if matches.is_empty() {
            continue;
        }
        if require_unique && matches.len() > 1 {
            return Err(FileEditError::new("Update target is not unique.").into());
        }
        return Ok(matches[0]);
    }
    Err(FileEditError::new("Update target not found.").into())
}

fn find_at_eof(lines: &[String], needle: &[String]) -> Result<usize> {
    if needle.is_empty() {
        return Ok(lines.len());
    }
    if lines.len() < needle.len() {
        return Err(FileEditError::new("End-of-file update target not found.").into());
    }
    let start = lines.len() - needle.len();
    for strategy in MatchStrategy::all() {
        if sequence_matches_at(lines, needle, start, strategy) {
            return Ok(start);
        }
    }
    Err(FileEditError::new("End-of-file update target not found.").into())
}

#[derive(Clone, Copy)]
enum MatchStrategy {
    Exact,
    TrimEnd,
    Trim,
    NormalizeUnicode,
}

impl MatchStrategy {
    fn all() -> [Self; 4] {
        [
            Self::Exact,
            Self::TrimEnd,
            Self::Trim,
            Self::NormalizeUnicode,
        ]
    }
}

fn sequence_matches_at(
    lines: &[String],
    needle: &[String],
    index: usize,
    strategy: MatchStrategy,
) -> bool {
    needle
        .iter()
        .enumerate()
        .all(|(offset, expected)| line_matches(&lines[index + offset], expected, strategy))
}

fn line_matches(actual: &str, expected: &str, strategy: MatchStrategy) -> bool {
    match strategy {
        MatchStrategy::Exact => actual == expected,
        MatchStrategy::TrimEnd => actual.trim_end() == expected.trim_end(),
        MatchStrategy::Trim => actual.trim() == expected.trim(),
        MatchStrategy::NormalizeUnicode => {
            normalize_unicode_punctuation(actual.trim())
                == normalize_unicode_punctuation(expected.trim())
        }
    }
}

fn normalize_unicode_punctuation(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => output.push('\''),
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => output.push('"'),
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' => {
                output.push('-')
            }
            '\u{2026}' => output.push_str("..."),
            '\u{00a0}' => output.push(' '),
            _ => output.push(ch),
        }
    }
    output
}

fn detect_line_ending(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn convert_line_endings(text: &str, line_ending: &str) -> String {
    if line_ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

fn resolve_workspace_path(root: &Path, raw_path: &str) -> Result<PathBuf> {
    let path = Path::new(raw_path);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    resolve_existing_prefix(&lexical_resolve(&joined))
}

/// Normalize `.`, `..`, and duplicate separators before resolving existing
/// ancestors. This keeps nonexistent leaf paths usable while still preventing
/// `..` escapes.
fn lexical_resolve(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn resolve_existing_prefix(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return std::fs::canonicalize(path)
            .with_context(|| format!("resolve path {}", path.display()));
    }

    let mut existing = path;
    let mut missing: Vec<OsString> = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or_else(|| {
            FileEditError::new(format!("Path does not exist: {}", path.display()))
        })?;
        missing.push(name.to_os_string());
        existing = existing.parent().ok_or_else(|| {
            FileEditError::new(format!("Path does not exist: {}", path.display()))
        })?;
    }

    let mut resolved = std::fs::canonicalize(existing)
        .with_context(|| format!("resolve path {}", existing.display()))?;
    for item in missing.iter().rev() {
        resolved.push(item);
    }
    Ok(resolved)
}

fn write_text(path: &Path, content: &str, bom: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent for {}", path.display()))?;
    }
    let mut bytes = Vec::with_capacity(UTF8_BOM.len() + content.len());
    if bom {
        bytes.extend_from_slice(UTF8_BOM);
    }
    bytes.extend_from_slice(content.as_bytes());
    std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
fn read_text(path: &Path) -> Result<String> {
    Ok(read_text_file(path)?.text)
}

fn read_text_file(path: &Path) -> Result<TextFile> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let bom = bytes.starts_with(UTF8_BOM);
    let body = bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes.as_slice());
    let text = std::str::from_utf8(body)
        .map_err(|_| FileEditError::new(format!("File is not UTF-8: {}", path.display())))?;
    Ok(TextFile {
        text: text.to_string(),
        bom,
    })
}

#[cfg(test)]
fn read_utf8_bom_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if !bytes.starts_with(UTF8_BOM) {
        return Err(
            FileEditError::new(format!("File is not UTF-8 with BOM: {}", path.display())).into(),
        );
    }
    let text = std::str::from_utf8(&bytes[UTF8_BOM.len()..]).map_err(|_| {
        FileEditError::new(format!("File is not UTF-8 after BOM: {}", path.display()))
    })?;
    Ok(text.to_string())
}

#[cfg(test)]
fn has_utf8_bom(path: &Path) -> Result<bool> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(bytes.starts_with(UTF8_BOM))
}

#[cfg(test)]
fn utf8_bom_count(path: &Path) -> Result<usize> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut count = 0;
    let mut rest = bytes.as_slice();
    while let Some(next) = rest.strip_prefix(UTF8_BOM) {
        count += 1;
        rest = next;
    }
    Ok(count)
}

#[cfg(test)]
fn write_utf8_bom_text_for_test(path: &Path, content: &str) -> Result<()> {
    let mut bytes = Vec::with_capacity(UTF8_BOM.len() + content.len());
    bytes.extend_from_slice(UTF8_BOM);
    bytes.extend_from_slice(content.as_bytes());
    std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn relative_display(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}

fn is_operation_header(line: &str) -> bool {
    line.starts_with(WRITE_FILE)
        || line.starts_with(DELETE_FILE)
        || line.starts_with(UPDATE_FILE)
        || line.starts_with(REPLACE_ALL)
        || line.trim() == END_PATCH
}

fn non_empty_path(path: &str) -> Result<String> {
    let cleaned = path.trim();
    if cleaned.is_empty() {
        return Err(FileEditError::new("Path must not be empty.").into());
    }
    Ok(cleaned.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn file_edit_adds_and_updates_file() {
        let tmp = tempdir().unwrap();
        let patch = r#"
*** Begin Patch
*** Write File: notes.txt
+alpha
+beta
*** Update File: notes.txt
@@
-beta
+gamma
*** End Patch
"#;

        let output = file_edit_text(patch, tmp.path()).unwrap();

        assert_eq!(output["tool"], "file_edit");
        assert_eq!(output["kind"], "file_edit");
        assert_eq!(output["status"], "completed");
        let path = tmp.path().join("notes.txt");
        assert!(has_utf8_bom(&path).unwrap());
        assert_eq!(read_utf8_bom_text(&path).unwrap(), "alpha\ngamma\n");
    }

    #[test]
    fn file_edit_updates_utf8_without_bom_and_preserves_no_bom() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("notes.txt");
        std::fs::write(&path, "alpha\nbeta\n").unwrap();
        let patch = r#"
*** Begin Patch
*** Update File: notes.txt
@@
-beta
+gamma
*** End Patch
"#;

        let output = file_edit_text(patch, tmp.path()).unwrap();

        assert_eq!(output["status"], "completed");
        assert!(!has_utf8_bom(&path).unwrap());
        assert_eq!(read_text(&path).unwrap(), "alpha\ngamma\n");
    }

    #[test]
    fn file_edit_updates_utf8_bom_without_duplicating_bom() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("notes.txt");
        write_utf8_bom_text_for_test(&path, "alpha\nbeta\n").unwrap();
        let patch = r#"
*** Begin Patch
*** Update File: notes.txt
@@
-beta
+gamma
*** End Patch
"#;

        let output = file_edit_text(patch, tmp.path()).unwrap();

        assert_eq!(output["status"], "completed");
        assert_eq!(utf8_bom_count(&path).unwrap(), 1);
        assert_eq!(read_utf8_bom_text(&path).unwrap(), "alpha\ngamma\n");
    }

    #[test]
    fn file_edit_updates_preserve_crlf() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("notes.txt");
        write_utf8_bom_text_for_test(&path, "alpha\r\nbeta\r\n").unwrap();
        let patch = r#"
*** Begin Patch
*** Update File: notes.txt
@@
-beta
+gamma
*** End Patch
"#;

        let output = file_edit_text(patch, tmp.path()).unwrap();

        assert_eq!(output["status"], "completed");
        assert_eq!(output["modified"][0], "notes.txt");
        assert!(has_utf8_bom(&path).unwrap());
        assert_eq!(read_utf8_bom_text(&path).unwrap(), "alpha\r\ngamma\r\n");
    }

    #[test]
    fn file_edit_rejects_non_utf8_update_target() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.txt"), [0xff, 0xfe, b'a']).unwrap();
        let patch = r#"
*** Begin Patch
*** Update File: notes.txt
@@
-a
+b
*** End Patch
"#;

        let error = file_edit_text(patch, tmp.path()).unwrap_err();

        assert!(error.to_string().contains("File is not UTF-8"));
    }

    #[test]
    fn file_edit_update_matches_with_trailing_whitespace_fallback() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("notes.txt");
        std::fs::write(&path, "alpha   \nbeta\n").unwrap();
        let patch = r#"
*** Begin Patch
*** Update File: notes.txt
@@
-alpha
+gamma
*** End Patch
"#;

        let output = file_edit_text(patch, tmp.path()).unwrap();

        assert_eq!(output["status"], "completed");
        assert_eq!(read_text(&path).unwrap(), "gamma\nbeta\n");
    }

    #[test]
    fn file_edit_update_matches_with_unicode_punctuation_fallback() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("notes.txt");
        std::fs::write(&path, "title: “hello”\n").unwrap();
        let patch = r#"
*** Begin Patch
*** Update File: notes.txt
@@
-title: "hello"
+title: "hi"
*** End Patch
"#;

        let output = file_edit_text(patch, tmp.path()).unwrap();

        assert_eq!(output["status"], "completed");
        assert_eq!(read_text(&path).unwrap(), "title: \"hi\"\n");
    }

    #[test]
    fn file_edit_update_fallback_still_rejects_non_unique_targets() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("notes.txt");
        std::fs::write(&path, "alpha   \nalpha\t\n").unwrap();
        let patch = r#"
*** Begin Patch
*** Update File: notes.txt
@@
-alpha
+gamma
*** End Patch
"#;

        let error = file_edit_text(patch, tmp.path()).unwrap_err();

        assert!(error.to_string().contains("Update target is not unique"));
    }

    #[test]
    fn replace_all_replaces_unique_text() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("notes.txt");
        std::fs::write(&path, "alpha\nbeta\n").unwrap();
        let patch = r#"
*** Begin Patch
*** Replace All: notes.txt
-beta
+gamma
*** End Patch
"#;

        let output = file_edit_text(patch, tmp.path()).unwrap();

        assert_eq!(output["tool"], "file_edit");
        assert_eq!(output["kind"], "file_edit");
        assert_eq!(output["status"], "completed");
        assert_eq!(output["replacements"][0]["count"], 1);
        assert_eq!(read_text(&path).unwrap(), "alpha\ngamma\n");
        assert!(!has_utf8_bom(&path).unwrap());
    }

    #[test]
    fn write_file_operation_creates_and_overwrites() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("notes.txt");

        let output = file_edit_text(
            "*** Begin Patch\n*** Write File: notes.txt\n+alpha\n*** End Patch",
            tmp.path(),
        )
        .unwrap();
        assert_eq!(output["tool"], "file_edit");
        assert_eq!(output["kind"], "file_edit");
        assert_eq!(output["status"], "completed");
        assert_eq!(output["added"][0], "notes.txt");
        assert!(has_utf8_bom(&path).unwrap());
        assert_eq!(read_utf8_bom_text(&path).unwrap(), "alpha\n");

        let output = file_edit_text(
            "*** Begin Patch\n*** Write File: notes.txt\n+beta\n*** End Patch",
            tmp.path(),
        )
        .unwrap();
        assert_eq!(output["status"], "completed");
        assert_eq!(output["modified"][0], "notes.txt");
        assert_eq!(read_utf8_bom_text(&path).unwrap(), "beta\n");
    }

    #[test]
    fn replace_all_preserves_crlf_and_bom() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("notes.txt");
        write_utf8_bom_text_for_test(&path, "alpha\r\nbeta\r\n").unwrap();
        let patch = r#"
*** Begin Patch
*** Replace All: notes.txt
-alpha
-beta
+gamma
+delta
*** End Patch
"#;

        let output = file_edit_text(patch, tmp.path()).unwrap();

        assert_eq!(output["status"], "completed");
        assert_eq!(output["replacements"][0]["count"], 1);
        assert!(has_utf8_bom(&path).unwrap());
        assert_eq!(read_utf8_bom_text(&path).unwrap(), "gamma\r\ndelta\r\n");
    }

    #[test]
    fn replace_all_replaces_all_matches() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("notes.txt");
        std::fs::write(&path, "alpha\nalpha\n").unwrap();
        let patch = r#"
*** Begin Patch
*** Replace All: notes.txt
-alpha
+beta
*** End Patch
"#;

        let output = file_edit_text(patch, tmp.path()).unwrap();

        assert_eq!(output["replacements"][0]["count"], 2);
        assert_eq!(read_text(&path).unwrap(), "beta\nbeta\n");
    }

    #[test]
    fn replace_all_rejects_empty_old_text() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "alpha\n").unwrap();
        let patch = r#"
*** Begin Patch
*** Replace All: notes.txt
-
+beta
*** End Patch
"#;

        let error = file_edit_text(patch, tmp.path()).unwrap_err();

        assert!(error.to_string().contains("old text must not be empty"));
    }

    #[test]
    fn file_edit_allows_parent_paths() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let patch = r#"
*** Begin Patch
*** Write File: ../outside.txt
+outside
*** End Patch
"#;

        let output = file_edit_text(patch, &workspace).unwrap();

        let path = tmp.path().join("outside.txt");
        assert_eq!(output["status"], "completed");
        assert!(has_utf8_bom(&path).unwrap());
        assert_eq!(read_utf8_bom_text(&path).unwrap(), "outside\n");
    }

    #[test]
    fn file_edit_allows_absolute_paths() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("absolute.txt");
        let patch = format!(
            "*** Begin Patch\n*** Write File: {}\n+absolute\n*** End Patch\n",
            path.display()
        );

        let output = file_edit_text(&patch, tmp.path()).unwrap();

        assert_eq!(output["status"], "completed");
        assert!(has_utf8_bom(&path).unwrap());
        assert_eq!(read_utf8_bom_text(&path).unwrap(), "absolute\n");
    }
}
