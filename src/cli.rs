//! CLI entry point.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::host::{HostMode, run_host_sync};
use crate::ingress::run_weixin_login_sync;

#[derive(Debug, Parser)]
#[command(name = "dwo-agent", about = "Dwo Agent (赤铎) CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run ACP over stdio.
    Acp(AgentFolderArgs),

    /// Run long-lived service ingress channels.
    Serve(AgentFolderArgs),

    /// Manage external channels.
    #[command(subcommand)]
    Channel(ChannelCommand),
}

#[derive(Debug, Subcommand)]
enum ChannelCommand {
    /// Log in to a channel and persist its credentials.
    #[command(subcommand)]
    Login(ChannelLoginCommand),
}

#[derive(Debug, Subcommand)]
enum ChannelLoginCommand {
    /// Log in to Weixin by scanning a QR code.
    Weixin(AgentFolderArgs),
}

#[derive(Debug, clap::Args)]
struct AgentFolderArgs {
    /// Agent folder or workspace root containing `agent-structure/`.
    #[arg(long, default_value = ".")]
    agent_folder: PathBuf,
}

/// CLI entry point invoked by `main.rs`.
pub fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Acp(args) => run_host_sync(args.agent_folder, HostMode::AcpStdio),
        Command::Serve(args) => run_host_sync(args.agent_folder, HostMode::ServiceIngress),
        Command::Channel(ChannelCommand::Login(ChannelLoginCommand::Weixin(args))) => {
            run_weixin_login_sync(args.agent_folder)
        }
    }
}
