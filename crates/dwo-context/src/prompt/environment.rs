use std::path::Path;

use chrono::Local;
use serde::{Deserialize, Serialize};

use super::xml_block;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentSnapshot {
    pub cwd: String,
    pub shell: String,
    pub current_date: String,
    pub timezone: String,
    pub platform: String,
}

impl EnvironmentSnapshot {
    pub fn capture(cwd: &Path) -> Self {
        Self {
            cwd: cwd.display().to_string(),
            shell: if cfg!(windows) { "powershell" } else { "sh" }.to_string(),
            current_date: Local::now().date_naive().format("%Y-%m-%d").to_string(),
            timezone: Local::now().offset().to_string(),
            platform: std::env::consts::OS.to_string(),
        }
    }

    pub(crate) fn render(&self) -> String {
        xml_block(
            "environment",
            &format!(
                "cwd: {}\nshell: {}\nplatform: {}\ncurrent_date: {}\ntimezone: {}",
                self.cwd, self.shell, self.platform, self.current_date, self.timezone
            ),
        )
    }
}
