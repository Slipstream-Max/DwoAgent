//! Build the `<env_context>` context block.

use chrono::Local;

use super::xml::{block, tag};

pub fn build_env_context(cwd: &str) -> String {
    let body = [
        tag("cwd", cwd),
        tag("shell", detect_shell()),
        tag("current_date", &local_current_date()),
        tag("timezone", &local_timezone()),
    ]
    .join("\n");
    block("env_context", &body)
}

/// Return the shell used by the agent terminal executor.
pub fn detect_shell() -> &'static str {
    if cfg!(windows) { "powershell" } else { "sh" }
}

fn local_current_date() -> String {
    Local::now().date_naive().format("%Y-%m-%d").to_string()
}

fn local_timezone() -> String {
    // `chrono::Local` doesn't expose an IANA zone name portably, so mirror
    // Python's stringified `tzinfo` which ultimately resolves to the UTC
    // offset when zoneinfo isn't loaded.
    let offset = Local::now().offset().to_string();
    if offset.is_empty() {
        "local".to_string()
    } else {
        offset
    }
}
