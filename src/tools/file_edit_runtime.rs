//! Small file edit runtime for text edits.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use thiserror::Error;

pub const BEGIN_PATCH: &str = "*** Begin Patch";
pub const END_PATCH: &str = "*** End Patch";
pub const ADD_FILE: &str = "*** Add File: ";
pub const DELETE_FILE: &str = "*** Delete File: ";
pub const UPDATE_FILE: &str = "*** Update File: ";
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
pub struct AddFile {
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
pub enum Operation {
    Add(AddFile),
    Delete(DeleteFile),
    Update(UpdateFile),
}

/// Entry point — parses a patch and resolves relative paths against *cwd*.
pub fn file_edit_text(patch: &str, cwd: &Path) -> Result<Value> {
    let operations = parse_patch(patch)?;
    let root = std::fs::canonicalize(cwd)
        .with_context(|| format!("resolve workspace root {}", cwd.display()))?;

    let mut added: Vec<String> = Vec::new();
    let mut modified: Vec<String> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();

    for operation in operations {
        match operation {
            Operation::Add(add) => {
                let path = resolve_workspace_path(&root, &add.path)?;
                let content = format!("{}\n", add.lines.join("\n"));
                write_text(&path, &content)?;
                added.push(relative_display(&root, &path));
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
                let original = read_text(&source_path)?;
                let updated = apply_update_segments(&original, &update.segments)?;

                if let Some(dest_raw) = update.move_to {
                    let dest_path = resolve_workspace_path(&root, &dest_raw)?;
                    write_text(&dest_path, &updated)?;
                    std::fs::remove_file(&source_path)
                        .with_context(|| format!("remove {}", source_path.display()))?;
                    modified.push(relative_display(&root, &dest_path));
                    deleted.push(relative_display(&root, &source_path));
                } else {
                    write_text(&source_path, &updated)?;
                    modified.push(relative_display(&root, &source_path));
                }
            }
        }
    }

    Ok(json!({
        "status": "completed_success",
        "done": true,
        "added": added,
        "modified": modified,
        "deleted": deleted,
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
        if let Some(rest) = line.strip_prefix(ADD_FILE) {
            let (op, next) = parse_add_file(&lines, index, end_index, rest)?;
            operations.push(Operation::Add(op));
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

fn parse_add_file(
    lines: &[&str],
    start: usize,
    end_index: usize,
    first_rest: &str,
) -> Result<(AddFile, usize)> {
    let path = non_empty_path(first_rest)?;
    let mut index = start + 1;
    let mut add_lines: Vec<String> = Vec::new();
    while index < end_index && !is_operation_header(lines[index]) {
        let line = lines[index];
        if !line.starts_with('+') {
            return Err(FileEditError::new("Add File body lines must start with '+'.").into());
        }
        add_lines.push(line[1..].to_string());
        index += 1;
    }
    if add_lines.is_empty() {
        return Err(FileEditError::new(format!("Add File body is empty: {path}")).into());
    }
    Ok((
        AddFile {
            path,
            lines: add_lines,
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

fn find_single_line(lines: &[String], needle: &str, start: usize) -> Result<usize> {
    let mut matches: Vec<usize> = Vec::new();
    for (index, line) in lines.iter().enumerate().skip(start) {
        if line == needle {
            matches.push(index);
        }
    }
    if matches.is_empty() {
        return Err(FileEditError::new(format!("Anchor not found: {needle}")).into());
    }
    if matches.len() > 1 {
        return Err(FileEditError::new(format!(
            "Anchor is not unique after current position: {needle}"
        ))
        .into());
    }
    Ok(matches[0])
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
    let upper = lines.len().saturating_sub(length);
    let mut index = start;
    while index <= upper {
        if &lines[index..index + length] == needle {
            matches.push(index);
        }
        index += 1;
    }
    if matches.is_empty() {
        return Err(FileEditError::new("Update target not found.").into());
    }
    if require_unique && matches.len() > 1 {
        return Err(FileEditError::new("Update target is not unique.").into());
    }
    Ok(matches[0])
}

fn find_at_eof(lines: &[String], needle: &[String]) -> Result<usize> {
    if needle.is_empty() {
        return Ok(lines.len());
    }
    if lines.len() < needle.len() {
        return Err(FileEditError::new("End-of-file update target not found.").into());
    }
    let start = lines.len() - needle.len();
    if &lines[start..] != needle {
        return Err(FileEditError::new("End-of-file update target not found.").into());
    }
    Ok(start)
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

fn write_text(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent for {}", path.display()))?;
    }
    let mut bytes = Vec::with_capacity(UTF8_BOM.len() + content.len());
    bytes.extend_from_slice(UTF8_BOM);
    bytes.extend_from_slice(content.as_bytes());
    std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn read_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let body = bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes.as_slice());
    let text = std::str::from_utf8(body)
        .map_err(|_| FileEditError::new(format!("File is not UTF-8: {}", path.display())))?;
    Ok(text.to_string())
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
    line.starts_with(ADD_FILE)
        || line.starts_with(DELETE_FILE)
        || line.starts_with(UPDATE_FILE)
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
*** Add File: notes.txt
+alpha
+beta
*** Update File: notes.txt
@@
-beta
+gamma
*** End Patch
"#;

        let output = file_edit_text(patch, tmp.path()).unwrap();

        assert_eq!(output["status"], "completed_success");
        let path = tmp.path().join("notes.txt");
        assert!(has_utf8_bom(&path).unwrap());
        assert_eq!(read_utf8_bom_text(&path).unwrap(), "alpha\ngamma\n");
    }

    #[test]
    fn file_edit_updates_utf8_without_bom_and_writes_bom() {
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

        assert_eq!(output["status"], "completed_success");
        assert!(has_utf8_bom(&path).unwrap());
        assert_eq!(read_utf8_bom_text(&path).unwrap(), "alpha\ngamma\n");
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

        assert_eq!(output["status"], "completed_success");
        assert_eq!(utf8_bom_count(&path).unwrap(), 1);
        assert_eq!(read_utf8_bom_text(&path).unwrap(), "alpha\ngamma\n");
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
    fn file_edit_allows_parent_paths() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let patch = r#"
*** Begin Patch
*** Add File: ../outside.txt
+outside
*** End Patch
"#;

        let output = file_edit_text(patch, &workspace).unwrap();

        let path = tmp.path().join("outside.txt");
        assert_eq!(output["status"], "completed_success");
        assert!(has_utf8_bom(&path).unwrap());
        assert_eq!(read_utf8_bom_text(&path).unwrap(), "outside\n");
    }

    #[test]
    fn file_edit_allows_absolute_paths() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("absolute.txt");
        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+absolute\n*** End Patch\n",
            path.display()
        );

        let output = file_edit_text(&patch, tmp.path()).unwrap();

        assert_eq!(output["status"], "completed_success");
        assert!(has_utf8_bom(&path).unwrap());
        assert_eq!(read_utf8_bom_text(&path).unwrap(), "absolute\n");
    }
}
