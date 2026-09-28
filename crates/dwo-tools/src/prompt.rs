//! Static system-prompt fragments for tool usage guidance. Daemon feature
//! prompts (subsessions, automation, channels) live in `dwo-agent-service`.

pub const COMMON: &str = include_str!("../prompts/common.md");
pub const TERMINAL: &str = include_str!("../prompts/terminal.md");
pub const READ_FILE: &str = include_str!("../prompts/read_file.md");
pub const FILE_EDIT: &str = include_str!("../prompts/file_edit.md");
pub const TODO: &str = include_str!("../prompts/todo.md");

pub fn tools() -> String {
    format!(
        "{}\n\n{}\n\n{}\n\n{}\n\n{}",
        COMMON.trim(),
        TERMINAL.trim(),
        READ_FILE.trim(),
        FILE_EDIT.trim(),
        TODO.trim()
    )
}
