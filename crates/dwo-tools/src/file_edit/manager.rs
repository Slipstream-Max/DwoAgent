use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;
use tokio::sync::Mutex;

use super::{PatchChange, apply_patch};

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
        let changes = tokio::task::spawn_blocking(move || apply_patch(&patch, &cwd)).await??;
        Ok(FileEditResult { changes })
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
}
