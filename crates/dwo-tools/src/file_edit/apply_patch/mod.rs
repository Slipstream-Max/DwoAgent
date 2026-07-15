mod parser;
mod seek_sequence;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use parser::{Hunk, UpdateChunk, parse_patch};
use seek_sequence::seek_sequence;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PatchChange {
    pub path: PathBuf,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved_to: Option<PathBuf>,
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
pub fn apply_patch(patch: &str, cwd: &Path) -> Result<Vec<PatchChange>> {
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
                let updated = apply_chunks(&original.text, &chunks)?;
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
    Ok(changes)
}

fn apply_chunks(content: &str, chunks: &[UpdateChunk]) -> Result<String> {
    let mut lines = split_lines(content);
    let mut cursor = 0;
    for chunk in chunks {
        if let Some(context) = &chunk.context {
            let context_index = seek_sequence(&lines, std::slice::from_ref(context), cursor, false)
                .ok_or_else(|| anyhow::anyhow!("Update context not found: {context}"))?;
            cursor = context_index + 1;
        }
        let index = seek_sequence(&lines, &chunk.old_lines, cursor, chunk.end_of_file)
            .ok_or_else(|| anyhow::anyhow!("Update hunk did not match target file"))?;
        let old_len = chunk.old_lines.len();
        lines.splice(index..index + old_len, chunk.new_lines.clone());
        cursor = index + chunk.new_lines.len();
    }
    Ok(lines.join("\n"))
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
}
