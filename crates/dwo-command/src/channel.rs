use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};

use super::prompt::COMMAND_DESCRIPTIONS as PROMPT_DIRECTIVE_DESCRIPTIONS;

#[derive(Debug, Parser)]
#[command(
    name = "channel",
    about = "These commands are supported:",
    disable_help_flag = true,
    disable_help_subcommand = true,
    disable_version_flag = true
)]
struct ChannelCommandLine {
    #[command(subcommand)]
    command: ChannelCommand,
}

#[derive(Debug, Subcommand)]
pub enum ChannelCommand {
    #[command(about = "Display this command list.")]
    Help,
    #[command(about = "List Inbox or project sessions, with optional page number.")]
    List {
        #[arg(value_name = "PROJECT_OR_PAGE")]
        scope: Option<String>,
        #[arg(value_name = "PAGE")]
        page: Option<usize>,
    },
    #[command(about = "Create and select a session.")]
    New {
        #[arg(value_name = "PROJECT")]
        project: Option<String>,
        #[arg(value_name = "SECTION", requires = "project")]
        section: Option<String>,
    },
    #[command(about = "Copy the selected session into a new session.")]
    Fork,
    #[command(about = "Select by number or short ID and replay recent turns; bare /use lists sessions.")]
    Use {
        #[arg(value_name = "SESSION")]
        session: Option<String>,
        #[arg(value_name = "NUMBER")]
        number: Option<usize>,
    },
    #[command(about = "Show the selected session state.")]
    Status,
    #[command(about = "Cancel the active turn.")]
    Cancel,
    #[command(about = "Compact the selected session context.")]
    Compact,
    #[command(about = "Continue the selected session when it is idle.")]
    Resume,
    #[command(about = "List available models or change the selected session model.")]
    Model {
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
    #[command(about = "List supported reasoning efforts or change the current effort.")]
    Think {
        #[arg(value_name = "LEVEL|off")]
        level: Option<String>,
    },
    #[command(about = "List skills; use /skill <name> [prompt] to run one.")]
    Skill,
    #[command(about = "Show or change the tool permission policy.")]
    Policy {
        #[arg(value_name = "full_access|confirm|watch")]
        mode: Option<String>,
    },
    #[command(about = "Allow the current or specified permission request.")]
    Allow {
        #[arg(value_name = "ID")]
        id: Option<String>,
    },
    #[command(about = "Deny the current or specified permission request.")]
    Deny {
        #[arg(value_name = "ID")]
        id: Option<String>,
    },
}

pub fn parse_command(input: &str) -> Result<ChannelCommand> {
    let mut tokens = split_command_line(input)?;
    let command = tokens.first_mut().context("command is required")?;
    let normalized = command.strip_prefix('/').unwrap_or(command.as_str());
    *command = normalized
        .split_once('@')
        .map(|(name, _)| name)
        .unwrap_or(normalized)
        .to_string();
    ChannelCommandLine::try_parse_from(std::iter::once("channel".to_string()).chain(tokens))
        .map(|line| line.command)
        .map_err(|error| anyhow::Error::msg(render_command_error(error)))
}

pub fn render_command_help() -> String {
    let mut command = ChannelCommandLine::command();
    command.build();
    let heading = command
        .get_about()
        .map(ToString::to_string)
        .unwrap_or_else(|| "Commands:".to_string());
    let commands = command
        .get_subcommands()
        .map(|command| {
            let description = command
                .get_about()
                .map(ToString::to_string)
                .unwrap_or_default();
            (command.get_name().to_string(), description)
        })
        .chain(
            PROMPT_DIRECTIVE_DESCRIPTIONS
                .into_iter()
                .map(|(name, description)| (name.to_string(), description.to_string())),
        )
        .map(|(name, description)| format!("/{name} - {description}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{heading}\n\n{commands}")
}

pub fn command_descriptions() -> Vec<(String, String)> {
    let mut command = ChannelCommandLine::command();
    command.build();
    command
        .get_subcommands()
        .map(|command| {
            (
                command.get_name().to_string(),
                command
                    .get_about()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            )
        })
        .chain(
            PROMPT_DIRECTIVE_DESCRIPTIONS
                .into_iter()
                .map(|(name, description)| (name.to_string(), description.to_string())),
        )
        .collect()
}

fn split_command_line(input: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    for character in input.chars() {
        if let Some(expected) = quote {
            if character == expected {
                quote = None;
            } else {
                current.push(character);
            }
            started = true;
            continue;
        }
        match character {
            '\'' | '"' => {
                quote = Some(character);
                started = true;
            }
            character if character.is_whitespace() => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            _ => {
                current.push(character);
                started = true;
            }
        }
    }
    if quote.is_some() {
        bail!("unterminated quote in command");
    }
    if started {
        tokens.push(current);
    }
    Ok(tokens)
}

fn render_command_error(error: clap::Error) -> String {
    let rendered = error
        .to_string()
        .replace("Usage: channel ", "Usage: /")
        .replace("Usage: channel", "Usage: /help")
        .trim()
        .to_string();
    rendered
        .strip_prefix("error: ")
        .map(ToString::to_string)
        .unwrap_or(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_accepts_project_and_section_names() {
        assert!(
            matches!(parse_command(r#"/new "My project" "Development""#).unwrap(),
            ChannelCommand::New { project: Some(p), section: Some(s) } if p == "My project" && s == "Development")
        );
        assert!(matches!(
            parse_command("/new").unwrap(),
            ChannelCommand::New {
                project: None,
                section: None
            }
        ));
        assert!(parse_command("/new a b c").is_err());
    }

    #[test]
    fn command_help_lists_every_command() {
        let help = render_command_help();
        assert!(help.starts_with("These commands are supported:\n\n"));
        assert!(help.contains("/help - Display this command list."));
        assert!(help.contains("/compact - Compact the selected session context."));
        assert!(help.contains("/skill - List skills; use /skill <name> [prompt] to run one."));
        assert!(help.contains(
            "/think - List supported reasoning efforts or change the current effort."
        ));
        assert!(!help.contains("/reasoning"));
        assert!(!help.contains("/del"));
    }

    #[test]
    fn command_parse_errors_use_slash_command_usage() {
        let error = parse_command("/model a b").unwrap_err().to_string();
        assert!(error.contains("Usage: /model [NAME]"), "{error}");
    }

    #[test]
    fn telegram_bot_suffix_is_ignored() {
        assert!(matches!(
            parse_command("/status@dwoagent_bot").unwrap(),
            ChannelCommand::Status
        ));
    }

    #[test]
    fn fork_command_accepts_no_arguments() {
        assert!(matches!(
            parse_command("/fork").unwrap(),
            ChannelCommand::Fork
        ));
        assert!(parse_command("/fork now").is_err());
    }

    #[test]
    fn permission_commands_default_to_the_current_request() {
        assert!(matches!(
            parse_command("/allow").unwrap(),
            ChannelCommand::Allow { id: None }
        ));
        assert!(matches!(
            parse_command("/deny request-7").unwrap(),
            ChannelCommand::Deny {
                id: Some(ref id)
            } if id == "request-7"
        ));
    }

    #[test]
    fn session_commands_accept_short_references() {
        assert!(matches!(
            parse_command("/use 2").unwrap(),
            ChannelCommand::Use { session: Some(session), number: None } if session == "2"
        ));
        assert!(matches!(
            parse_command("/use abc12345").unwrap(),
            ChannelCommand::Use { session: Some(session), number: None } if session == "abc12345"
        ));
        assert!(matches!(
            parse_command("/use").unwrap(),
            ChannelCommand::Use {
                session: None,
                number: None
            }
        ));
        assert!(parse_command("/del abc12345").is_err());
        assert!(parse_command("/archive abc12345").is_err());
    }

    #[test]
    fn channel_scopes_and_optional_settings_parse() {
        assert!(matches!(
            parse_command("/model").unwrap(),
            ChannelCommand::Model { name: None }
        ));
        assert!(matches!(
            parse_command("/think").unwrap(),
            ChannelCommand::Think { level: None }
        ));
        assert!(matches!(
            parse_command("/think high").unwrap(),
            ChannelCommand::Think { level: Some(level) } if level == "high"
        ));
        assert!(matches!(
            parse_command("/skill").unwrap(),
            ChannelCommand::Skill
        ));
        assert!(
            matches!(parse_command("/list 2").unwrap(), ChannelCommand::List { scope: Some(s), page: None } if s == "2")
        );
        assert!(
            matches!(parse_command(r#"/list "My project" 3"#).unwrap(), ChannelCommand::List { scope: Some(s), page: Some(3) } if s == "My project")
        );
        assert!(
            matches!(parse_command(r#"/use "My project" 21"#).unwrap(), ChannelCommand::Use { session: Some(session), number: Some(21) } if session == "My project")
        );
        assert!(!render_command_help().contains("/plan"));
    }

    #[test]
    fn platform_command_menu_lists_channel_commands() {
        assert!(command_descriptions().contains(&(
            "new".to_string(),
            "Create and select a session.".to_string()
        )));
        assert!(command_descriptions().contains(&(
            "skill".to_string(),
            "List skills; use /skill <name> [prompt] to run one.".to_string()
        )));
        assert!(command_descriptions().contains(&(
            "think".to_string(),
            "List supported reasoning efforts or change the current effort.".to_string()
        )));
    }
}
