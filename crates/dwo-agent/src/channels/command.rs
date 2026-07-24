use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};

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
pub(crate) enum ChannelCommand {
    #[command(about = "Display this command list.")]
    Help,
    #[command(about = "List sessions and show the selected session.")]
    List,
    #[command(about = "Create and select a session.")]
    New {
        #[arg(value_name = "NAME", num_args = 0..)]
        name: Vec<String>,
        #[arg(long, value_name = "PATH")]
        cwd: Option<PathBuf>,
    },
    #[command(about = "Select a session and replay its recent turns.")]
    Use {
        #[arg(value_name = "SESSION")]
        session: String,
    },
    #[command(about = "Show the selected session state.")]
    Status,
    #[command(about = "Delete a session.")]
    Del {
        #[arg(value_name = "SESSION")]
        session: String,
    },
    #[command(about = "Cancel the active turn.")]
    Cancel,
    #[command(about = "Change the selected session model.")]
    Model {
        #[arg(value_name = "NAME")]
        name: String,
    },
    #[command(about = "Change reasoning effort, or disable reasoning.")]
    Reasoning {
        #[arg(value_name = "LEVEL|off")]
        level: String,
    },
    #[command(about = "Show or change the tool permission policy.")]
    Policy {
        #[arg(value_name = "full_access|confirm|watch")]
        mode: Option<String>,
    },
    #[command(about = "Allow a pending permission request.")]
    Allow {
        #[arg(value_name = "ID")]
        id: String,
    },
    #[command(about = "Deny a pending permission request.")]
    Deny {
        #[arg(value_name = "ID")]
        id: String,
    },
}

pub(crate) fn parse_command(input: &str) -> Result<ChannelCommand> {
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

pub(crate) fn render_command_help() -> String {
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
            format!("/{} - {description}", command.get_name())
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{heading}\n\n{commands}")
}

pub(crate) fn command_descriptions() -> Vec<(String, String)> {
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
    error
        .to_string()
        .replace("Usage: channel ", "Usage: /")
        .replace("Usage: channel", "Usage: /help")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn new_command_supports_quoted_windows_cwd_and_multiword_title() {
        let command =
            parse_command(r#"/new Project review --cwd "C:\Users\Example User\Documents\repo""#)
                .unwrap();
        let ChannelCommand::New { name, cwd } = command else {
            panic!("expected /new command");
        };

        assert_eq!(name, ["Project", "review"]);
        assert_eq!(
            cwd.as_deref(),
            Some(Path::new(r"C:\Users\Example User\Documents\repo"))
        );
    }

    #[test]
    fn command_help_is_generated_from_clap_metadata() {
        let help = render_command_help();

        assert!(help.starts_with("These commands are supported:\n\n"));
        assert!(help.contains("/help - Display this command list."));
        assert!(help.contains("/new - Create and select a session."));
        assert!(help.contains("/policy - Show or change the tool permission policy."));
        assert!(help.contains("/deny - Deny a pending permission request."));
    }

    #[test]
    fn command_parse_errors_use_slash_command_usage() {
        let error = parse_command("/model").unwrap_err().to_string();

        assert!(error.contains("Usage: /model <NAME>"), "{error}");
    }

    #[test]
    fn telegram_bot_suffix_is_ignored() {
        assert!(matches!(
            parse_command("/status@dwoagent_bot").unwrap(),
            ChannelCommand::Status
        ));
    }

    #[test]
    fn platform_command_menu_uses_the_same_metadata_as_help() {
        assert!(command_descriptions().contains(&(
            "new".to_string(),
            "Create and select a session.".to_string()
        )));
    }
}
