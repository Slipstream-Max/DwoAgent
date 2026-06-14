//! Policy mode and tool-call permission decision helpers.

use anyhow::{Result, bail};
use serde_json::{Map, Value};

use super::constants::{
    MODE_CONFIRM, MODE_FULL_ACCESS, MODE_WATCH, PERMISSION_ALLOW_ONCE, PERMISSION_CANCELLED,
    PERMISSION_REJECT_ONCE,
};
use crate::config::policy::ToolPolicyConfig;
use crate::utils::policy::{parse_policy_mode, policy_mode_rank};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolPolicyAction {
    Allow,
    Confirm,
    Reject(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOutcome {
    pub allowed: bool,
}

pub fn resolve_tool_policy(
    mode_id: &str,
    tool_name: &str,
    tool_args: &Map<String, Value>,
    policy: &ToolPolicyConfig,
) -> Result<ToolPolicyAction> {
    let mode = parse_policy_mode(mode_id)?;
    let name = tool_name.trim();

    if name == "file_edit" {
        return Ok(match mode.as_str() {
            MODE_FULL_ACCESS => ToolPolicyAction::Allow,
            MODE_CONFIRM => ToolPolicyAction::Confirm,
            MODE_WATCH => reject("file_edit is not allowed in watch mode."),
            other => bail!("Invalid mode_id: {other}"),
        });
    }

    if name == "terminal_exec" {
        return resolve_terminal_exec(&mode, tool_args, policy);
    }

    if name == "wait" || is_terminal_read_tool(name) || is_subagent_tool(name) {
        return Ok(ToolPolicyAction::Allow);
    }

    if name == "terminal_kill" {
        return Ok(match mode.as_str() {
            MODE_FULL_ACCESS => ToolPolicyAction::Allow,
            MODE_CONFIRM => ToolPolicyAction::Confirm,
            MODE_WATCH => reject("terminal_kill is not allowed in watch mode."),
            other => bail!("Invalid mode_id: {other}"),
        });
    }

    Ok(match mode.as_str() {
        MODE_FULL_ACCESS => ToolPolicyAction::Allow,
        MODE_CONFIRM => ToolPolicyAction::Confirm,
        MODE_WATCH => reject("Tool is not allowed in watch mode."),
        other => bail!("Invalid mode_id: {other}"),
    })
}

pub fn resolve_permission_decision(decision: &str) -> Result<PermissionOutcome> {
    let allowed = match decision {
        PERMISSION_ALLOW_ONCE => true,
        PERMISSION_REJECT_ONCE | PERMISSION_CANCELLED => false,
        other => bail!("Invalid permission decision: {other}"),
    };
    Ok(PermissionOutcome { allowed })
}

pub fn clamp_subagent_policy(parent_mode: &str, requested_mode: Option<&str>) -> Result<String> {
    let parent = parse_policy_mode(parent_mode)?;
    let requested = match requested_mode {
        Some(value) => parse_policy_mode(value)?,
        None => parent.clone(),
    };

    let parent_rank = policy_mode_rank(&parent)?;
    let requested_rank = policy_mode_rank(&requested)?;
    if requested_rank <= parent_rank {
        Ok(requested)
    } else {
        Ok(parent)
    }
}

fn resolve_terminal_exec(
    mode: &str,
    tool_args: &Map<String, Value>,
    policy: &ToolPolicyConfig,
) -> Result<ToolPolicyAction> {
    let command = tool_args
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if policy.terminal.command_is_denied(command) {
        return Ok(reject("Terminal command denied by policy."));
    }

    let simple = is_simple_terminal_command(command);
    let env_empty = terminal_env_is_empty(tool_args);
    Ok(match mode {
        MODE_FULL_ACCESS => ToolPolicyAction::Allow,
        MODE_CONFIRM => {
            if simple && env_empty && policy.terminal.command_is_allowed(command) {
                ToolPolicyAction::Allow
            } else {
                ToolPolicyAction::Confirm
            }
        }
        MODE_WATCH => {
            if simple && env_empty && policy.terminal.command_is_watch_allowed(command) {
                ToolPolicyAction::Allow
            } else {
                reject("Terminal command is not allowed in watch mode.")
            }
        }
        other => bail!("Invalid mode_id: {other}"),
    })
}

fn reject(reason: &str) -> ToolPolicyAction {
    ToolPolicyAction::Reject(reason.to_string())
}

fn is_terminal_read_tool(name: &str) -> bool {
    matches!(name, "list_terminals" | "terminal_checkout")
}

fn is_subagent_tool(name: &str) -> bool {
    matches!(
        name,
        "spawn_subagent"
            | "list_subagents"
            | "checkout_subagent"
            | "send_subagent"
            | "close_subagent"
    )
}

fn terminal_env_is_empty(tool_args: &Map<String, Value>) -> bool {
    match tool_args.get("env") {
        None | Some(Value::Null) => true,
        Some(Value::Object(map)) => map.is_empty(),
        Some(_) => false,
    }
}

fn is_simple_terminal_command(command: &str) -> bool {
    let text = command.trim();
    if text.is_empty() {
        return false;
    }

    let mut chars = text.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(ch) = chars.next() {
        if in_single {
            if ch == '\'' {
                in_single = false;
            }
            continue;
        }
        if in_double {
            if ch == '"' {
                in_double = false;
                continue;
            }
            if ch == '$' && chars.peek().copied() == Some('(') {
                return false;
            }
            continue;
        }

        match ch {
            '\'' => in_single = true,
            '"' => in_double = true,
            '\n' | '\r' | ';' | '|' | '&' | '>' | '<' | '`' => return false,
            '$' if chars.peek().copied() == Some('(') => return false,
            _ => {}
        }
    }

    !in_single && !in_double
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::policy::ToolPolicyConfig;
    use serde_json::json;

    fn policy() -> ToolPolicyConfig {
        ToolPolicyConfig::from_value(json!({
            "terminal": {
                "deny": [
                    {"regex": "(?i)^\\s*git\\s+reset\\s+--hard\\b"},
                    {"regex": "(?i)Remove-Item\\b.*\\b-Recurse\\b"}
                ],
                "allow": [
                    {"exact": "git status"},
                    {"prefix": "cargo check"},
                    {"prefix": "rg "}
                ],
                "watch_allow": [
                    {"exact": "git status"},
                    {"prefix": "git diff"},
                    {"prefix": "rg "},
                    {"prefix": "Get-Content "}
                ]
            }
        }))
        .unwrap()
    }

    #[test]
    fn full_access_terminal_uses_deny_only() {
        let policy = policy();
        assert_eq!(
            resolve_tool_policy(
                MODE_FULL_ACCESS,
                "terminal_exec",
                json!({"command": "cargo build"}).as_object().unwrap(),
                &policy,
            )
            .unwrap(),
            ToolPolicyAction::Allow
        );
        assert!(matches!(
            resolve_tool_policy(
                MODE_FULL_ACCESS,
                "terminal_exec",
                json!({"command": "git reset --hard HEAD"})
                    .as_object()
                    .unwrap(),
                &policy,
            )
            .unwrap(),
            ToolPolicyAction::Reject(_)
        ));
    }

    #[test]
    fn confirm_terminal_allows_allowlist_rejects_deny_and_confirms_otherwise() {
        let policy = policy();
        assert_eq!(
            resolve_tool_policy(
                MODE_CONFIRM,
                "terminal_exec",
                json!({"command": "rg policy src"}).as_object().unwrap(),
                &policy,
            )
            .unwrap(),
            ToolPolicyAction::Allow
        );
        assert!(matches!(
            resolve_tool_policy(
                MODE_CONFIRM,
                "terminal_exec",
                json!({"command": "git reset --hard"}).as_object().unwrap(),
                &policy,
            )
            .unwrap(),
            ToolPolicyAction::Reject(_)
        ));
        assert_eq!(
            resolve_tool_policy(
                MODE_CONFIRM,
                "terminal_exec",
                json!({"command": "cargo run"}).as_object().unwrap(),
                &policy,
            )
            .unwrap(),
            ToolPolicyAction::Confirm
        );
    }

    #[test]
    fn allowlist_does_not_autopass_multi_command_or_env_override() {
        let policy = policy();
        assert_eq!(
            resolve_tool_policy(
                MODE_CONFIRM,
                "terminal_exec",
                json!({"command": "git status; cargo run"})
                    .as_object()
                    .unwrap(),
                &policy,
            )
            .unwrap(),
            ToolPolicyAction::Confirm
        );
        assert_eq!(
            resolve_tool_policy(
                MODE_CONFIRM,
                "terminal_exec",
                json!({"command": "git status", "env": {"PATH": "custom"}})
                    .as_object()
                    .unwrap(),
                &policy,
            )
            .unwrap(),
            ToolPolicyAction::Confirm
        );
    }

    #[test]
    fn watch_terminal_only_allows_simple_watch_commands() {
        let policy = policy();
        assert_eq!(
            resolve_tool_policy(
                MODE_WATCH,
                "terminal_exec",
                json!({"command": "Get-Content Cargo.toml"})
                    .as_object()
                    .unwrap(),
                &policy,
            )
            .unwrap(),
            ToolPolicyAction::Allow
        );
        assert!(matches!(
            resolve_tool_policy(
                MODE_WATCH,
                "terminal_exec",
                json!({"command": "Get-Content Cargo.toml | Set-Content out.txt"})
                    .as_object()
                    .unwrap(),
                &policy,
            )
            .unwrap(),
            ToolPolicyAction::Reject(_)
        ));
    }

    #[test]
    fn file_edit_policy_is_fixed_by_mode() {
        let policy = policy();
        let args_value = json!({"patch": "*** Begin Patch\n*** End Patch"});
        let args = args_value.as_object().unwrap();
        assert_eq!(
            resolve_tool_policy(MODE_FULL_ACCESS, "file_edit", args, &policy).unwrap(),
            ToolPolicyAction::Allow
        );
        assert_eq!(
            resolve_tool_policy(MODE_CONFIRM, "file_edit", args, &policy).unwrap(),
            ToolPolicyAction::Confirm
        );
        assert!(matches!(
            resolve_tool_policy(MODE_WATCH, "file_edit", args, &policy).unwrap(),
            ToolPolicyAction::Reject(_)
        ));
    }

    #[test]
    fn subagent_policy_is_clamped_at_creation_to_parent_mode() {
        assert_eq!(
            clamp_subagent_policy(MODE_CONFIRM, Some(MODE_FULL_ACCESS)).unwrap(),
            MODE_CONFIRM
        );
        assert_eq!(
            clamp_subagent_policy(MODE_CONFIRM, Some(MODE_WATCH)).unwrap(),
            MODE_WATCH
        );
        assert_eq!(
            clamp_subagent_policy(MODE_CONFIRM, None).unwrap(),
            MODE_CONFIRM
        );

        assert_eq!(
            clamp_subagent_policy(MODE_WATCH, Some(MODE_FULL_ACCESS)).unwrap(),
            MODE_WATCH
        );
        assert_eq!(
            clamp_subagent_policy(MODE_WATCH, Some(MODE_CONFIRM)).unwrap(),
            MODE_WATCH
        );

        assert_eq!(
            clamp_subagent_policy(MODE_FULL_ACCESS, Some(MODE_CONFIRM)).unwrap(),
            MODE_CONFIRM
        );
        assert_eq!(
            clamp_subagent_policy(MODE_FULL_ACCESS, Some(MODE_WATCH)).unwrap(),
            MODE_WATCH
        );
        assert_eq!(
            clamp_subagent_policy(MODE_FULL_ACCESS, None).unwrap(),
            MODE_FULL_ACCESS
        );
    }
}
