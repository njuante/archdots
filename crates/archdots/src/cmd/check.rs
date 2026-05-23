//! Implementation of `archdots check`.

use std::io::IsTerminal;

use anyhow::{bail, Result};
use archdots_core::{
    packages::{AurHelper, PackageDB, PackageError},
    profile::Profile,
    validator::{
        DepKind, DepSource, DepStatus, ValidationReport, ValidationWarning, Validator,
        ValidatorError, ValidatorOptions,
    },
};
use owo_colors::OwoColorize;

use crate::xdg;

/// Arguments for `archdots check`.
#[allow(clippy::struct_excessive_bools)]
pub struct CheckArgs {
    /// Profile name.
    pub profile: String,
    /// Promote implicit-missing to required-missing for exit-code purposes.
    pub strict: bool,
    /// Fall back to `pacman -F` for binaries not in the curated table.
    pub deep: bool,
    /// Emit machine-readable JSON instead of a human-readable table.
    pub json: bool,
    /// Show all mentions in implicit entries (default: first 3 only).
    pub verbose: bool,
}

/// Run `archdots check` and return the process exit code.
#[allow(clippy::needless_pass_by_value)]
pub fn run(args: CheckArgs) -> Result<i32> {
    // 1. Load profile.
    let profile_dir = xdg::profiles_dir()?;
    let profile_path = profile_dir.join(format!("{}.toml", args.profile));
    if !profile_path.exists() {
        bail!(
            "profile '{}' not found (looked in {})",
            args.profile,
            profile_path.display()
        );
    }
    let profile = match Profile::load_from_file(&profile_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error loading profile: {e}");
            return Ok(3);
        }
    };

    // 2. Build PackageDB.
    let db = match PackageDB::new() {
        Ok(db) => db,
        Err(PackageError::PacmanMissing) => {
            eprintln!(
                "error: archdots check requires an Arch-based system (pacman not found on PATH)"
            );
            return Ok(3);
        }
        Err(e) => bail!(e),
    };

    // 3. Validate.
    let home = xdg::home_dir()?;
    let validator = Validator::new(&db, &home);
    let opts = ValidatorOptions {
        strict: args.strict,
        deep: args.deep,
    };
    let report = match validator.validate(&profile, opts) {
        Ok(r) => r,
        Err(ValidatorError::Package(PackageError::PacmanMissing)) => {
            eprintln!(
                "error: archdots check requires an Arch-based system (pacman not found on PATH)"
            );
            return Ok(3);
        }
        Err(ValidatorError::Profile(e)) => {
            eprintln!("error in profile: {e}");
            return Ok(3);
        }
        Err(e) => bail!(e),
    };

    // 4. Render.
    if args.json {
        render_json(&report, args.strict)?;
    } else {
        render_table(&report, args.verbose, args.strict);
    }

    // 5. Exit code (strict may promote implicit-missing to required-missing).
    let mut code = report.exit_code();
    if args.strict && code < 3 {
        let has_implicit_missing = report.entries.iter().any(|e| {
            matches!(e.kind, DepKind::ImplicitFromConfig)
                && matches!(
                    e.status,
                    DepStatus::Missing | DepStatus::UnknownBinary { .. }
                )
        });
        if has_implicit_missing && (code < 1 || code == 2) {
            code = 1;
        }
    }

    Ok(code)
}

// ── text renderer ─────────────────────────────────────────────────────────────

fn render_table(report: &ValidationReport, verbose: bool, strict: bool) {
    let tty = std::io::stdout().is_terminal();

    println!("Heuristic — review manually.");
    println!();
    print!("Profile: {}", report.profile_name);
    match report.aur_helper {
        Some(AurHelper::Paru) => println!("   AUR helper: paru"),
        Some(AurHelper::Yay) => println!("   AUR helper: yay"),
        _ => println!("   AUR helper: (none)"),
    }

    print_group(
        "Required (pacman):",
        &report.entries,
        DepKind::Pacman,
        tty,
        verbose,
    );
    print_group(
        "Required (AUR):",
        &report.entries,
        DepKind::Aur,
        tty,
        verbose,
    );
    print_group(
        "Optional (pacman):",
        &report.entries,
        DepKind::OptionalPacman,
        tty,
        verbose,
    );
    print_group(
        "Implicit (mentioned in configs, not declared):",
        &report.entries,
        DepKind::ImplicitFromConfig,
        tty,
        verbose,
    );

    if !report.warnings.is_empty() {
        println!("Warnings:");
        for w in &report.warnings {
            println!("  {}", format_warning(w));
        }
        println!();
    }

    let req_missing = report
        .entries
        .iter()
        .filter(|e| {
            matches!(e.kind, DepKind::Pacman | DepKind::Aur)
                && matches!(e.status, DepStatus::Missing)
        })
        .count();
    let opt_missing = report
        .entries
        .iter()
        .filter(|e| {
            matches!(e.kind, DepKind::OptionalPacman) && matches!(e.status, DepStatus::Missing)
        })
        .count();
    let imp_missing = report
        .entries
        .iter()
        .filter(|e| {
            matches!(e.kind, DepKind::ImplicitFromConfig)
                && matches!(
                    e.status,
                    DepStatus::Missing | DepStatus::UnknownBinary { .. }
                )
        })
        .count();

    println!(
        "Summary: {req_missing} required missing, {opt_missing} optional missing, {imp_missing} implicit missing"
    );

    let code = {
        let base = report.exit_code();
        if strict && base < 3 && imp_missing > 0 {
            1.max(base)
        } else {
            base
        }
    };
    println!("Exit: {code}");
}

fn print_group(
    header: &str,
    entries: &[archdots_core::validator::DepEntry],
    kind: DepKind,
    tty: bool,
    verbose: bool,
) {
    let group: Vec<_> = entries.iter().filter(|e| e.kind == kind).collect();
    if group.is_empty() {
        return;
    }
    println!();
    println!("{header}");
    for entry in &group {
        let icon = status_icon(&entry.status, tty);
        print!("  {icon} {}", entry.name);

        if let DepSource::InferredFromConfig { mentions } = &entry.source {
            if mentions.is_empty() {
                println!();
            } else {
                let shown = if verbose {
                    mentions.len()
                } else {
                    mentions.len().min(3)
                };
                let first: Vec<String> = mentions[..shown]
                    .iter()
                    .map(|m| format!("{}:{}", m.file.display(), m.line))
                    .collect();
                let suffix = if verbose || mentions.len() <= 3 {
                    String::new()
                } else {
                    format!(" … ({} more)", mentions.len() - 3)
                };
                println!("  [{}{}]", first.join(", "), suffix);
            }
        } else {
            println!();
        }
    }
}

fn status_icon(status: &DepStatus, tty: bool) -> String {
    if tty {
        match status {
            DepStatus::Installed => "✓".green().to_string(),
            DepStatus::Missing => "✗".red().to_string(),
            DepStatus::UnknownBinary { .. } => "?".yellow().to_string(),
            DepStatus::Indeterminate { .. } | _ => "!".cyan().to_string(),
        }
    } else {
        match status {
            DepStatus::Installed => "✓".to_string(),
            DepStatus::Missing => "✗".to_string(),
            DepStatus::UnknownBinary { .. } => "?".to_string(),
            DepStatus::Indeterminate { .. } | _ => "!".to_string(),
        }
    }
}

fn format_warning(w: &ValidationWarning) -> String {
    match w {
        ValidationWarning::DeclaredButUnused { name, kind } => {
            format!("declared but unused: {name} ({kind:?})")
        }
        ValidationWarning::AurDepsButNoHelper { aur_deps } => {
            format!(
                "AUR deps declared but no AUR helper installed: {}",
                aur_deps.join(", ")
            )
        }
        ValidationWarning::PacmanFilesDbNotSynced => {
            "pacman file DB not synced — run `sudo pacman -Fy` then retry with --deep".to_string()
        }
        ValidationWarning::UnresolvedVariable { var, line, file } => {
            format!(
                "unresolved variable ${var} at {}:{line} (declare the dep manually)",
                file.display()
            )
        }
        ValidationWarning::ConfigUnreadable { path, reason } => {
            format!("config unreadable: {} ({reason})", path.display())
        }
        ValidationWarning::PacmanDatabaseLocked => {
            "pacman database is locked — close any pacman instance and retry".to_string()
        }
        _ => format!("{w:?}"),
    }
}

// ── JSON renderer ─────────────────────────────────────────────────────────────

fn render_json(report: &ValidationReport, strict: bool) -> Result<()> {
    let mut value = serde_json::to_value(report)?;

    let code = {
        let base = report.exit_code();
        let imp_missing = report.entries.iter().any(|e| {
            matches!(e.kind, DepKind::ImplicitFromConfig)
                && matches!(
                    e.status,
                    DepStatus::Missing | DepStatus::UnknownBinary { .. }
                )
        });
        if strict && base < 3 && imp_missing {
            if base < 1 || base == 2 {
                1
            } else {
                base
            }
        } else {
            base
        }
    };

    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "exit_code".to_string(),
            serde_json::Value::Number(code.into()),
        );
    }

    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
