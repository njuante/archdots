//! Implementation of `archdots export`.

use std::io::{BufRead, IsTerminal, Write as _};
use std::path::{Path, PathBuf};

use anyhow::Result;
use archdots_core::{
    error::ExportError,
    exporter::{
        ExportFormat, ExportOptions, ExportPlan, ExportReport, Exporter, ItemClassification,
        PlannedExportItem, SecretFinding, SecretSeverity,
    },
    profile::Profile,
};
use owo_colors::OwoColorize;

use crate::xdg;

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

// ── --include-secrets gate (§A.5) ─────────────────────────────────────────────

/// Collect every High-severity finding from the plan.
fn collect_high_findings<'a>(
    plan: &'a ExportPlan,
) -> Vec<(&'a PlannedExportItem, &'a SecretFinding)> {
    plan.items
        .iter()
        .flat_map(|item| {
            item.findings
                .iter()
                .filter(|f| f.severity == SecretSeverity::High)
                .map(move |f| (item, f))
        })
        .collect()
}

/// Show the banner + typed prompt on real I/O.
fn confirm_include_secrets(findings_high: &[(&PlannedExportItem, &SecretFinding)]) -> bool {
    confirm_include_secrets_inner(
        findings_high,
        &mut std::io::stderr(),
        &mut std::io::stdin().lock(),
    )
}

/// Testable inner implementation: banner → prompt → compare.
///
/// Returns `true` only when the user types exactly `I UNDERSTAND` (after
/// trimming surrounding whitespace).  Any other input, including `EOF`,
/// returns `false`.
pub(crate) fn confirm_include_secrets_inner<W, R>(
    findings_high: &[(&PlannedExportItem, &SecretFinding)],
    out: &mut W,
    inp: &mut R,
) -> bool
where
    W: std::io::Write,
    R: BufRead,
{
    let tty = std::io::stdout().is_terminal();

    macro_rules! red {
        ($s:expr) => {
            if tty {
                $s.red().to_string()
            } else {
                $s.to_string()
            }
        };
    }

    let _ = writeln!(
        out,
        "{}",
        red!("┌────────────────────────────────────────────────────────────────────┐")
    );
    let _ = writeln!(
        out,
        "{}",
        red!("│  WARNING: --include-secrets is active                              │")
    );
    let _ = writeln!(
        out,
        "{}",
        red!("│  The following High-severity findings WILL be included.            │")
    );
    let _ = writeln!(
        out,
        "{}",
        red!("└────────────────────────────────────────────────────────────────────┘")
    );
    let _ = writeln!(out);

    for (item, finding) in findings_high {
        let path = item
            .source_canonical
            .as_deref()
            .unwrap_or(&item.source)
            .display()
            .to_string();
        let _ = writeln!(
            out,
            "  {path}:{}  [{}]  {}",
            finding.line, finding.rule_id, finding.preview
        );
    }

    let _ = writeln!(out);
    let _ = write!(out, "[type 'I UNDERSTAND' to continue] ");
    let _ = out.flush();

    let mut line = String::new();
    if inp.read_line(&mut line).is_err() {
        return false;
    }
    line.trim() == "I UNDERSTAND"
}

// ── other helpers ──────────────────────────────────────────────────────────────

fn print_plan_summary(plan: &ExportPlan, output_dir: &Path, is_safe: bool) {
    let items_included = plan
        .items
        .iter()
        .filter(|i| matches!(i.classification, ItemClassification::Include {}))
        .count();
    let items_excluded_path = plan
        .items
        .iter()
        .filter(|i| matches!(i.classification, ItemClassification::ExcludeSensitivePath { .. }))
        .count();
    let items_excluded_size = plan
        .items
        .iter()
        .filter(|i| matches!(i.classification, ItemClassification::ExcludeBySize { .. }))
        .count();
    let items_excluded_binary = plan
        .items
        .iter()
        .filter(|i| matches!(i.classification, ItemClassification::ExcludeBinary {}))
        .count();
    let items_missing = plan
        .items
        .iter()
        .filter(|i| matches!(i.classification, ItemClassification::MissingSource {}))
        .count();
    let findings_high = plan
        .items
        .iter()
        .flat_map(|i| &i.findings)
        .filter(|f| f.severity == SecretSeverity::High)
        .count();
    let findings_medium = plan
        .items
        .iter()
        .flat_map(|i| &i.findings)
        .filter(|f| f.severity == SecretSeverity::Medium)
        .count();

    eprintln!("Export plan: {} → {}", plan.profile_name, output_dir.display());
    eprintln!(
        "  included={items_included} excl-path={items_excluded_path} \
         excl-size={items_excluded_size} excl-binary={items_excluded_binary} \
         missing={items_missing}"
    );
    eprintln!("  findings: high={findings_high} medium={findings_medium}");
    if !is_safe {
        eprintln!("  status: BLOCKED");
    }
}

fn print_write_report(report: &ExportReport, output_dir: &Path) {
    eprintln!("Export complete: {}", output_dir.display());
    eprintln!(
        "  included={} bytes={}",
        report.items_included, report.bytes_written
    );
    eprintln!();
    eprintln!("Next steps:");
    eprintln!("  cd {}", output_dir.display());
    eprintln!("  git init");
    eprintln!("  git add .");
    eprintln!("  git commit -m \"Initial commit (generated by archdots)\"");
    eprintln!("  gh repo create --public --source=. --push");
}

fn confirm_write() -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    print!("ready to write? [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

// ── entry point ───────────────────────────────────────────────────────────────

/// Run `archdots export` and return the process exit code.
#[allow(clippy::needless_pass_by_value)]
pub fn run(args: ExportArgs) -> Result<i32> {
    let format: ExportFormat = args.format.into();

    // 1. Reject --include-secrets on non-TTY stdin early (§A.5, §H #12).
    //    This is checked before anything expensive.
    if args.include_secrets && !std::io::stdin().is_terminal() {
        eprintln!("refusing: --include-secrets requires a TTY");
        return Ok(3);
    }

    // 2. Load profile.
    let profile_dir = xdg::profiles_dir()?;
    let profile_path = profile_dir.join(format!("{}.toml", args.profile));
    if !profile_path.exists() {
        eprintln!(
            "profile '{}' not found (looked in {})",
            args.profile,
            profile_path.display()
        );
        return Ok(3);
    }
    let profile = match Profile::load_from_file(&profile_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error loading profile: {e}");
            return Ok(3);
        }
    };

    // 3. Resolve output directory (§Q1: ./<profile>-export/ default).
    let output_dir = args
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("./{}-export", args.profile)));

    // 4. Pre-flight: output_dir must not be a regular file (§H #25).
    if output_dir.is_file() {
        eprintln!(
            "output path is a regular file: {} (expected a directory)",
            output_dir.display()
        );
        return Ok(3);
    }

    // 5. Build ExportOptions (allow_secret_rules filled in Part 4).
    let opts = ExportOptions {
        format,
        allow_paths: args.allow_path.clone(),
        allow_binary: args.allow_binary.clone(),
        allow_secret_rules: vec![],
        max_bytes: args.max_bytes,
        include_secrets: args.include_secrets,
        include_install_script: !args.no_install_script,
        include_readme: !args.no_readme,
    };

    // 6. Build Exporter.
    let home = xdg::home_dir()?;
    let exporter = Exporter::new(&profile, &profile_dir, &home);

    // 7. PLAN.
    let mut plan = match exporter.plan(&opts) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("export plan error: {e}");
            return Ok(3);
        }
    };

    // 8. SCAN (skip entirely for profile-only; §B).
    if format == ExportFormat::Full {
        if let Err(e) = exporter.scan(&mut plan) {
            eprintln!("scan error: {e}");
            return Ok(3);
        }
    }

    // 9. --include-secrets I UNDERSTAND gate (non-check mode only; §A.5, #13).
    //    --yes is orthogonal: it never skips this prompt (#13).
    if args.include_secrets && !args.check {
        let high_findings = collect_high_findings(&plan);
        if !high_findings.is_empty() {
            tracing::warn!(
                count = high_findings.len(),
                "--include-secrets: overriding High-severity findings before export"
            );
            if !confirm_include_secrets(&high_findings) {
                println!("Aborted.");
                return Ok(1);
            }
        }
    }

    // 10. DECIDE.
    let is_safe = exporter.is_safe_to_write(&plan, &opts);
    let exit_code = if is_safe { 0i32 } else { 2i32 };

    // 11. --check: report and exit, never write (§H #5).
    if args.check {
        print_plan_summary(&plan, &output_dir, is_safe);
        return Ok(exit_code);
    }

    // 12. Not safe → blocked (exit 2).
    if !is_safe {
        print_plan_summary(&plan, &output_dir, false);
        return Ok(2);
    }

    // 13. CONFIRM: ask unless --yes (§H #21).
    if !args.yes && !confirm_write()? {
        println!("Aborted.");
        return Ok(1);
    }

    // 14. WRITE.
    let report = match exporter.write(&plan, &output_dir, &opts, args.force) {
        Ok(r) => r,
        Err(ExportError::OutputNotEmpty(_)) => {
            eprintln!(
                "output directory is not empty: {}; use --force to overwrite",
                output_dir.display()
            );
            return Ok(3);
        }
        Err(ExportError::InvalidOptions(msg)) => {
            eprintln!("invalid options: {msg}");
            return Ok(3);
        }
        Err(ExportError::Io { path, source }) => {
            eprintln!("I/O error at {}: {source}", path.display());
            return Ok(3);
        }
        Err(ExportError::Unsafe) => {
            eprintln!(
                "internal error: Unsafe from write() despite is_safe_to_write=true; \
                 please file an issue"
            );
            return Ok(3);
        }
        Err(e) => {
            eprintln!("export error: {e}");
            return Ok(3);
        }
    };

    // 15. Post-write report + hints (refined in Part 4).
    print_write_report(&report, &output_dir);
    Ok(0)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use archdots_core::exporter::{ExportFormat, ExportOptions};
    use clap::Parser;

    use super::{ExportArgs, ExportFormatArg, confirm_include_secrets_inner};

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(flatten)]
        args: ExportArgs,
    }

    // ── Part 1: parsing ───────────────────────────────────────────────────────

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
        let opts = ExportOptions {
            include_install_script: !cli.args.no_install_script,
            include_readme: !cli.args.no_readme,
            ..ExportOptions::default()
        };
        assert!(!opts.include_install_script);
        assert!(!opts.include_readme);
    }

    // ── Part 3: confirm_include_secrets_inner ─────────────────────────────────

    fn run_prompt(input: &str) -> bool {
        let findings: Vec<_> = vec![];
        let mut out = Vec::new();
        let mut inp = std::io::Cursor::new(input.as_bytes());
        confirm_include_secrets_inner(&findings, &mut out, &mut inp)
    }

    #[test]
    fn confirm_exact_i_understand_returns_true() {
        assert!(run_prompt("I UNDERSTAND\n"));
    }

    #[test]
    fn confirm_i_understand_with_surrounding_whitespace_returns_true() {
        assert!(run_prompt("  I UNDERSTAND  \n"));
    }

    #[test]
    fn confirm_lowercase_i_understand_returns_false() {
        assert!(!run_prompt("i understand\n"));
    }

    #[test]
    fn confirm_mixed_case_returns_false() {
        assert!(!run_prompt("I understand\n"));
    }

    #[test]
    fn confirm_yes_returns_false() {
        assert!(!run_prompt("YES\n"));
    }

    #[test]
    fn confirm_empty_returns_false() {
        assert!(!run_prompt("\n"));
    }

    #[test]
    fn confirm_eof_returns_false() {
        assert!(!run_prompt(""));
    }
}
