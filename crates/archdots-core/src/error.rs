//! Typed errors for archdots-core using `thiserror`.

use std::path::PathBuf;

use thiserror::Error;

/// Top-level error type for all core operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CoreError {
    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Profile loading, saving, or validation error.
    #[error(transparent)]
    Profile(#[from] ProfileError),

    /// Detector catalog parse error.
    #[error(transparent)]
    Detector(#[from] DetectorError),
}

/// Errors that can occur when initialising the dotfile [`Detector`][crate::detector::Detector].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DetectorError {
    /// The embedded `known_dotfiles.toml` catalog could not be parsed.
    ///
    /// This should never happen in a release build; it indicates a
    /// programming error in the catalog file.
    #[error("failed to parse embedded dotfile catalog: {0}")]
    ParseCatalog(#[from] toml::de::Error),
}

/// Errors specific to profile loading, saving, and validation.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProfileError {
    /// Filesystem I/O error while reading or writing a profile file.
    #[error("I/O error at {path}: {source}")]
    Io {
        /// Path that triggered the error.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// The TOML content could not be parsed.
    #[error("TOML parse error: {0}")]
    ParseToml(#[from] toml::de::Error),

    /// The in-memory profile could not be serialized to TOML.
    #[error("TOML serialize error: {0}")]
    SerializeToml(#[from] toml::ser::Error),

    /// The `schema_version` field does not match the supported version.
    #[error("unsupported schema_version {found}, expected {expected}")]
    UnsupportedSchema {
        /// Version found in the file.
        found: u32,
        /// Version this build supports.
        expected: u32,
    },

    /// The profile `name` field contains invalid characters.
    ///
    /// Valid names match `[a-z0-9_-]+`.
    #[error("invalid profile name `{0}` (must match [a-z0-9_-]+)")]
    InvalidName(String),

    /// Two [`FileEntry`][crate::profile::FileEntry] items share the same `id`.
    #[error("duplicate file id `{0}`")]
    DuplicateFileId(String),

    /// A required path field (`source` or `target`) is empty.
    #[error("file entry `{id}` has empty {field}")]
    EmptyPath {
        /// Id of the offending entry.
        id: String,
        /// Which field is empty (`"source"`, `"target"`, or `"id"`).
        field: &'static str,
    },

    /// A `source` path would escape the profile directory via `..` components.
    #[error("source path `{0}` escapes the profile directory")]
    SourceEscape(PathBuf),

    /// A `target` string is not absolute after `~` / `$VAR` expansion.
    #[error("target `{0}` is not absolute after expansion")]
    NonAbsoluteTarget(String),

    /// A `$VAR` reference in a target string could not be resolved.
    #[error("unknown env var `${name}` in target `{target}`")]
    UnknownEnvVar {
        /// Variable name (without the `$` sigil).
        name: String,
        /// The full unexpanded target string for context.
        target: String,
    },
}
