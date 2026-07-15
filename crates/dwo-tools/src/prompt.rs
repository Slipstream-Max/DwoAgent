//! Static system-prompt fragments for tool usage guidance.

pub const COMMON: &str = include_str!("../prompts/common.md");
pub const TERMINAL: &str = include_str!("../prompts/terminal.md");
pub const FILE_EDIT: &str = include_str!("../prompts/file_edit.md");

pub fn combined() -> String {
    format!(
        "{}\n\n{}\n\n{}",
        COMMON.trim(),
        TERMINAL.trim(),
        FILE_EDIT.trim()
    )
}
