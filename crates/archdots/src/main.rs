use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::{fmt, EnvFilter};

/// archdots — dotfile manager for Arch Linux ricers.
#[derive(Debug, Parser)]
#[command(name = "archdots", version, about, long_about = None, arg_required_else_help = true)]
struct Cli {
    /// Log verbosity: off, error, warn, info, debug, trace.
    #[arg(long, global = true, default_value = "warn")]
    log_level: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Initialize a new archdots profile by scanning ~/.config.
    Init,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    fmt()
        .with_env_filter(EnvFilter::try_new(&cli.log_level)?)
        .with_target(false)
        .init();

    match cli.command {
        Commands::Init => {
            tracing::info!("init subcommand — not yet implemented");
            anyhow::bail!("archdots init is not implemented yet (Fase 1)");
        }
    }
}
