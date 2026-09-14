use std::path::PathBuf;

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
