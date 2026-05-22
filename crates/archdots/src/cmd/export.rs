//! Implementation of `archdots export`.

use std::path::PathBuf;

use anyhow::Result;
use archdots_core::exporter::{ExportFormat, ExportOptions};

// ── CLI arg type for --format ─────────────────────────────────────────────────

/// Local proxy for [`ExportFormat`] that also implements [`clap::ValueEnum`].
#[derive(Debug, Clone, clap::ValueEnum)]
pub enum ExportFormatArg {
    Full,
    #[value(name = "profile-only")]
    ProfileOnly,
}

impl From<ExportFormatArg> for ExportFormat {
    fn from(arg: ExportFormatArg) -> Self {
        match arg {
            ExportFormatArg::Full => ExportFormat::Full,
            ExportFormatArg::ProfileOnly => ExportFormat::ProfileOnly,
        }
    }
}

// ── CLI args ──────────────────────────────────────────────────────────────────

/// Arguments for `archdots export`.
#[derive(Debug, clap::Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct ExportArgs {
    /// Profile name (under XDG config home/archdots/profiles/).
    pub profile: String,

    /// Destination directory. Default: ./<profile>-export/
    #[arg(long, short = 'o', value_name = "DIR")]
    pub output: Option<PathBuf>,

    /// Allow writing into an existing non-empty directory.
    #[arg(long)]
    pub force: bool,

    /// Output format (full or profile-only). Default: full.
    #[arg(long, value_name = "FORMAT", default_value = "full")]
    pub format: ExportFormatArg,

    /// Whitelist a path against the sensitive-path filter (repeatable, glob-aware).
    #[arg(long, value_name = "GLOB")]
    pub allow_path: Vec<String>,

    /// Whitelist a path against the binary-content filter (repeatable, glob-aware).
    #[arg(long, value_name = "GLOB")]
    pub allow_binary: Vec<String>,

    /// Whitelist a secret-scanner rule, optionally scoped to a path glob (repeatable).
    #[arg(long, value_name = "ID[:GLOB]")]
    pub allow_secret: Vec<String>,

    /// Maximum per-file size in bytes.
    #[arg(long, default_value_t = 1_048_576u64)]
    pub max_bytes: u64,

    /// Disable the content-scan abort. Requires a TTY and typed confirmation.
    #[arg(long)]
    pub include_secrets: bool,

    /// Run plan + scan, print report, exit without writing.
    #[arg(long)]
    pub check: bool,

    /// Do not generate install.sh.
    #[arg(long)]
    pub no_install_script: bool,

    /// Do not generate README.md.
    #[arg(long)]
    pub no_readme: bool,

    /// Skip the "ready to write?" confirmation. Does not bypass --include-secrets prompt.
    #[arg(long, short)]
    pub yes: bool,

    /// Emit the export report as JSON to stdout.
    #[arg(long)]
    pub json: bool,
}

// ── entry point (stub — full pipeline lands in subsequent commits) ─────────────

/// Run `archdots export` and return the process exit code.
#[allow(clippy::needless_pass_by_value)]
pub fn run(args: ExportArgs) -> Result<i32> {
    let format: ExportFormat = args.format.into();
    let _opts = ExportOptions {
        format,
        allow_paths: args.allow_path.clone(),
        allow_binary: args.allow_binary.clone(),
        allow_secret_rules: vec![],
        max_bytes: args.max_bytes,
        include_secrets: args.include_secrets,
        include_install_script: !args.no_install_script,
        include_readme: !args.no_readme,
    };
    Ok(0)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use archdots_core::exporter::{ExportFormat, ExportOptions};
    use clap::Parser;

    use super::{ExportArgs, ExportFormatArg};

    /// Minimal top-level parser that delegates to [`ExportArgs`].
    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(flatten)]
        args: ExportArgs,
    }

    #[test]
    fn parse_minimal_all_defaults() {
        let cli = TestCli::try_parse_from(["test", "myprofile"]).unwrap();
        assert_eq!(cli.args.profile, "myprofile");
        assert!(cli.args.output.is_none());
        assert!(!cli.args.force);
        assert!(matches!(cli.args.format, ExportFormatArg::Full));
        assert!(cli.args.allow_path.is_empty());
        assert!(cli.args.allow_binary.is_empty());
        assert!(cli.args.allow_secret.is_empty());
        assert_eq!(cli.args.max_bytes, 1_048_576);
        assert!(!cli.args.include_secrets);
        assert!(!cli.args.check);
        assert!(!cli.args.no_install_script);
        assert!(!cli.args.no_readme);
        assert!(!cli.args.yes);
        assert!(!cli.args.json);
    }

    #[test]
    fn parse_format_profile_only_maps_to_correct_variant() {
        let cli =
            TestCli::try_parse_from(["test", "myprofile", "--format", "profile-only"]).unwrap();
        assert!(matches!(cli.args.format, ExportFormatArg::ProfileOnly));
        let fmt: ExportFormat = cli.args.format.into();
        assert_eq!(fmt, ExportFormat::ProfileOnly);
    }

    #[test]
    fn parse_format_invalid_is_clap_error() {
        assert!(
            TestCli::try_parse_from(["test", "myprofile", "--format", "invalid"]).is_err()
        );
    }

    #[test]
    fn parse_repeatable_flags_accumulate_in_vec() {
        let cli = TestCli::try_parse_from([
            "test",
            "myprofile",
            "--allow-path",
            ".config/hypr",
            "--allow-path",
            ".config/waybar",
            "--allow-secret",
            "jwt",
            "--allow-secret",
            "aws-access-key-id:.aws/mock",
        ])
        .unwrap();
        assert_eq!(cli.args.allow_path, vec![".config/hypr", ".config/waybar"]);
        assert_eq!(
            cli.args.allow_secret,
            vec!["jwt", "aws-access-key-id:.aws/mock"]
        );
    }

    #[test]
    fn parse_max_bytes_default_is_1_mib() {
        let cli = TestCli::try_parse_from(["test", "myprofile"]).unwrap();
        assert_eq!(cli.args.max_bytes, 1_048_576);
    }

    #[test]
    fn parse_max_bytes_custom() {
        let cli =
            TestCli::try_parse_from(["test", "myprofile", "--max-bytes", "2097152"]).unwrap();
        assert_eq!(cli.args.max_bytes, 2_097_152);
    }

    #[test]
    fn parse_no_install_script_and_no_readme_invert_export_opts_bools() {
        let cli = TestCli::try_parse_from([
            "test",
            "myprofile",
            "--no-install-script",
            "--no-readme",
        ])
        .unwrap();
        assert!(cli.args.no_install_script);
        assert!(cli.args.no_readme);
        // Wire up the inversion as run() does.
        let opts = ExportOptions {
            include_install_script: !cli.args.no_install_script,
            include_readme: !cli.args.no_readme,
            ..ExportOptions::default()
        };
        assert!(!opts.include_install_script);
        assert!(!opts.include_readme);
    }
}
