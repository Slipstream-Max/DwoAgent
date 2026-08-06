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

#[derive(Debug)]
pub struct PatchApplication {
    pub changes: Vec<PatchChange>,
    pub git_patch: String,
}

struct AppliedTextChange {
    source: PathBuf,
    destination: PathBuf,
    old: Vec<u8>,
    new: Vec<u8>,
}

/// OpenAI Codex apply-patch semantics behind dwoagent's local file_edit surface.
/// The CLI, shell interception, streaming input, sandbox adapters, and remote
/// filesystem transport intentionally remain outside this module.
pub fn apply_patch(patch: &str, cwd: &Path) -> Result<PatchApplication> {
    let hunks = parse_patch(patch)?;
    if hunks.is_empty() {
        bail!("No files were modified.");
    }

    let mut changes = Vec::with_capacity(hunks.len());
    let mut applied_text = Vec::with_capacity(hunks.len());

    for hunk in hunks {
        match hunk {
            Hunk::Add { path, contents } => {
                let path = resolve_path(cwd, &path);
                let old = fs::read(&path).unwrap_or_default();
                let new = contents.into_bytes();
                write_file(&path, &new)?;
                applied_text.push(AppliedTextChange {
                    source: path.clone(),
                    destination: path.clone(),
                    old,
                    new,
                });
                changes.push(PatchChange {
                    path,
                    kind: "add",
                    moved_to: None,
                });
            }
            Hunk::Delete { path } => {
                let path = resolve_path(cwd, &path);
                let old = fs::read(&path).unwrap_or_default();
                fs::remove_file(&path)
                    .with_context(|| format!("Failed to delete file {}", path.display()))?;
                applied_text.push(AppliedTextChange {
                    source: path.clone(),
                    destination: path.clone(),
                    old,
                    new: Vec::new(),
                });
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
                let is_move = move_path.is_some();
                let destination = move_path
                    .as_ref()
                    .map(|path| resolve_path(cwd, path))
                    .unwrap_or_else(|| source.clone());
                let original = fs::read_to_string(&source).with_context(|| {
                    format!("Failed to read file to update {}", source.display())
                })?;
                let updated = derive_new_contents(&original, &source, &chunks)?;
                write_file(&destination, updated.as_bytes())?;
                if is_move {
                    fs::remove_file(&source).with_context(|| {
                        format!("Failed to remove original {}", source.display())
                    })?;
                }
                applied_text.push(AppliedTextChange {
                    source: source.clone(),
                    destination: destination.clone(),
                    old: original.into_bytes(),
                    new: updated.into_bytes(),
                });
                changes.push(PatchChange {
                    path: source,
                    kind: if is_move { "move" } else { "update" },
                    moved_to: is_move.then_some(destination),
                });
            }
        }
    }

    Ok(PatchApplication {
        changes,
        git_patch: render_git_patch(&applied_text),
    })
}

fn derive_new_contents(original: &str, path: &Path, chunks: &[UpdateChunk]) -> Result<String> {
    let mut original_lines = original.split('\n').map(str::to_string).collect::<Vec<_>>();
    if original_lines.last().is_some_and(String::is_empty) {
        original_lines.pop();
    }

    let replacements = compute_replacements(&original_lines, path, chunks)?;
    let mut new_lines = apply_replacements(original_lines, &replacements);
    if !new_lines.last().is_some_and(String::is_empty) {
        new_lines.push(String::new());
    }
    Ok(new_lines.join("\n"))
}

fn compute_replacements(
    original_lines: &[String],
    path: &Path,
    chunks: &[UpdateChunk],
) -> Result<Vec<(usize, usize, Vec<String>)>> {
    let mut replacements = Vec::new();
    let mut line_index = 0;

    for chunk in chunks {
        if let Some(context) = &chunk.context {
            let Some(index) = seek_sequence(
                original_lines,
                std::slice::from_ref(context),
                line_index,
                false,
            ) else {
                bail!("Update context not found in {}: {context}", path.display());
            };
            line_index = index + 1;
        }

        if chunk.old_lines.is_empty() {
            let insertion_index = if original_lines.last().is_some_and(String::is_empty) {
                original_lines.len() - 1
            } else {
                original_lines.len()
            };
            replacements.push((insertion_index, 0, chunk.new_lines.clone()));
            continue;
        }

        let mut pattern = chunk.old_lines.as_slice();
        let mut new_lines = chunk.new_lines.as_slice();
        let mut found = seek_sequence(original_lines, pattern, line_index, chunk.end_of_file);
        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern = &pattern[..pattern.len() - 1];
            if new_lines.last().is_some_and(String::is_empty) {
                new_lines = &new_lines[..new_lines.len() - 1];
            }
            found = seek_sequence(original_lines, pattern, line_index, chunk.end_of_file);
        }

        let Some(start) = found else {
            bail!(
                "Update hunk did not match target file {}.\nExpected lines:\n{}",
                path.display(),
                chunk.old_lines.join("\n")
            );
        };
        replacements.push((start, pattern.len(), new_lines.to_vec()));
        line_index = start + pattern.len();
    }

    replacements.sort_by_key(|(index, _, _)| *index);
    Ok(replacements)
}

fn apply_replacements(
    mut lines: Vec<String>,
    replacements: &[(usize, usize, Vec<String>)],
) -> Vec<String> {
    for (start, old_len, new_lines) in replacements.iter().rev() {
        lines.splice(*start..*start + *old_len, new_lines.clone());
    }
    lines
}

fn render_git_patch(changes: &[AppliedTextChange]) -> String {
    changes
        .iter()
        .filter_map(|change| {
            let old = String::from_utf8(change.old.clone()).ok()?;
            let new = String::from_utf8(change.new.clone()).ok()?;
            let old_name = git_path(&change.source);
            let new_name = git_path(&change.destination);
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

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create parent directory {}", parent.display()))?;
    }
    fs::write(path, bytes).with_context(|| format!("Failed to write file {}", path.display()))
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
    fn add_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        apply_patch(
            "*** Begin Patch\n*** Add File: a.txt\n+new\n*** End Patch",
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn move_overwrites_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("source.txt"), "old\n").unwrap();
        fs::write(dir.path().join("destination.txt"), "existing\n").unwrap();
        apply_patch(
            "*** Begin Patch\n*** Update File: source.txt\n*** Move to: destination.txt\n@@\n-old\n+new\n*** End Patch",
            dir.path(),
        )
        .unwrap();
        assert!(!dir.path().join("source.txt").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("destination.txt")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn pure_addition_appends_to_end_of_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        apply_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n+three\n*** End Patch",
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );
    }

    #[test]
    fn later_failure_preserves_earlier_changes() {
        let dir = tempfile::tempdir().unwrap();
        let error = apply_patch(
            "*** Begin Patch\n*** Add File: created.txt\n+hello\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch",
            dir.path(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("Failed to read file to update"));
        assert_eq!(
            fs::read_to_string(dir.path().join("created.txt")).unwrap(),
            "hello\n"
        );
    }

    #[test]
    fn repeated_update_hunks_see_prior_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        apply_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** Update File: a.txt\n@@\n-two\n+TWO\n*** End Patch",
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "ONE\nTWO\n"
        );
    }

    #[test]
    fn update_appends_final_newline() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "old").unwrap();
        apply_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch",
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn multiple_chunks_apply_against_original_line_positions() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        apply_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-two\n+TWO\n@@\n-four\n+FOUR\n*** End Patch",
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\nTWO\nthree\nFOUR\n"
        );
    }

    #[test]
    fn end_of_file_marker_requires_tail_match() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "first\nsecond\nthird\n").unwrap();
        let error = apply_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-first\n+FIRST\n*** End of File\n*** End Patch",
            dir.path(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("did not match target file"));
    }

    #[test]
    fn unicode_punctuation_is_fuzzy_matched() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "value — quoted “text”\n").unwrap();
        apply_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-value - quoted \"text\"\n+updated\n*** End Patch",
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "updated\n"
        );
    }

    #[test]
    fn empty_patch_is_rejected_at_application_time() {
        let dir = tempfile::tempdir().unwrap();
        let error = apply_patch("*** Begin Patch\n*** End Patch", dir.path()).unwrap_err();
        assert_eq!(error.to_string(), "No files were modified.");
    }
}
