use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use tokio::sync::Mutex;

use super::{PatchChange, PatchFailure, apply_patch};

/// Global FIFO shared by every loaded session.
pub struct FileEditManager {
    operation: Mutex<()>,
}

impl FileEditManager {
    pub fn new() -> Self {
        Self {
            operation: Mutex::new(()),
        }
    }

    pub async fn execute(&self, patch: String, cwd: PathBuf) -> Result<FileEditResult> {
        let _operation = self.operation.lock().await;
        let applied = tokio::task::spawn_blocking(move || apply_patch(&patch, &cwd)).await??;
        Ok(FileEditResult {
            changes: applied.changes,
            patch: applied.git_patch,
            failure: applied.failure,
            skipped: applied.skipped,
        })
    }
}

impl Default for FileEditManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEditResult {
    pub changes: Vec<PatchChange>,
    pub patch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<PatchFailure>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

impl FileEditResult {
    pub fn model_text(&self, cwd: &Path) -> String {
        let applied = self.changes.len();
        let mut text = String::new();
        match &self.failure {
            None => {
                text.push_str("Applied patch:");
                for change in &self.changes {
                    text.push_str(&format!("\n- {}", change_line(change, cwd)));
                }
            }
            Some(failure) => {
                if applied == 0 {
                    text.push_str("Patch failed, no changes were applied.");
                } else {
                    let total = applied + 1 + self.skipped.len();
                    text.push_str(&format!("Applied {applied} of {total} operations:"));
                    for change in &self.changes {
                        text.push_str(&format!("\n- {}", change_line(change, cwd)));
                    }
                }
                text.push_str(&format!(
                    "\nFailed step {} ({}): {}",
                    failure.step, failure.operation, failure.error
                ));
                if !self.skipped.is_empty() {
                    text.push_str("\nSkipped:");
                    for label in &self.skipped {
                        text.push_str(&format!("\n- {label}"));
                    }
                }
                text.push_str(
                    "\nFix the failed step and send the remaining operations in a new patch.",
                );
            }
        }
        text
    }
}

fn change_line(change: &PatchChange, cwd: &Path) -> String {
    let path = relative_path(&change.path, cwd);
    match &change.moved_to {
        Some(target) => format!("move: {path} -> {}", relative_path(target, cwd)),
        None => format!("{}: {path}", change.kind),
    }
}

fn relative_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str, kind: &'static str) -> PatchChange {
        PatchChange {
            path: Path::new("C:/work").join(path),
            kind,
            moved_to: None,
        }
    }

    #[test]
    fn model_text_reports_applied_failed_and_skipped_steps() {
        let cwd = Path::new("C:/work");
        let applied = FileEditResult {
            changes: vec![change("a.txt", "add")],
            patch: String::new(),
            failure: None,
            skipped: Vec::new(),
        };
        assert_eq!(applied.model_text(cwd), "Applied patch:\n- add: a.txt");

        let partial = FileEditResult {
            changes: vec![change("a.txt", "add")],
            patch: String::new(),
            failure: Some(PatchFailure {
                step: 2,
                operation: "Update File: missing.txt".to_string(),
                error: "Failed to read file to update missing.txt".to_string(),
            }),
            skipped: vec!["Delete File: keep.txt".to_string()],
        };
        let text = partial.model_text(cwd);
        assert!(text.starts_with("Applied 1 of 3 operations:\n- add: a.txt"));
        assert!(text.contains("Failed step 2 (Update File: missing.txt): Failed to read"));
        assert!(text.contains("Skipped:\n- Delete File: keep.txt"));
        assert!(text.contains("Fix the failed step"));
    }
}
