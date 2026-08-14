use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::ToolIntent;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    FullAccess,
    Confirm,
    Watch,
}

impl SessionMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "full_access" | "full-access" | "fullaccess" => Ok(Self::FullAccess),
            "confirm" => Ok(Self::Confirm),
            "watch" | "watchmode" | "watch_mode" => Ok(Self::Watch),
            other => Err(format!("Unknown policy mode: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authorization {
    Allow,
    Confirm,
    Deny(String),
}

#[derive(Debug, Clone)]
pub enum CommandRule {
    Exact(String),
    Prefix(String),
    Regex { source: String, compiled: Regex },
}

impl CommandRule {
    pub fn exact(value: impl Into<String>) -> Self {
        Self::Exact(normalize(&value.into()))
    }

    pub fn prefix(value: impl Into<String>) -> Self {
        Self::Prefix(normalize(&value.into()))
    }

    pub fn regex(value: impl Into<String>) -> Result<Self, regex::Error> {
        let source = value.into();
        Ok(Self::Regex {
            compiled: Regex::new(&source)?,
            source,
        })
    }

    fn matches(&self, command: &str) -> bool {
        let normalized = normalize(command);
        match self {
            Self::Exact(value) => normalized.eq_ignore_ascii_case(value),
            Self::Prefix(value) => {
                normalized.eq_ignore_ascii_case(value)
                    || normalized
                        .to_ascii_lowercase()
                        .strip_prefix(&value.to_ascii_lowercase())
                        .is_some_and(|rest| rest.starts_with(char::is_whitespace))
            }
            Self::Regex { compiled, .. } => compiled.is_match(command.trim()),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PolicyConfig {
    pub terminal_deny: Vec<CommandRule>,
    pub terminal_allow: Vec<CommandRule>,
    pub watch_allow: Vec<CommandRule>,
}

#[derive(Debug, Clone)]
pub struct ToolPolicyEngine {
    config: PolicyConfig,
}

impl ToolPolicyEngine {
    pub fn new(config: PolicyConfig) -> Self {
        Self { config }
    }

    pub fn authorize(&self, mode: SessionMode, intent: &ToolIntent) -> Authorization {
        match intent {
            ToolIntent::TerminalRun { command } => self.authorize_command(mode, command),
            ToolIntent::TerminalInput { data, .. } if data.is_empty() => Authorization::Allow,
            ToolIntent::TerminalInput { data, .. } => self.authorize_command(mode, data),
            ToolIntent::TerminalKill { .. } => Authorization::Allow,
            ToolIntent::ReadFile => Authorization::Allow,
            ToolIntent::FileEdit => match mode {
                SessionMode::FullAccess => Authorization::Allow,
                SessionMode::Confirm => Authorization::Confirm,
                SessionMode::Watch => {
                    Authorization::Deny("file_edit is unavailable in watch mode.".to_string())
                }
            },
            ToolIntent::Handoff => Authorization::Allow,
            ToolIntent::Plan => Authorization::Allow,
        }
    }

    fn authorize_command(&self, mode: SessionMode, command: &str) -> Authorization {
        if self
            .config
            .terminal_deny
            .iter()
            .any(|rule| rule.matches(command))
        {
            return Authorization::Deny("Command matches a terminal deny rule.".to_string());
        }

        match mode {
            SessionMode::FullAccess => Authorization::Allow,
            SessionMode::Confirm => {
                if self
                    .config
                    .terminal_allow
                    .iter()
                    .any(|rule| rule.matches(command))
                    || is_builtin_read_only_command(command)
                {
                    Authorization::Allow
                } else {
                    Authorization::Confirm
                }
            }
            SessionMode::Watch => {
                if self
                    .config
                    .watch_allow
                    .iter()
                    .any(|rule| rule.matches(command))
                    || is_builtin_read_only_command(command)
                {
                    Authorization::Allow
                } else {
                    Authorization::Deny(
                        "Only simple read-only terminal commands are available in watch mode."
                            .to_string(),
                    )
                }
            }
        }
    }
}

impl Default for ToolPolicyEngine {
    fn default() -> Self {
        Self::new(PolicyConfig::default())
    }
}

fn is_builtin_read_only_command(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() || contains_shell_control_syntax(command) {
        return false;
    }
    let Some(program) = first_word(command) else {
        return false;
    };
    let program = program
        .trim_matches(|character| character == '"' || character == '\'')
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();

    matches!(
        program.as_str(),
        "pwd"
            | "ls"
            | "dir"
            | "cat"
            | "head"
            | "tail"
            | "rg"
            | "cd"
            | "get-location"
            | "get-childitem"
            | "get-child-item"
            | "get-content"
            | "select-string"
    ) && !contains_dangerous_read_flag(command)
}

fn contains_shell_control_syntax(command: &str) -> bool {
    let mut single_quoted = false;
    let mut double_quoted = false;
    let mut escaped = false;
    let chars: Vec<char> = command.chars().collect();
    for (index, character) in chars.iter().copied().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && !single_quoted {
            escaped = true;
            continue;
        }
        if character == '\'' && !double_quoted {
            single_quoted = !single_quoted;
            continue;
        }
        if character == '"' && !single_quoted {
            double_quoted = !double_quoted;
            continue;
        }
        if single_quoted || double_quoted {
            continue;
        }
        if matches!(
            character,
            '|' | ';' | '>' | '<' | '`' | '\n' | '\r' | '(' | ')' | '{' | '}'
        ) {
            return true;
        }
        if matches!(character, '&') || (character == '$' && chars.get(index + 1) == Some(&'(')) {
            return true;
        }
    }
    false
}

fn contains_dangerous_read_flag(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    ["--exec", "--pre", "--pre-glob", "-replace", "-raw |"]
        .iter()
        .any(|flag| lower.contains(flag))
}

fn first_word(command: &str) -> Option<&str> {
    command.split_whitespace().next()
}

fn normalize(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::TerminalId;

    fn run(command: &str) -> ToolIntent {
        ToolIntent::TerminalRun {
            command: command.to_string(),
        }
    }

    #[test]
    fn deny_rules_take_precedence_in_every_mode() {
        let engine = ToolPolicyEngine::new(PolicyConfig {
            terminal_deny: vec![CommandRule::prefix("git reset --hard")],
            ..PolicyConfig::default()
        });
        for mode in [
            SessionMode::FullAccess,
            SessionMode::Confirm,
            SessionMode::Watch,
        ] {
            assert!(matches!(
                engine.authorize(mode, &run("git reset --hard HEAD")),
                Authorization::Deny(_)
            ));
        }
    }

    #[test]
    fn watch_allows_simple_reads_but_rejects_composition() {
        let engine = ToolPolicyEngine::default();
        for command in [
            "rg foo src",
            "Get-Content Cargo.toml",
            "ls -Force",
            "cd crates",
        ] {
            assert_eq!(
                engine.authorize(SessionMode::Watch, &run(command)),
                Authorization::Allow
            );
        }
        for command in ["cat a | sh", "rg foo > out", "ls; rm a", "echo $(whoami)"] {
            assert!(matches!(
                engine.authorize(SessionMode::Watch, &run(command)),
                Authorization::Deny(_)
            ));
        }
    }

    #[test]
    fn every_terminal_action_has_a_policy_result() {
        let engine = ToolPolicyEngine::default();
        let id = TerminalId::parse("term-1").unwrap();
        assert_eq!(
            engine.authorize(
                SessionMode::Watch,
                &ToolIntent::TerminalInput {
                    terminal_id: id.clone(),
                    data: String::new(),
                }
            ),
            Authorization::Allow
        );
        assert_eq!(
            engine.authorize(
                SessionMode::Watch,
                &ToolIntent::TerminalKill { terminal_id: id }
            ),
            Authorization::Allow
        );
    }
}
