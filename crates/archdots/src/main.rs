#![deny(clippy::all)]
#![warn(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]

//! `archdots` — dotfile manager for Arch Linux ricers.

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::{fmt, EnvFilter};

mod cmd;
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
        #[arg(long, default_value = "default")]
        name: String,
        #[arg(long, value_name = "PATH")]
        output: Option<std::path::PathBuf>,
        #[arg(long)]
        force: bool,
    },
    /// Manage saved profiles.
    Profile {
        #[command(subcommand)]
        command: ProfileCommands,
    },
    /// Apply a profile: create/replace symlinks for all its file entries.
    Apply {
        /// Profile name.
        profile: String,
        /// Show plan without making any changes.
        #[arg(long)]
        dry_run: bool,
        /// Proceed even when conflicts are detected.
        #[arg(long)]
        force: bool,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
    /// Show diff between profile sources and current targets.
    Diff {
        /// Profile name.
        profile: String,
    },
    /// Restore the pre-apply snapshot for a profile.
    Rollback {
        /// Profile name (required to avoid accidental global rollback).
        #[arg(long)]
        profile: String,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
    /// Show the apply/rollback history from the journal.
    History {
        /// Filter by profile name.
        #[arg(long)]
        profile: Option<String>,
        /// Maximum number of entries to show.
        #[arg(long, default_value = "20")]
        limit: usize,
        /// Show all entries without a limit.
        #[arg(long)]
        all: bool,
    },
    /// Manage snapshots (list, show, prune).
    Snapshots {
        #[command(subcommand)]
        command: SnapshotsCmd,
    },
    /// Close orphaned in-progress journal entries left by crashed applies.
    Recover {
        /// Filter by profile name.
        #[arg(long)]
        profile: Option<String>,
        /// Recover all orphans without prompting.
        #[arg(long, short)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ProfileCommands {
    /// List all saved profiles.
    List,
    /// Show the contents of a saved profile.
    Show { name: String },
    /// Delete a saved profile.
    Delete {
        name: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SnapshotsCmd {
    /// List all snapshots.
    List {
        #[arg(long)]
        profile: Option<String>,
    },
    /// Show the full manifest of a snapshot by id prefix (min 6 chars).
    Show { id_prefix: String },
    /// Remove old snapshots according to a retention policy.
    Prune {
        /// Keep the N most recent snapshots.
        #[arg(long, conflicts_with = "older_than")]
        keep: Option<usize>,
        /// Remove snapshots older than this duration (e.g. 30d, 1h, 60m).
        #[arg(long)]
        older_than: Option<String>,
        /// Scope to a single profile (with --keep: per-profile limit).
        #[arg(long)]
        profile: Option<String>,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
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
        } => cmd::init::run(&name, output, force),

        Commands::Profile { command } => match command {
            ProfileCommands::List => cmd::profile::run_list(),
            ProfileCommands::Show { name } => cmd::profile::run_show(&name),
            ProfileCommands::Delete { name, yes } => cmd::profile::run_delete(&name, yes),
        },

        Commands::Apply {
            profile,
            dry_run,
            force,
            yes,
        } => cmd::apply::run(&profile, dry_run, force, yes),

        Commands::Diff { profile } => cmd::diff::run(&profile),

        Commands::Rollback { profile, yes } => cmd::rollback::run(&profile, yes),

        Commands::History {
            profile,
            limit,
            all,
        } => cmd::history::run(profile.as_deref(), limit, all),

        Commands::Snapshots { command } => match command {
            SnapshotsCmd::List { profile } => cmd::snapshots::run_list(profile.as_deref()),
            SnapshotsCmd::Show { id_prefix } => cmd::snapshots::run_show(&id_prefix),
            SnapshotsCmd::Prune {
                keep,
                older_than,
                profile,
                yes,
            } => cmd::snapshots::run_prune(keep, older_than, profile.as_deref(), yes),
        },

        Commands::Recover { profile, yes } => cmd::recover::run(profile.as_deref(), yes),
    }
}
