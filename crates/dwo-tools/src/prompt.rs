//! Static system-prompt fragments for tool usage guidance.

pub const COMMON: &str = include_str!("../prompts/common.md");
pub const TERMINAL: &str = include_str!("../prompts/terminal.md");
pub const READ_FILE: &str = include_str!("../prompts/read_file.md");
pub const FILE_EDIT: &str = include_str!("../prompts/file_edit.md");
pub const PLAN: &str = include_str!("../prompts/plan.md");
pub const SUBSESSIONS: &str = include_str!("../prompts/subsessions.md");
pub const AUTOMATION: &str = include_str!("../prompts/automation.md");
pub const CHANNELS: &str = include_str!("../prompts/channels.md");

pub fn tools() -> String {
    format!(
        "{}\n\n{}\n\n{}\n\n{}\n\n{}",
        COMMON.trim(),
        TERMINAL.trim(),
        READ_FILE.trim(),
        FILE_EDIT.trim(),
        PLAN.trim()
    )
}

pub fn combined() -> String {
    format!(
        "{}\n\n{}\n\n{}\n\n{}",
        tools(),
        SUBSESSIONS.trim(),
        AUTOMATION.trim(),
        CHANNELS.trim()
    )
}
