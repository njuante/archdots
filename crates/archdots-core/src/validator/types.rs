//! Public types for the dependency validator.

use std::path::PathBuf;

use serde::Serialize;
use thiserror::Error;

use crate::error::{PackageError, ProfileError};
use crate::packages::AurHelper;
use crate::parsers::MentionSource;

/// A mention of a binary in a config file, augmented with the source file path.
///
/// Parsers emit raw [`crate::parsers::Mention`] values; the validator wraps
/// them in `LocatedMention` to carry the file path through to JSON output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocatedMention {
    /// Binary name (basename only, no arguments).
    pub binary: String,
    /// 1-based line number in the source file.
    pub line: u32,
    /// Syntactic context in which the mention was found.
    #[serde(rename = "kind")]
    pub source: MentionSource,
    /// Absolute path to the source config file.
    pub file: PathBuf,
}

/// Aggregated validation result for a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationReport {
    /// Schema version of this JSON output format (currently `1`).
    pub schema_version: u32,
    /// Name of the validated profile.
    #[serde(rename = "profile")]
    pub profile_name: String,
    /// Detected AUR helper, or `None` if neither paru nor yay is installed.
    pub aur_helper: Option<AurHelper>,
    /// One entry per unique dependency (declared or inferred).
    pub entries: Vec<DepEntry>,
    /// Informational warnings that do not map to a specific entry.
    pub warnings: Vec<ValidationWarning>,
}

/// One resolved dependency — declared in the profile or inferred from configs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DepEntry {
    /// Package name (best-effort for [`DepKind::ImplicitFromConfig`]).
    pub name: String,
    /// Where this dependency originates.
    pub kind: DepKind,
    /// Installation state of the dependency.
    #[serde(flatten)]
    pub status: DepStatus,
    /// Whether this entry was declared in the profile or inferred.
    #[serde(flatten)]
    pub source: DepSource,
}

/// Classification of a dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum DepKind {
    /// Listed in `[dependencies.pacman]`.
    Pacman,
    /// Listed in `[dependencies.aur]`.
    Aur,
    /// Listed in `[dependencies.optional_pacman]`.
    OptionalPacman,
    /// Inferred from a `[[files]]` config file; not in the profile dependencies.
    ImplicitFromConfig,
}

/// Installation state of a dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DepStatus {
    /// Package is currently installed (`pacman -Q` hit).
    Installed,
    /// Package is not installed.
    Missing,
    /// Binary was mentioned but no package provider could be determined.
    #[serde(rename = "unknown_binary")]
    UnknownBinary {
        /// The unresolved binary name.
        binary: String,
    },
    /// Could not determine: pacman failed, db locked, or similar.
    Indeterminate {
        /// Human-readable reason.
        reason: String,
    },
}

/// Whether a dependency entry was declared or inferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum DepSource {
    /// Declared explicitly in `[dependencies]`.
    #[serde(rename = "declared")]
    DeclaredInProfile,
    /// Inferred by parsing one or more `[[files]]` config entries.
    #[serde(rename = "inferred")]
    InferredFromConfig {
        /// All occurrences of this binary across the profile's config files.
        mentions: Vec<LocatedMention>,
    },
}

/// Informational validation warning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ValidationWarning {
    /// A declared dependency was not mentioned in any parsed config file.
    DeclaredButUnused {
        /// Package name.
        name: String,
        /// Dep kind.
        kind: DepKind,
    },
    /// AUR packages are declared but no AUR helper is installed.
    AurDepsButNoHelper {
        /// Names of the AUR packages.
        aur_deps: Vec<String>,
    },
    /// `--deep` was requested but the pacman file database is not synced.
    ///
    /// Run `sudo pacman -Fy` to sync, then retry with `--deep`.
    PacmanFilesDbNotSynced,
    /// A `$VAR` reference was found in a config file that the parser skipped.
    UnresolvedVariable {
        /// Variable name without the `$` sigil.
        var: String,
        /// 1-based line number.
        line: u32,
        /// Source config file path.
        file: PathBuf,
    },
    /// A config file listed in `[[files]]` could not be read.
    ConfigUnreadable {
        /// Path that triggered the error.
        path: PathBuf,
        /// Human-readable reason.
        reason: String,
    },
    /// `/var/lib/pacman/db.lck` was present during the validation run.
    PacmanDatabaseLocked,
}

/// Options controlling validator behaviour.
#[derive(Debug, Clone, Copy, Default)]
pub struct ValidatorOptions {
    /// When `true`, treat implicit-missing as required-missing for exit-code purposes.
    pub strict: bool,
    /// When `true`, fall back to `pacman -F` for binaries not in the curated table.
    pub deep: bool,
}

/// Errors that can arise from [`crate::validator::Validator::validate`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ValidatorError {
    /// Package-manager error propagated from [`crate::packages::PackageDB`].
    #[error(transparent)]
    Package(#[from] PackageError),
    /// Profile error (e.g. unresolvable `$VAR` in a target path).
    #[error(transparent)]
    Profile(#[from] ProfileError),
    /// Filesystem I/O error not covered by [`ValidationWarning::ConfigUnreadable`].
    #[error("I/O error at {path}: {source}")]
    Io {
        /// Path that triggered the error.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
}

impl ValidationReport {
    /// Compute the process exit code for this report (without `--strict`).
    ///
    /// | Code | Meaning |
    /// |------|---------|
    /// | 0 | All required deps installed; nothing indeterminate. |
    /// | 1 | At least one declared required (pacman/aur) dep is missing. |
    /// | 2 | At least one optional or implicit dep is missing/unknown. |
    /// | 3 | Any dep is indeterminate (pacman failed, db locked, etc.). |
    ///
    /// Precedence: `3 > 1 > 2 > 0`.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if self
            .entries
            .iter()
            .any(|e| matches!(e.status, DepStatus::Indeterminate { .. }))
        {
            return 3;
        }
        if self.entries.iter().any(|e| {
            matches!(e.kind, DepKind::Pacman | DepKind::Aur)
                && matches!(e.status, DepStatus::Missing)
        }) {
            return 1;
        }
        if self.entries.iter().any(|e| {
            matches!(
                e.kind,
                DepKind::OptionalPacman | DepKind::ImplicitFromConfig
            ) && matches!(
                e.status,
                DepStatus::Missing | DepStatus::UnknownBinary { .. }
            )
        }) {
            return 2;
        }
        0
    }

    /// Returns `true` when at least one declared required dep (pacman or aur) is missing.
    #[must_use]
    pub fn has_critical_missing(&self) -> bool {
        self.entries.iter().any(|e| {
            matches!(e.kind, DepKind::Pacman | DepKind::Aur)
                && matches!(e.status, DepStatus::Missing)
        })
    }
}
