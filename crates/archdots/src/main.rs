#![deny(clippy::all)]
#![warn(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)] // binary entry points; errors go to anyhow

//! `archdots` — dotfile manager for Arch Linux ricers.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::{fmt, EnvFilter};

mod init;
mod profile_cmds;
mod xdg;

// ── CLI types ─────────────────────────────────────────────────────────────────

/// archdots — dotfile manager for Arch Linux ricers.
#[derive(Debug, Parser)]
#[command(
    name = "archdots",
    version,
    about,
    long_about = None,
    arg_required_else_help = true
)]
struct Cli {
    /// Log verbosity: off | error | warn | info | debug | trace.
    #[arg(long, global = true, default_value = "warn")]
    log_level: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Scan $HOME for known dotfiles and generate a starter profile.
    Init {
        /// Profile name (must match [a-z0-9_-]+).
        #[arg(long, default_value = "default")]
        name: String,
        /// Write profile to this path instead of the default XDG location.
        #[arg(long, value_name = "PATH")]
        output: Option<PathBuf>,
        /// Overwrite an existing profile without prompting.
        #[arg(long)]
        force: bool,
    },
    /// Manage saved profiles.
    Profile {
        #[command(subcommand)]
        command: ProfileCommands,
    },
}

#[derive(Debug, Subcommand)]
enum ProfileCommands {
    /// List all saved profiles.
    List,
    /// Show the contents of a saved profile.
    Show {
        /// Profile name.
        name: String,
    },
    /// Delete a saved profile.
    Delete {
        /// Profile name.
        name: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

// ── entry point ───────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = Cli::parse();

    fmt()
        .with_env_filter(EnvFilter::try_new(&cli.log_level)?)
        .with_target(false)
        .init();

    match cli.command {
        Commands::Init {
            name,
            output,
            force,
        } => init::run(&name, output, force),
        Commands::Profile { command } => match command {
            ProfileCommands::List => profile_cmds::run_list(),
            ProfileCommands::Show { name } => profile_cmds::run_show(&name),
            ProfileCommands::Delete { name, yes } => profile_cmds::run_delete(&name, yes),
        },
    }
}
