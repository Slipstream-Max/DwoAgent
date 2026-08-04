mod parser;
mod seek_sequence;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use parser::{Hunk, UpdateChunk, parse_patch};
use seek_sequence::{best_partial_match, seek_sequence};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PatchChange {
    pub path: PathBuf,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved_to: Option<PathBuf>,
}

#[derive(Debug)]
pub struct PatchApplication {
    pub changes: Vec<PatchChange>,
    pub git_patch: String,
}

struct TextFile {
    text: String,
    bom: bool,
    line_ending: &'static str,
    final_newline: bool,
}

enum PreparedChange {
    Add {
        path: PathBuf,
        bytes: Vec<u8>,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        source: PathBuf,
        destination: PathBuf,
        bytes: Vec<u8>,
    },
}

/// Parser and matching behavior derived from OpenAI Codex `codex-rs/apply-patch`.
/// Filesystem application is local-only and intentionally omits its CLI, shell
/// interception, streaming parser, sandbox adapters, and remote filesystem API.
pub fn apply_patch(patch: &str, cwd: &Path) -> Result<PatchApplication> {
    let hunks = parse_patch(patch)?;
    let mut prepared = Vec::with_capacity(hunks.len());
    let mut changes = Vec::with_capacity(hunks.len());

    for hunk in hunks {
        match hunk {
            Hunk::Add { path, contents } => {
                let path = resolve_path(cwd, &path);
                if path.exists() {
                    bail!("Add File target already exists: {}", path.display());
                }
                prepared.push(PreparedChange::Add {
                    path: path.clone(),
                    bytes: contents.into_bytes(),
                });
                changes.push(PatchChange {
                    path,
                    kind: "add",
                    moved_to: None,
                });
            }
            Hunk::Delete { path } => {
                let path = resolve_path(cwd, &path);
                if !path.is_file() {
                    bail!("Delete File target does not exist: {}", path.display());
                }
                prepared.push(PreparedChange::Delete { path: path.clone() });
                changes.push(PatchChange {
                    path,
                    kind: "delete",
                    moved_to: None,
                });
            }
            Hunk::Update {
                path,
                move_path,
                chunks,
            } => {
                let source = resolve_path(cwd, &path);
                let destination = move_path
                    .as_ref()
                    .map(|path| resolve_path(cwd, path))
                    .unwrap_or_else(|| source.clone());
                if !source.is_file() {
                    bail!("Update File target does not exist: {}", source.display());
                }
                if destination != source && destination.exists() {
                    bail!("Move destination already exists: {}", destination.display());
                }
                let original = read_text_file(&source)?;
                let updated = apply_chunks(&original.text, &chunks)
                    .with_context(|| format!("apply update to {}", source.display()))?;
                let updated = restore_text_format(&updated, &original);
                let mut bytes = Vec::with_capacity(updated.len() + usize::from(original.bom) * 3);
                if original.bom {
                    bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
                }
                bytes.extend_from_slice(updated.as_bytes());
                prepared.push(PreparedChange::Update {
                    source: source.clone(),
                    destination: destination.clone(),
                    bytes,
                });
                changes.push(PatchChange {
                    path: source,
                    kind: if destination == resolve_path(cwd, &path) {
                        "update"
                    } else {
                        "move"
                    },
                    moved_to: (destination != resolve_path(cwd, &path)).then_some(destination),
                });
            }
        }
    }

    let git_patch = render_git_patch(&prepared);
    for change in prepared {
        match change {
            PreparedChange::Add { path, bytes } => write_file(&path, &bytes)?,
            PreparedChange::Delete { path } => {
                fs::remove_file(&path)
                    .with_context(|| format!("delete file {}", path.display()))?;
            }
            PreparedChange::Update {
                source,
                destination,
                bytes,
            } => {
                write_file(&destination, &bytes)?;
                if destination != source {
                    fs::remove_file(&source)
                        .with_context(|| format!("remove moved source {}", source.display()))?;
                }
            }
        }
    }
    Ok(PatchApplication { changes, git_patch })
}

fn render_git_patch(changes: &[PreparedChange]) -> String {
    changes
        .iter()
        .filter_map(|change| {
            let (old_path, new_path, old, new) = match change {
                PreparedChange::Add { path, bytes } => (path, path, Vec::new(), bytes.clone()),
                PreparedChange::Delete { path } => (path, path, fs::read(path).ok()?, Vec::new()),
                PreparedChange::Update {
                    source,
                    destination,
                    bytes,
                } => (source, destination, fs::read(source).ok()?, bytes.clone()),
            };
            let old = String::from_utf8(old).ok()?;
            let new = String::from_utf8(new).ok()?;
            let old_name = git_path(old_path);
            let new_name = git_path(new_path);
            let mut options = diffy::DiffOptions::new();
            options
                .set_original_filename(old_name.clone())
                .set_modified_filename(new_name.clone());
            let patch = options.create_patch(&old, &new).to_string();
            Some(format!("diff --git {old_name} {new_name}\n{patch}"))
        })
        .collect::<Vec<_>>()
        .join("")
}

fn git_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn apply_chunks(content: &str, chunks: &[UpdateChunk]) -> Result<String> {
    let mut lines = split_lines(content);
    let mut cursor = 0;
    for chunk in chunks {
        let anchor = if let Some(context) = &chunk.context {
            let context_index = seek_sequence(&lines, std::slice::from_ref(context), cursor, false)
                .ok_or_else(|| context_not_found_error(&lines, context, cursor))?;
            cursor = context_index + 1;
            Some((context.as_str(), context_index))
        } else {
            None
        };
        let index = seek_sequence(&lines, &chunk.old_lines, cursor, chunk.end_of_file)
            .ok_or_else(|| hunk_mismatch_error(&lines, chunk, anchor, cursor))?;
        let old_len = chunk.old_lines.len();
        lines.splice(index..index + old_len, chunk.new_lines.clone());
        cursor = index + chunk.new_lines.len();
    }
    Ok(lines.join("\n"))
}

fn context_not_found_error(lines: &[String], context: &str, search_start: usize) -> anyhow::Error {
    let actual = lines.get(search_start).map(String::as_str);
    anyhow::anyhow!(
        "Update context not found.\nAnchor: {context:?}\nSearch started at target line {}.\nFirst mismatch:\n  expected anchor: {}\n  actual target: {}\nTarget file near line {}:\n{}",
        search_start + 1,
        display_line(Some(context)),
        display_line(actual),
        search_start + 1,
        render_excerpt(lines, search_start),
    )
}

fn hunk_mismatch_error(
    lines: &[String],
    chunk: &UpdateChunk,
    anchor: Option<(&str, usize)>,
    search_start: usize,
) -> anyhow::Error {
    let partial = best_partial_match(lines, &chunk.old_lines, search_start, chunk.end_of_file);
    let expected = chunk.old_lines.get(partial.matched).map(String::as_str);
    let target_index = partial.index + partial.matched;
    let actual = lines.get(target_index).map(String::as_str);
    let anchor_description = anchor.map_or_else(
        || "<none>".to_string(),
        |(context, index)| format!("{context:?} at target line {}", index + 1),
    );
    anyhow::anyhow!(
        "Update hunk did not match target file.\nAnchor: {anchor_description}\nSearch started at target line {}.\nFirst mismatch at patch old line {} and target line {}:\n  expected: {}\n  actual: {}\nTarget file near line {}:\n{}",
        search_start + 1,
        partial.matched + 1,
        target_index + 1,
        display_line(expected),
        display_line(actual),
        target_index + 1,
        render_excerpt(lines, target_index),
    )
}

fn display_line(line: Option<&str>) -> String {
    line.map_or_else(|| "<end of file>".to_string(), |line| format!("{line:?}"))
}

fn render_excerpt(lines: &[String], center: usize) -> String {
    if lines.is_empty() {
        return "  <empty file>".to_string();
    }
    let center = center.min(lines.len() - 1);
    let start = center.saturating_sub(2);
    let end = (center + 3).min(lines.len());
    lines[start..end]
        .iter()
        .enumerate()
        .map(|(offset, line)| format!("  {:>4} | {}", start + offset + 1, truncate_line(line)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_line(line: &str) -> String {
    const LIMIT: usize = 200;
    let mut chars = line.chars();
    let prefix: String = chars.by_ref().take(LIMIT).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

fn split_lines(content: &str) -> Vec<String> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let without_final = normalized.strip_suffix('\n').unwrap_or(&normalized);
    if without_final.is_empty() {
        Vec::new()
    } else {
        without_final.split('\n').map(str::to_string).collect()
    }
}

fn restore_text_format(content: &str, original: &TextFile) -> String {
    let mut output = if original.line_ending == "\n" {
        content.to_string()
    } else {
        content.replace('\n', original.line_ending)
    };
    if original.final_newline && !output.ends_with(original.line_ending) {
        output.push_str(original.line_ending);
    }
    output
}

fn read_text_file(path: &Path) -> Result<TextFile> {
    let bytes = fs::read(path).with_context(|| format!("read file {}", path.display()))?;
    let (bom, text_bytes) = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        (true, &bytes[3..])
    } else {
        (false, bytes.as_slice())
    };
    let text = String::from_utf8(text_bytes.to_vec())
        .with_context(|| format!("file is not UTF-8: {}", path.display()))?;
    let line_ending = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let final_newline = text.ends_with('\n') || text.ends_with('\r');
    Ok(TextFile {
        text,
        bom,
        line_ending,
        final_newline,
    })
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create parent directory {}", parent.display()))?;
    }
    fs::write(path, bytes).with_context(|| format!("write file {}", path.display()))
}

fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_update_move_delete() {
        let dir = tempfile::tempdir().unwrap();
        apply_patch(
            "*** Begin Patch\n*** Add File: a.txt\n+one\n+two\n*** End Patch",
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\ntwo\n"
        );
        apply_patch(
            "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n@@\n-one\n+ONE\n*** End Patch",
            dir.path(),
        )
        .unwrap();
        assert!(!dir.path().join("a.txt").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "ONE\ntwo\n"
        );
        apply_patch(
            "*** Begin Patch\n*** Delete File: b.txt\n*** End Patch",
            dir.path(),
        )
        .unwrap();
        assert!(!dir.path().join("b.txt").exists());
    }

    #[test]
    fn update_preserves_crlf_bom_and_no_final_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        fs::write(
            &path,
            [0xEF, 0xBB, 0xBF]
                .into_iter()
                .chain(*b"one\r\ntwo")
                .collect::<Vec<_>>(),
        )
        .unwrap();
        apply_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** End Patch",
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            [0xEF, 0xBB, 0xBF]
                .into_iter()
                .chain(*b"ONE\r\ntwo")
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn mismatch_reports_anchor_first_difference_and_excerpt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        fs::write(&path, "heading\nbadge\nold value\ntail\n").unwrap();
        let error = apply_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@ heading\n badge\n-expected value\n+new value\n*** End Patch",
            dir.path(),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("Anchor: \"heading\" at target line 1"));
        assert!(message.contains("First mismatch at patch old line 2 and target line 3"));
        assert!(message.contains("expected: \"expected value\""));
        assert!(message.contains("actual: \"old value\""));
        assert!(message.contains("3 | old value"));
    }

    #[test]
    fn missing_anchor_reports_target_excerpt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        fs::write(&path, "one\ntwo\n").unwrap();
        let error = apply_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@ missing\n-two\n+TWO\n*** End Patch",
            dir.path(),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("Anchor: \"missing\""));
        assert!(message.contains("expected anchor: \"missing\""));
        assert!(message.contains("actual target: \"one\""));
        assert!(message.contains("1 | one"));
    }

    #[test]
    fn unprefixed_blank_lines_between_additions_are_inserted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        fs::write(&path, "marker\n").unwrap();
        apply_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@ marker\n+i = 1\n\n\n+def abc()\n*** End Patch",
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "marker\ni = 1\n\n\ndef abc()\n"
        );
    }
}
