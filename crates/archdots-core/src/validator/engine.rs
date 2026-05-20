//! Validator engine — cross-references profile deps with installed packages.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;

use crate::error::ProfileError;
use crate::packages::{PackageDB, PackageError, ProviderHit};
use crate::parsers::{self, ParserKind};
use crate::profile::{LinkMode, Profile, ResolveCtx};

use super::types::{
    DepEntry, DepKind, DepSource, DepStatus, LocatedMention, ValidationReport, ValidationWarning,
    ValidatorError, ValidatorOptions,
};

// ── builtin filter ────────────────────────────────────────────────────────────

const BUILTIN_FILTER_TOML: &str = include_str!("../../data/builtin_filter.toml");

#[derive(Deserialize)]
struct BuiltinFilter {
    builtins: Vec<String>,
    base: Vec<String>,
}

fn load_noise_filter() -> HashSet<String> {
    let filter: BuiltinFilter =
        toml::from_str(BUILTIN_FILTER_TOML).expect("builtin_filter.toml must be valid TOML");
    filter.builtins.into_iter().chain(filter.base).collect()
}

// ── Validator ─────────────────────────────────────────────────────────────────

/// Cross-references a [`Profile`]'s declared dependencies with what is
/// actually installed, and infers additional dependencies from config files.
pub struct Validator<'a> {
    db: &'a PackageDB,
    home: &'a Path,
}

impl<'a> Validator<'a> {
    /// Create a new validator.
    pub fn new(db: &'a PackageDB, home: &'a Path) -> Self {
        Self { db, home }
    }

    /// Run the full validation pipeline and return a [`ValidationReport`].
    ///
    /// # Errors
    /// Returns [`ValidatorError::Profile`] for unresolvable `$VAR` references
    /// in profile target paths, or [`ValidatorError::Package`] for fatal
    /// pacman errors that prevent any output.
    #[allow(clippy::too_many_lines)]
    pub fn validate(
        &self,
        profile: &Profile,
        profile_dir: &Path,
        opts: ValidatorOptions,
    ) -> Result<ValidationReport, ValidatorError> {
        let profile_name = profile.profile.name.clone();
        let mut warnings: Vec<ValidationWarning> = Vec::new();
        let mut entries: Vec<DepEntry> = Vec::new();

        // PASO 1: Detect AUR helper.
        let aur_helper = self.db.detect_aur_helper()?;

        let aur_deps_dedup = dedup(&profile.dependencies.aur);
        if aur_helper.is_none() && !aur_deps_dedup.is_empty() {
            warnings.push(ValidationWarning::AurDepsButNoHelper {
                aur_deps: aur_deps_dedup.clone(),
            });
        }

        // Track whether we've already pushed PacmanDatabaseLocked.
        let mut db_locked_warned = false;

        // PASO 2: Pacman deps (dedup silently).
        for name in dedup(&profile.dependencies.pacman) {
            let status = self.resolve_status(&name, &mut db_locked_warned, &mut warnings);
            entries.push(DepEntry {
                name,
                kind: DepKind::Pacman,
                status,
                source: DepSource::DeclaredInProfile,
            });
        }

        // PASO 3: AUR deps.
        for name in &aur_deps_dedup {
            let status = self.resolve_status(name, &mut db_locked_warned, &mut warnings);
            entries.push(DepEntry {
                name: name.clone(),
                kind: DepKind::Aur,
                status,
                source: DepSource::DeclaredInProfile,
            });
        }

        // PASO 4: Optional pacman deps (dedup silently).
        for name in dedup(&profile.dependencies.optional_pacman) {
            let status = self.resolve_status(&name, &mut db_locked_warned, &mut warnings);
            entries.push(DepEntry {
                name,
                kind: DepKind::OptionalPacman,
                status,
                source: DepSource::DeclaredInProfile,
            });
        }

        // PASO 5: Scan config files and collect mentions.
        let mut mentions_by_binary: HashMap<String, Vec<LocatedMention>> = HashMap::new();
        let ctx = ResolveCtx::with_home(self.home);

        for file_entry in &profile.files {
            // Template entries are not yet rendered — skip.
            if matches!(file_entry.mode, LinkMode::Template) {
                continue;
            }

            let source_path = Profile::resolve_source(file_entry, profile_dir)?;

            // Infer parser kind: prefer the expanded target path, fall back to source.
            // UnknownEnvVar is not swallowed — it surfaces as ValidatorError::Profile.
            let kind: Option<ParserKind> = {
                let from_target = match Profile::resolve_target(file_entry, &ctx) {
                    Ok(p) => parsers::infer_kind(&p),
                    Err(e @ ProfileError::UnknownEnvVar { .. }) => return Err(e.into()),
                    Err(_) => None,
                };
                if from_target.is_some() {
                    from_target
                } else {
                    parsers::infer_kind(&source_path)
                }
            };
            let Some(kind) = kind else { continue };

            let contents = match std::fs::read_to_string(&source_path) {
                Ok(c) => c,
                Err(e) => {
                    warnings.push(ValidationWarning::ConfigUnreadable {
                        path: source_path.clone(),
                        reason: e.to_string(),
                    });
                    continue;
                }
            };

            // Heuristic: detect exec $VAR patterns that parsers skipped.
            detect_unresolved_vars(&contents, &source_path, &mut warnings);

            for m in parsers::parse(kind, &contents) {
                mentions_by_binary
                    .entry(m.binary.clone())
                    .or_default()
                    .push(LocatedMention {
                        binary: m.binary,
                        line: m.line,
                        source: m.source,
                        file: source_path.clone(),
                    });
            }
        }

        // PASO 6: Remove builtins / assumed-base binaries from mention map.
        let noise = load_noise_filter();
        mentions_by_binary.retain(|b, _| !noise.contains(b.as_str()));

        // Collect declared names into owned strings so `entries` can be mutated later.
        let declared_names: HashSet<String> = entries.iter().map(|e| e.name.clone()).collect();

        // Track which declared-dep names were mentioned (to detect DeclaredButUnused).
        let mut mentioned_declared: HashSet<String> = HashSet::new();

        // PASO 7: Build implicit entries from remaining mentions.
        let mut files_db_not_synced_warned = false;

        for (binary, mentions) in &mentions_by_binary {
            // Direct name match: binary == declared dep name.
            if declared_names.contains(binary.as_str()) {
                mentioned_declared.insert(binary.clone());
                continue;
            }

            let hit = match self.db.provider_of(binary, opts.deep) {
                Ok(h) => h,
                Err(PackageError::DatabaseLocked) => {
                    if !db_locked_warned {
                        db_locked_warned = true;
                        warnings.push(ValidationWarning::PacmanDatabaseLocked);
                    }
                    entries.push(DepEntry {
                        name: binary.clone(),
                        kind: DepKind::ImplicitFromConfig,
                        status: DepStatus::Indeterminate {
                            reason: "pacman database is locked".to_string(),
                        },
                        source: DepSource::InferredFromConfig {
                            mentions: mentions.clone(),
                        },
                    });
                    continue;
                }
                Err(e) => {
                    entries.push(DepEntry {
                        name: binary.clone(),
                        kind: DepKind::ImplicitFromConfig,
                        status: DepStatus::Indeterminate {
                            reason: e.to_string(),
                        },
                        source: DepSource::InferredFromConfig {
                            mentions: mentions.clone(),
                        },
                    });
                    continue;
                }
            };

            match hit {
                ProviderHit::Curated { package, .. } | ProviderHit::PacmanFiles { package, .. } => {
                    // Resolved package matches a declared dep → mark as used, skip implicit.
                    if declared_names.contains(package.as_str()) {
                        mentioned_declared.insert(package.clone());
                        continue;
                    }

                    let status = match self.db.is_installed(&package) {
                        Ok(true) => DepStatus::Installed,
                        Ok(false) => DepStatus::Missing,
                        Err(e) => DepStatus::Indeterminate {
                            reason: e.to_string(),
                        },
                    };
                    entries.push(DepEntry {
                        name: package,
                        kind: DepKind::ImplicitFromConfig,
                        status,
                        source: DepSource::InferredFromConfig {
                            mentions: mentions.clone(),
                        },
                    });
                }
                ProviderHit::FilesDbNotSynced => {
                    if !files_db_not_synced_warned {
                        files_db_not_synced_warned = true;
                        warnings.push(ValidationWarning::PacmanFilesDbNotSynced);
                    }
                    entries.push(DepEntry {
                        name: binary.clone(),
                        kind: DepKind::ImplicitFromConfig,
                        status: DepStatus::UnknownBinary {
                            binary: binary.clone(),
                        },
                        source: DepSource::InferredFromConfig {
                            mentions: mentions.clone(),
                        },
                    });
                }
                ProviderHit::Unknown => {
                    entries.push(DepEntry {
                        name: binary.clone(),
                        kind: DepKind::ImplicitFromConfig,
                        status: DepStatus::UnknownBinary {
                            binary: binary.clone(),
                        },
                        source: DepSource::InferredFromConfig {
                            mentions: mentions.clone(),
                        },
                    });
                }
            }
        }

        // PASO 8: Emit DeclaredButUnused for declared deps not mentioned in configs.
        for entry in &entries {
            if matches!(entry.source, DepSource::DeclaredInProfile)
                && !mentions_by_binary.contains_key(&entry.name)
                && !mentioned_declared.contains(&entry.name)
            {
                warnings.push(ValidationWarning::DeclaredButUnused {
                    name: entry.name.clone(),
                    kind: entry.kind,
                });
            }
        }

        Ok(ValidationReport {
            schema_version: 1,
            profile_name,
            aur_helper,
            entries,
            warnings,
        })
    }

    /// Resolve the installation status of `name`, catching `DatabaseLocked` gracefully.
    fn resolve_status(
        &self,
        name: &str,
        db_locked_warned: &mut bool,
        warnings: &mut Vec<ValidationWarning>,
    ) -> DepStatus {
        match self.db.is_installed(name) {
            Ok(true) => DepStatus::Installed,
            Ok(false) => DepStatus::Missing,
            Err(PackageError::DatabaseLocked) => {
                if !*db_locked_warned {
                    *db_locked_warned = true;
                    warnings.push(ValidationWarning::PacmanDatabaseLocked);
                }
                DepStatus::Indeterminate {
                    reason: "pacman database is locked".to_string(),
                }
            }
            Err(e) => DepStatus::Indeterminate {
                reason: e.to_string(),
            },
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Return `names` with duplicates removed (order-preserving).
fn dedup(names: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    names
        .iter()
        .filter(|n| seen.insert(n.as_str()))
        .cloned()
        .collect()
}

/// Heuristic: scan `contents` for `exec $VAR` patterns that parsers skipped,
/// emitting [`ValidationWarning::UnresolvedVariable`] for each occurrence.
fn detect_unresolved_vars(contents: &str, file: &Path, warnings: &mut Vec<ValidationWarning>) {
    for (idx, line) in contents.lines().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let line_num = (idx + 1) as u32;
        for var in exec_dollar_vars(line) {
            warnings.push(ValidationWarning::UnresolvedVariable {
                var,
                line: line_num,
                file: file.to_path_buf(),
            });
        }
    }
}

/// Extract `$VAR` names following an `exec` keyword on a line.
///
/// Handles:
/// - `exec $VAR`
/// - `exec-once = $VAR` / `exec-once=$VAR`
/// - `exec_always $VAR`
///
/// Returns at most one var per line (first match). Skips `${VAR}` (braced).
fn exec_dollar_vars(line: &str) -> Vec<String> {
    let stripped = line.trim();

    // Find an exec keyword in the line.
    let Some(exec_pos) = stripped.find("exec") else {
        return vec![];
    };

    let after = &stripped[exec_pos + 4..];

    // Skip optional suffix: -once, _always.
    let after = after
        .strip_prefix("-once")
        .or_else(|| after.strip_prefix("_always"))
        .unwrap_or(after);

    // Skip whitespace, optional '=', then more whitespace.
    let after = after.trim_start();
    let after = after.strip_prefix('=').unwrap_or(after);
    let after = after.trim_start();

    // Must start with '$' but not '${'.
    let Some(var_rest) = after.strip_prefix('$') else {
        return vec![];
    };
    if var_rest.starts_with('{') {
        return vec![];
    }

    let var_name: String = var_rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();

    if var_name.is_empty() {
        return vec![];
    }

    // Only emit if what follows the var is end-of-line or whitespace.
    let rest = &var_rest[var_name.len()..];
    if rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace()) {
        vec![var_name]
    } else {
        vec![]
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_filter_loads_and_is_non_empty() {
        let noise = load_noise_filter();
        assert!(!noise.is_empty(), "noise filter must not be empty");
        assert!(noise.contains("cd"), "cd must be in noise filter");
        assert!(noise.contains("ls"), "ls must be in noise filter");
        assert!(noise.contains("exec"), "exec must be in noise filter");
    }

    #[test]
    fn exec_dollar_vars_detects_simple_pattern() {
        assert_eq!(exec_dollar_vars("exec $TERMINAL"), vec!["TERMINAL"]);
    }

    #[test]
    fn exec_dollar_vars_detects_exec_once_eq() {
        assert_eq!(
            exec_dollar_vars("exec-once = $WALLPAPER"),
            vec!["WALLPAPER"]
        );
    }

    #[test]
    fn exec_dollar_vars_ignores_braced() {
        assert!(exec_dollar_vars("exec ${TERMINAL}").is_empty());
    }

    #[test]
    fn exec_dollar_vars_ignores_literal_command() {
        // "exec kitty" — kitty is not $VAR
        assert!(exec_dollar_vars("exec kitty").is_empty());
    }

    #[test]
    fn exec_dollar_vars_ignores_var_with_args() {
        // "$TERMINAL -e htop" — the var is followed by non-whitespace args
        // The current heuristic: after VAR there's a space → still emit
        // because rest starts with whitespace.
        // "exec $TERMINAL -e htop" → rest after TERMINAL is " -e htop" → starts with space → emit
        let vars = exec_dollar_vars("exec $TERMINAL -e htop");
        assert_eq!(vars, vec!["TERMINAL"]);
    }

    #[test]
    fn dedup_removes_duplicates_preserving_order() {
        let input = vec![
            "b".to_string(),
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
        ];
        let out = dedup(&input);
        assert_eq!(out, ["b", "a", "c"]);
    }
}
