//! Implementation of `archdots export`.

use std::io::{BufRead, IsTerminal, Write as _};
use std::path::{Path, PathBuf};

use anyhow::Result;
use archdots_core::{
    error::ExportError,
    exporter::{
        ExportFormat, ExportOptions, ExportPlan, ExportReport, Exporter, ItemClassification,
        PlannedExportItem, SecretAllowance, SecretFinding, SecretSeverity,
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

// ── --allow-secret parsing (§E) ───────────────────────────────────────────────

/// Parse raw `--allow-secret` strings (`ID` or `ID:GLOB`) into [`SecretAllowance`]s.
///
/// Splits on the **first** `:` only.  An empty string or an empty rule ID is
/// an error.
pub fn parse_allow_secrets(raw: &[String]) -> Result<Vec<SecretAllowance>, String> {
    let mut out = Vec::with_capacity(raw.len());
    for s in raw {
        if s.is_empty() {
            return Err("--allow-secret: rule ID must not be empty".to_string());
        }
        let (rule_id, path_glob) = match s.find(':') {
            Some(idx) => {
                let id = s[..idx].to_string();
                if id.is_empty() {
                    return Err("--allow-secret: rule ID must not be empty".to_string());
                }
                (id, Some(s[idx + 1..].to_string()))
            }
            None => (s.clone(), None),
        };
        out.push(SecretAllowance { rule_id, path_glob });
    }
    Ok(out)
}

// ── --include-secrets gate (§A.5) ─────────────────────────────────────────────

fn collect_high_findings(plan: &ExportPlan) -> Vec<(&PlannedExportItem, &SecretFinding)> {
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

fn confirm_include_secrets(findings_high: &[(&PlannedExportItem, &SecretFinding)]) -> bool {
    confirm_include_secrets_inner(
        findings_high,
        &mut std::io::stderr(),
        &mut std::io::stdin().lock(),
    )
}

/// Testable inner: shows the warning banner + typed prompt.
///
/// Returns `true` only when the user types `I UNDERSTAND` (after whitespace trim).
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

// ── text report ───────────────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
fn render_text_report(plan: &ExportPlan, output_dir: &Path, is_safe: bool) {
    let tty = std::io::stdout().is_terminal();
    let ok = || {
        if tty {
            "✓".green().to_string()
        } else {
            "✓".to_string()
        }
    };
    let warn = || {
        if tty {
            "⚠".yellow().to_string()
        } else {
            "!".to_string()
        }
    };
    let err = || {
        if tty {
            "✗".red().to_string()
        } else {
            "✗".to_string()
        }
    };

    let included = plan
        .items
        .iter()
        .filter(|i| matches!(i.classification, ItemClassification::Include {}))
        .count();
    let excl_path = plan
        .items
        .iter()
        .filter(|i| {
            matches!(
                i.classification,
                ItemClassification::ExcludeSensitivePath { .. }
            )
        })
        .count();
    let excl_size = plan
        .items
        .iter()
        .filter(|i| matches!(i.classification, ItemClassification::ExcludeBySize { .. }))
        .count();
    let excl_bin = plan
        .items
        .iter()
        .filter(|i| matches!(i.classification, ItemClassification::ExcludeBinary {}))
        .count();
    let missing = plan
        .items
        .iter()
        .filter(|i| matches!(i.classification, ItemClassification::MissingSource {}))
        .count();

    let high_items: Vec<_> = plan
        .items
        .iter()
        .flat_map(|i| {
            i.findings
                .iter()
                .filter(|f| f.severity == SecretSeverity::High)
                .map(move |f| (&i.source, f))
        })
        .collect();
    let med_items: Vec<_> = plan
        .items
        .iter()
        .flat_map(|i| {
            i.findings
                .iter()
                .filter(|f| f.severity == SecretSeverity::Medium)
                .map(move |f| (&i.source, f))
        })
        .collect();

    println!("Export plan: {}", plan.profile_name);
    println!("Output:      {}", output_dir.display());
    println!();
    println!("Items:");
    println!("  {} {} included", ok(), included);
    if excl_path > 0 {
        println!("  {} {} excluded (sensitive path)", warn(), excl_path);
    }
    if excl_size > 0 {
        println!("  {} {} excluded (too large)", warn(), excl_size);
    }
    if excl_bin > 0 {
        println!("  {} {} excluded (binary)", warn(), excl_bin);
    }
    if missing > 0 {
        println!("  {} {} missing source", warn(), missing);
    }

    if !high_items.is_empty() || !med_items.is_empty() {
        println!();
        println!("Findings:");
        for (src, f) in &high_items {
            let marker = if tty {
                "HIGH".red().to_string()
            } else {
                "HIGH".to_string()
            };
            println!(
                "  {} {} {}:{}  [{}]  {}",
                err(),
                marker,
                src.display(),
                f.line,
                f.rule_id,
                f.preview
            );
        }
        for (src, f) in &med_items {
            let marker = if tty {
                "MED".yellow().to_string()
            } else {
                "MED".to_string()
            };
            println!(
                "  {} {} {}:{}  [{}]  {}",
                warn(),
                marker,
                src.display(),
                f.line,
                f.rule_id,
                f.preview
            );
        }
    }

    println!();
    if is_safe {
        println!("Status: {} ready to export", ok());
    } else if excl_path > 0 {
        println!(
            "Status: {} blocked — sensitive path(s) excluded; use --allow-path to override",
            err()
        );
    } else {
        println!(
            "Status: {} blocked — High-severity finding(s); use --allow-secret or \
             --include-secrets to override",
            err()
        );
    }
}

fn render_text_write_report(report: &ExportReport, output_dir: &Path) {
    let tty = std::io::stdout().is_terminal();
    let ok = if tty {
        "✓".green().to_string()
    } else {
        "✓".to_string()
    };

    println!("{ok} Export complete: {}", output_dir.display());
    println!();
    println!("Summary:");
    println!("  {} files included", report.items_included);
    if report.items_excluded_by_path > 0 {
        println!(
            "  {} excluded (sensitive path)",
            report.items_excluded_by_path
        );
    }
    if report.items_excluded_by_size > 0 {
        println!("  {} excluded (too large)", report.items_excluded_by_size);
    }
    if report.items_excluded_binary > 0 {
        println!("  {} excluded (binary)", report.items_excluded_binary);
    }
    if report.items_missing > 0 {
        println!("  {} missing source", report.items_missing);
    }
    if report.findings_high > 0 || report.findings_medium > 0 {
        println!(
            "  {} High / {} Medium findings",
            report.findings_high, report.findings_medium
        );
    }
    if report.findings_overridden > 0 {
        println!(
            "  {} finding(s) overridden by --include-secrets / --allow-secret",
            report.findings_overridden
        );
    }
    println!("  {} bytes written", report.bytes_written);
}

fn print_next_steps(output_dir: &Path) {
    println!();
    println!("Next steps:");
    println!("  cd {}", output_dir.display());
    println!("  git init");
    println!("  git add .");
    println!("  git commit -m \"Initial commit (generated by archdots)\"");
    println!("  gh repo create --public --source=. --push");
}

// ── JSON report (§8) ──────────────────────────────────────────────────────────

fn summary_from_plan(plan: &ExportPlan, bytes_written: u64, home: &Path) -> serde_json::Value {
    serde_json::json!({
        "items_included": plan.items.iter().filter(|i| matches!(i.classification, ItemClassification::Include {})).count(),
        "items_excluded_by_path": plan.items.iter().filter(|i| matches!(i.classification, ItemClassification::ExcludeSensitivePath { .. })).count(),
        "items_excluded_by_size": plan.items.iter().filter(|i| matches!(i.classification, ItemClassification::ExcludeBySize { .. })).count(),
        "items_excluded_binary": plan.items.iter().filter(|i| matches!(i.classification, ItemClassification::ExcludeBinary {})).count(),
        "items_missing": plan.items.iter().filter(|i| matches!(i.classification, ItemClassification::MissingSource {})).count(),
        "findings_high": plan.items.iter().flat_map(|i| &i.findings).filter(|f| f.severity == SecretSeverity::High).count(),
        "findings_medium": plan.items.iter().flat_map(|i| &i.findings).filter(|f| f.severity == SecretSeverity::Medium).count(),
        "findings_overridden": plan.count_overridden_findings(home),
        "bytes_written": bytes_written,
    })
}

fn summary_from_report(report: &ExportReport) -> serde_json::Value {
    serde_json::json!({
        "items_included": report.items_included,
        "items_excluded_by_path": report.items_excluded_by_path,
        "items_excluded_by_size": report.items_excluded_by_size,
        "items_excluded_binary": report.items_excluded_binary,
        "items_missing": report.items_missing,
        "findings_high": report.findings_high,
        "findings_medium": report.findings_medium,
        "findings_overridden": report.findings_overridden,
        "bytes_written": report.bytes_written,
    })
}

fn render_json(
    plan: &ExportPlan,
    profile: &str,
    output_dir: &Path,
    format: ExportFormat,
    wrote: bool,
    summary: serde_json::Value,
    exit_code: i32,
) -> Result<()> {
    let items = serde_json::to_value(&plan.items)?;
    let mut obj = serde_json::Map::new();
    obj.insert("schema_version".to_string(), serde_json::json!(1));
    obj.insert("profile".to_string(), serde_json::json!(profile));
    obj.insert("output_dir".to_string(), serde_json::to_value(output_dir)?);
    obj.insert("format".to_string(), serde_json::to_value(format)?);
    obj.insert("wrote".to_string(), serde_json::json!(wrote));
    obj.insert("items".to_string(), items);
    obj.insert("summary".to_string(), summary);
    obj.insert("exit_code".to_string(), serde_json::json!(exit_code));
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::Value::Object(obj))?
    );
    Ok(())
}

// ── confirm write ──────────────────────────────────────────────────────────────

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
#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
pub fn run(args: ExportArgs) -> Result<i32> {
    let format: ExportFormat = args.format.into();

    // 1. Validate flag combinations early (§H #20).
    if format == ExportFormat::ProfileOnly && args.include_secrets {
        eprintln!("--include-secrets is meaningless with --format profile-only");
        return Ok(3);
    }

    // 2. Reject --include-secrets on non-TTY stdin (§A.5, §H #12).
    if args.include_secrets && !std::io::stdin().is_terminal() {
        eprintln!("refusing: --include-secrets requires a TTY");
        return Ok(3);
    }

    // 3. Load profile.
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

    // 4. Resolve output directory (§Q1: ./<profile>-export/ default).
    let output_dir = args
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("./{}-export", args.profile)));

    // 5. Pre-flight: output_dir must not be a regular file (§H #25).
    if output_dir.is_file() {
        eprintln!(
            "output path is a regular file: {} (expected a directory)",
            output_dir.display()
        );
        return Ok(3);
    }

    // 6. Parse --allow-secret strings.
    let allow_secret_rules = match parse_allow_secrets(&args.allow_secret) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return Ok(3);
        }
    };

    // 7. Build ExportOptions.
    let opts = ExportOptions {
        format,
        allow_paths: args.allow_path.clone(),
        allow_binary: args.allow_binary.clone(),
        allow_secret_rules,
        max_bytes: args.max_bytes,
        include_secrets: args.include_secrets,
        include_install_script: !args.no_install_script,
        include_readme: !args.no_readme,
    };

    // 8. Build Exporter.
    let home = xdg::home_dir()?;
    let exporter = Exporter::new(&profile, &profile_dir, &home);

    // 9. PLAN.
    let mut plan = match exporter.plan(&opts) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("export plan error: {e}");
            return Ok(3);
        }
    };

    // 10. SCAN (skip entirely for profile-only; §B).
    if format == ExportFormat::Full {
        if let Err(e) = exporter.scan(&mut plan) {
            eprintln!("scan error: {e}");
            return Ok(3);
        }
    }

    // 11. --include-secrets I UNDERSTAND gate (non-check mode only; §A.5, #13).
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

    // 12. DECIDE.
    let is_safe = exporter.is_safe_to_write(&plan, &opts);
    let exit_code = if is_safe { 0i32 } else { 2i32 };

    // 13. --check: report and exit, never write (§H #5).
    if args.check {
        if args.json {
            render_json(
                &plan,
                &args.profile,
                &output_dir,
                format,
                false,
                summary_from_plan(&plan, 0, &home),
                exit_code,
            )?;
        } else {
            render_text_report(&plan, &output_dir, is_safe);
        }
        return Ok(exit_code);
    }

    // 14. Not safe → blocked (exit 2).
    if !is_safe {
        if args.json {
            render_json(
                &plan,
                &args.profile,
                &output_dir,
                format,
                false,
                summary_from_plan(&plan, 0, &home),
                2,
            )?;
        } else {
            render_text_report(&plan, &output_dir, false);
        }
        return Ok(2);
    }

    // 15. CONFIRM: ask unless --yes (§H #21).
    if !args.yes && !confirm_write()? {
        println!("Aborted.");
        return Ok(1);
    }

    // 16. WRITE.
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

    // 17. Post-write report + hints.
    if args.json {
        render_json(
            &plan,
            &args.profile,
            &output_dir,
            format,
            true,
            summary_from_report(&report),
            0,
        )?;
    } else {
        render_text_write_report(&report, &output_dir);
        print_next_steps(&output_dir);
    }

    Ok(0)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use archdots_core::exporter::{ExportFormat, ExportOptions};
    use clap::Parser;

    use super::{confirm_include_secrets_inner, parse_allow_secrets, ExportArgs, ExportFormatArg};

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
        assert!(TestCli::try_parse_from(["test", "myprofile", "--format", "invalid"]).is_err());
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
        let cli = TestCli::try_parse_from(["test", "myprofile", "--max-bytes", "2097152"]).unwrap();
        assert_eq!(cli.args.max_bytes, 2_097_152);
    }

    #[test]
    fn parse_no_install_script_and_no_readme_invert_export_opts_bools() {
        let cli =
            TestCli::try_parse_from(["test", "myprofile", "--no-install-script", "--no-readme"])
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

    // ── Part 4: parse_allow_secrets ───────────────────────────────────────────

    #[test]
    fn allow_secret_id_only() {
        let result = parse_allow_secrets(&["jwt".to_string()]).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].rule_id, "jwt");
        assert!(result[0].path_glob.is_none());
    }

    #[test]
    fn allow_secret_id_with_glob() {
        let result = parse_allow_secrets(&["jwt:.config/foo.json".to_string()]).unwrap();
        assert_eq!(result[0].rule_id, "jwt");
        assert_eq!(result[0].path_glob.as_deref(), Some(".config/foo.json"));
    }

    #[test]
    fn allow_secret_glob_with_colon_in_path_splits_on_first_colon() {
        // A glob might theoretically have colons; only the first one is the separator.
        let result = parse_allow_secrets(&["rule:path/a:b".to_string()]).unwrap();
        assert_eq!(result[0].rule_id, "rule");
        assert_eq!(result[0].path_glob.as_deref(), Some("path/a:b"));
    }

    #[test]
    fn allow_secret_empty_string_is_error() {
        assert!(parse_allow_secrets(&[String::new()]).is_err());
    }

    #[test]
    fn allow_secret_colon_only_is_error() {
        assert!(parse_allow_secrets(&[":".to_string()]).is_err());
    }

    // ── Part 4: JSON shape ────────────────────────────────────────────────────

    #[test]
    fn json_schema_version_is_1_and_classification_is_object() {
        use archdots_core::exporter::{ExportPlan, ItemClassification, PlannedExportItem};
        use std::path::PathBuf;

        let item = PlannedExportItem {
            entry_id: "test".to_string(),
            source: PathBuf::from("/home/u/.zshrc"),
            source_canonical: Some(PathBuf::from("/home/u/.zshrc")),
            target: PathBuf::from("/home/u/.zshrc"),
            rel_in_repo: Some(PathBuf::from("dotfiles/.zshrc")),
            classification: ItemClassification::Include {},
            findings: vec![],
            size_bytes: Some(42),
            is_text: Some(true),
        };

        let plan = ExportPlan {
            profile_name: "test-profile".to_string(),
            items: vec![item],
            options: ExportOptions::default(),
        };

        let items_val = serde_json::to_value(&plan.items).unwrap();
        let classification = &items_val[0]["classification"];
        assert!(
            classification.is_object(),
            "classification must be a JSON object, got: {classification}"
        );
        assert!(
            classification.get("include").is_some(),
            "Include variant must have 'include' key"
        );

        // Verify the full JSON shape.
        let home = PathBuf::from("/home/u");
        let summary = super::summary_from_plan(&plan, 0, &home);
        let output_dir = PathBuf::from("/tmp/test-export");
        let mut obj = serde_json::Map::new();
        obj.insert("schema_version".to_string(), serde_json::json!(1));
        obj.insert("profile".to_string(), serde_json::json!("test-profile"));
        obj.insert(
            "output_dir".to_string(),
            serde_json::to_value(&output_dir).unwrap(),
        );
        obj.insert(
            "format".to_string(),
            serde_json::to_value(ExportFormat::Full).unwrap(),
        );
        obj.insert("wrote".to_string(), serde_json::json!(false));
        obj.insert("items".to_string(), items_val.clone());
        obj.insert("summary".to_string(), summary);
        obj.insert("exit_code".to_string(), serde_json::json!(0));
        let json = serde_json::Value::Object(obj);

        assert_eq!(json["schema_version"], 1);
        assert!(json["items"].is_array());
        assert!(
            json["items"][0]["classification"].is_object(),
            "items[0].classification must be object"
        );
    }
}
