//! Typed errors for archdots-core using `thiserror`.

use std::path::PathBuf;

use thiserror::Error;

use crate::{journal::JournalId, linker::ConflictReason, snapshot::SnapshotId};

/// Top-level error type for all core operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CoreError {
    /// Package manager / `pacman` error.
    #[error(transparent)]
    Package(#[from] PackageError),
    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Profile loading, saving, or validation error.
    #[error(transparent)]
    Profile(#[from] ProfileError),

    /// Detector catalog parse error.
    #[error(transparent)]
    Detector(#[from] DetectorError),

    /// Snapshot error.
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),

    /// Journal error.
    #[error(transparent)]
    Journal(#[from] JournalError),

    /// Lock error.
    #[error(transparent)]
    Lock(#[from] LockError),

    /// Linker error.
    #[error(transparent)]
    Linker(#[from] LinkerError),

    /// A path is not valid UTF-8.
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
}

/// Errors specific to the [`Linker`][crate::linker::Linker].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LinkerError {
    /// The plan contains one or more [`ConflictReason`] entries and `force`
    /// was not set.
    #[error("plan has {} conflicts; rerun with --force to override", conflicts.len())]
    ConflictsDetected {
        /// `(target, reason)` for each conflicting item.
        conflicts: Vec<(PathBuf, ConflictReason)>,
    },

    /// A previous in-progress transaction for the same profile is still open;
    /// the user must run `archdots recover` before retrying.
    #[error("orphaned in-progress transaction {id} — run `archdots recover`")]
    OrphanedTransaction {
        /// Id of the orphan journal entry.
        id: JournalId,
    },

    /// Rollback was requested but no prior `Success` transaction exists for
    /// the given profile.
    #[error("no successful transaction to roll back for profile `{profile}`")]
    NoTransactionToRollback {
        /// Profile name that was queried.
        profile: String,
    },

    /// Creation of a single symlink failed mid-apply; triggers full rollback.
    #[error("link failed: {source_path} → {target}: {reason}")]
    LinkFailed {
        /// Source path inside the dotfile repo.
        source_path: PathBuf,
        /// Filesystem target that could not be linked.
        target: PathBuf,
        /// Human-readable reason from the underlying I/O error.
        reason: String,
    },

    /// Defence-in-depth: a resolved source path escaped the dotfile repo.
    /// Should never trigger because profile validation runs earlier.
    #[error("source path escaped the dotfile repository")]
    SourceOutsideRepo,
}

/// Errors that can occur in snapshot operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SnapshotError {
    /// I/O error associated with a specific path.
    #[error("I/O error at {path}: {source}")]
    Io {
        /// Path that triggered the error.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// The tarball archive is corrupt or unreadable.
    #[error("snapshot {0} tarball is corrupt")]
    TarballCorrupt(SnapshotId),

    /// The sidecar manifest hash does not match the internal manifest hash.
    #[error("manifest mismatch: sidecar hash {sidecar_hash} != internal hash {internal_hash}")]
    ManifestMismatch {
        /// Hash from the sidecar manifest.
        sidecar_hash: String,
        /// Hash from the embedded manifest inside the tarball.
        internal_hash: String,
    },

    /// A target file's sha256 does not match what the manifest records.
    #[error("checksum mismatch for {path}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// The expected sha256 hex string from the manifest.
        expected: String,
        /// The actual sha256 hex string computed from the file.
        actual: String,
        /// Path of the file that failed verification.
        path: PathBuf,
    },

    /// The requested snapshot does not exist.
    #[error("snapshot not found: {0}")]
    NotFound(SnapshotId),

    /// The archive has an invalid structure or unsupported format.
    #[error("invalid archive: {0}")]
    InvalidArchive(String),

    /// JSON serialization/deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
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

/// Errors that can occur in journal operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum JournalError {
    /// I/O error associated with a specific path.
    #[error("I/O error at {path}: {source}")]
    Io {
        /// Path that triggered the error.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// A JSONL line could not be deserialized.
    #[error("failed to deserialize journal line {line_number}: {source}")]
    DeserializeLine {
        /// 1-based line number in the journal file.
        line_number: u64,
        /// Underlying JSON parse error.
        #[source]
        source: serde_json::Error,
    },

    /// A journal entry exceeds the maximum number of link records.
    #[error("too many links: found {found}, max is {max}")]
    TooManyLinks {
        /// Number of links found.
        found: usize,
        /// Maximum allowed.
        max: usize,
    },

    /// The `schema_version` in an entry is newer than this build supports.
    #[error("journal schema version {found} is newer than supported {supported}")]
    SchemaTooNew {
        /// Version found in the entry.
        found: u32,
        /// Maximum version this build supports.
        supported: u32,
    },

    /// A `PathBuf` field contains non-UTF-8 bytes and cannot be serialized.
    #[error("a path in the journal entry contains non-UTF-8 bytes: {0}")]
    NonUtf8Path(PathBuf),
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

/// Package manager errors for the `archdots check` pipeline.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PackageError {
    /// `pacman` not found on `PATH`; the host is not an Arch-based system.
    #[error("pacman not found on PATH; archdots check requires an Arch-based system")]
    PacmanMissing,

    /// `pacman` exited with a non-zero status code.
    #[error("pacman exited with status {code}: {stderr}")]
    PacmanFailed {
        /// Exit code returned by `pacman`.
        code: i32,
        /// Captured stderr output.
        stderr: String,
    },

    /// A line of `pacman` output could not be parsed.
    #[error("could not parse pacman output: {0}")]
    UnparseableOutput(String),

    /// `/var/lib/pacman/db.lck` exists; the database is locked.
    #[error("pacman database is locked (/var/lib/pacman/db.lck exists)")]
    DatabaseLocked,

    /// Failed to spawn a subprocess.
    #[error("failed to spawn subprocess: {0}")]
    Spawn(#[source] std::io::Error),

    /// The embedded `binary_providers.toml` table could not be parsed or
    /// failed validation (e.g. duplicate binary entries, empty package name).
    #[error("failed to parse curated binary\u{2192}package table: {0}")]
    CuratedTableParseError(String),
}

/// Errors that can occur when acquiring or releasing the apply lock.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LockError {
    /// I/O error associated with a specific path.
    #[error("I/O error at {path}: {source}")]
    Io {
        /// Path that triggered the error.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// The lock is currently held by another process.
    ///
    /// `pid` is `None` when the lockfile exists but contains no readable PID.
    #[error("lock is busy (held by pid {pid:?})")]
    Busy {
        /// PID of the lock holder, if readable from the lockfile.
        pid: Option<u32>,
    },

    /// `acquire_blocking` did not obtain the lock before the deadline.
    #[error("timed out waiting for apply lock")]
    Timeout,
}
