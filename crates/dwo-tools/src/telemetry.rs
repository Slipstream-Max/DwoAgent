use std::path::PathBuf;
use std::sync::Arc;

use crate::file_edit::PatchChange;

#[derive(Debug, Clone)]
pub enum ToolEvent {
    TerminalOpened {
        tool_call_id: String,
        terminal_id: String,
        command: String,
        cwd: PathBuf,
    },
    TerminalOutput {
        terminal_id: String,
        data: Vec<u8>,
    },
    TerminalExited {
        terminal_id: String,
        exit_code: Option<i32>,
        status: String,
    },
    FileRead {
        tool_call_id: String,
        path: PathBuf,
    },
    FileChanged {
        tool_call_id: String,
        changes: Vec<PatchChange>,
        patch: String,
    },
}

pub type ToolEventHandler = Arc<dyn Fn(ToolEvent) + Send + Sync>;
