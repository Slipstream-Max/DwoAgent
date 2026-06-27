//! Agent tool policy configuration loaded from the `policy` section in `agent.yaml`.

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct ToolPolicyConfig {
    pub terminal: TerminalPolicyConfig,
}

#[derive(Debug, Clone, Default)]
pub struct TerminalPolicyConfig {
    pub deny: Vec<CommandRule>,
    pub allow: Vec<CommandRule>,
    pub watch_allow: Vec<CommandRule>,
}

#[derive(Debug, Clone)]
pub enum CommandRule {
    Exact(String),
    Prefix(String),
    Regex { pattern: String, compiled: Regex },
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct RawToolPolicyConfig {
    terminal: RawTerminalPolicyConfig,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct RawTerminalPolicyConfig {
    deny: Vec<RawCommandRule>,
    allow: Vec<RawCommandRule>,
    watch_allow: Vec<RawCommandRule>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawCommandRule {
    ExactString(String),
    Object(RawCommandRuleObject),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommandRuleObject {
    exact: Option<String>,
    prefix: Option<String>,
    regex: Option<String>,
}

impl ToolPolicyConfig {
    pub fn from_value(value: Value) -> Result<Self> {
        let raw: RawToolPolicyConfig =
            serde_json::from_value(value).context("parse agent.yaml policy section")?;
        Ok(Self {
            terminal: TerminalPolicyConfig {
                deny: compile_rules(raw.terminal.deny, "terminal.deny")?,
                allow: compile_rules(raw.terminal.allow, "terminal.allow")?,
                watch_allow: compile_rules(raw.terminal.watch_allow, "terminal.watch_allow")?,
            },
        })
    }
}

impl TerminalPolicyConfig {
    pub fn command_is_denied(&self, command: &str) -> bool {
        command_matches_any(command, &self.deny)
    }

    pub fn command_is_allowed(&self, command: &str) -> bool {
        command_matches_any(command, &self.allow)
    }

    pub fn command_is_watch_allowed(&self, command: &str) -> bool {
        command_matches_any(command, &self.watch_allow)
    }
}

fn compile_rules(rules: Vec<RawCommandRule>, field: &str) -> Result<Vec<CommandRule>> {
    rules
        .into_iter()
        .enumerate()
        .map(|(index, rule)| compile_rule(rule, &format!("{field}[{index}]")))
        .collect()
}

fn compile_rule(rule: RawCommandRule, label: &str) -> Result<CommandRule> {
    match rule {
        RawCommandRule::ExactString(value) => {
            let value = normalize_literal_rule(&value, label)?;
            Ok(CommandRule::Exact(value))
        }
        RawCommandRule::Object(object) => {
            let mut selected = Vec::new();
            if let Some(value) = object.exact {
                selected.push(("exact", value));
            }
            if let Some(value) = object.prefix {
                selected.push(("prefix", value));
            }
            if let Some(value) = object.regex {
                selected.push(("regex", value));
            }
            if selected.len() != 1 {
                bail!("{label} must specify exactly one of exact, prefix, or regex");
            }
            let (kind, value) = selected.pop().unwrap();
            match kind {
                "exact" => Ok(CommandRule::Exact(normalize_literal_rule(&value, label)?)),
                "prefix" => Ok(CommandRule::Prefix(normalize_literal_rule(&value, label)?)),
                "regex" => {
                    let pattern = value.trim();
                    if pattern.is_empty() {
                        bail!("{label} regex must not be empty");
                    }
                    let compiled = Regex::new(pattern)
                        .with_context(|| format!("compile regex rule {label}: {pattern}"))?;
                    Ok(CommandRule::Regex {
                        pattern: pattern.to_string(),
                        compiled,
                    })
                }
                _ => unreachable!(),
            }
        }
    }
}

fn normalize_literal_rule(value: &str, label: &str) -> Result<String> {
    let normalized = normalize_command_like_text(value);
    if normalized.is_empty() {
        bail!("{label} must not be empty");
    }
    Ok(normalized)
}

fn command_matches_any(command: &str, rules: &[CommandRule]) -> bool {
    let normalized = normalize_command_like_text(command);
    rules.iter().any(|rule| match rule {
        CommandRule::Exact(value) => normalized.eq_ignore_ascii_case(value),
        CommandRule::Prefix(value) => command_has_prefix(&normalized, value),
        CommandRule::Regex { compiled, .. } => compiled.is_match(command.trim()),
    })
}

fn command_has_prefix(command: &str, prefix: &str) -> bool {
    if command.eq_ignore_ascii_case(prefix) {
        return true;
    }
    let command_lower = command.to_ascii_lowercase();
    let prefix_lower = prefix.to_ascii_lowercase();
    command_lower
        .strip_prefix(&prefix_lower)
        .is_some_and(|rest| rest.starts_with(' '))
}

fn normalize_command_like_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn policy_accepts_exact_prefix_and_regex_rules() {
        let policy = ToolPolicyConfig::from_value(json!({
            "terminal": {
                "deny": [{"regex": "(?i)^git reset --hard\\b"}],
                "allow": [{"exact": "git status"}, {"prefix": "rg "}],
                "watchAllow": [{"prefix": "git diff"}]
            }
        }))
        .unwrap();

        assert!(policy.terminal.command_is_denied("Git Reset --Hard"));
        assert!(policy.terminal.command_is_allowed("git status"));
        assert!(policy.terminal.command_is_allowed("rg policy src"));
        assert!(policy.terminal.command_is_watch_allowed("git diff"));
        assert!(
            policy
                .terminal
                .command_is_watch_allowed("git diff -- Cargo.toml")
        );
    }

    #[test]
    fn policy_rejects_ambiguous_rule_objects() {
        let err = ToolPolicyConfig::from_value(json!({
            "terminal": {
                "allow": [{"exact": "git status", "prefix": "git"}]
            }
        }))
        .unwrap_err();

        assert!(err.to_string().contains("exactly one"));
    }
}
