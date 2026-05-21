//! Snapshot management: gzip-compressed tarballs with JSON manifests.
//!
//! Layout:
//! ```text
//! $XDG_DATA_HOME/archdots/snapshots/
//! ├── <ULID>.tar.gz           # archive (manifest.json + payload/…)
//! └── <ULID>.manifest.json    # sidecar (derived; internal manifest wins)
//! ```

use std::{
    collections::HashMap,
    fmt,
    fmt::Write as FmtWrite,
    fs::{self, File},
    io::{self, BufReader, Read, Write},
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    time::Duration,
};

use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tracing::warn;
use ulid::Ulid;

use crate::error::{CoreError, SnapshotError};

// ─── SnapshotId ───────────────────────────────────────────────────────────────

/// Opaque, time-ordered snapshot identifier (ULID).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SnapshotId(Ulid);

impl SnapshotId {
    /// Generate a new unique `SnapshotId`.
    #[must_use]
    pub fn new() -> Self {
        Self(Ulid::new())
    }

    fn as_str(&self) -> String {
        self.0.to_string()
    }
}

impl Default for SnapshotId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for SnapshotId {
    type Err = ulid::DecodeError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

// ─── Manifest types ───────────────────────────────────────────────────────────

/// Kind of filesystem entry recorded in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TargetKind {
    /// A regular file.
    File,
    /// A directory (archived recursively).
    Dir,
    /// A symbolic link (recorded as-is, not followed).
    Symlink,
    /// The path did not exist at snapshot time.
    Absent,
}

/// One entry in the manifest's target list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetEntry {
    /// Path relative to `$HOME` (or absolute if the target is outside `$HOME`).
    pub path: String,
    /// Filesystem kind of this entry at snapshot time.
    pub kind: TargetKind,
    /// Symlink destination; present only when `kind == Symlink`.
    pub symlink_to: Option<String>,
    /// File size in bytes; `None` for non-files.
    pub size: Option<u64>,
    /// Unix mode bits.
    pub mode: Option<u32>,
    /// ISO 8601 modification time.
    pub mtime: Option<String>,
    /// Hex-encoded SHA-256 of file content; only for `kind == File`.
    pub sha256: Option<String>,
}

/// Host context captured at snapshot creation time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
    /// Machine hostname.
    pub hostname: String,
    /// Name of the user who ran `apply`.
    pub user: String,
    /// Absolute path to `$HOME`.
    pub home: String,
    /// `archdots-core` version string.
    pub archdots_version: String,
}

/// What caused this snapshot to be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SnapshotTrigger {
    /// Created immediately before an `apply` operation.
    PreApply,
    /// Created by an explicit user request.
    Manual,
    /// Created immediately before a `rollback` operation.
    PreRollback,
}

/// Authoritative snapshot manifest (stored inside the tarball and as a sidecar).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Always `1` for this version of the format.
    pub schema_version: u32,
    /// Snapshot identifier (ULID).
    pub id: SnapshotId,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
    /// Profile name this snapshot was taken for.
    pub profile: String,
    /// What triggered this snapshot.
    pub trigger: SnapshotTrigger,
    /// Host context at creation time.
    pub host: HostInfo,
    /// One entry per target path.
    pub targets: Vec<TargetEntry>,
    /// Hex-encoded SHA-256 of concatenated raw file bytes (streaming, computed during creation).
    pub payload_sha256: String,
    /// Compression algorithm; always `"gzip"`.
    pub compression: String,
    /// Archive format; always `"tar"`.
    pub format: String,
}

// ─── Public API types ─────────────────────────────────────────────────────────

/// A fully loaded snapshot: manifest and the path to its archive on disk.
pub struct Snapshot {
    /// Snapshot identifier.
    pub id: SnapshotId,
    /// Parsed manifest (authoritative).
    pub manifest: Manifest,
    /// Absolute path to the `.tar.gz` archive.
    pub archive_path: PathBuf,
}

/// Request to create a new snapshot.
#[derive(Clone, Copy)]
pub struct CreateRequest<'a> {
    /// Profile name being snapshotted.
    pub profile: &'a str,
    /// What triggered this snapshot.
    pub trigger: SnapshotTrigger,
    /// Absolute paths of every file/dir to capture.
    pub targets: &'a [PathBuf],
}

/// Options controlling how a `restore` is performed.
#[derive(Debug, Clone, Copy)]
pub struct RestoreOptions {
    /// If `true`, report what would change but write nothing to disk.
    pub dry_run: bool,
    /// If `false`, abort at the first error (all-or-nothing). If `true`, continue.
    pub continue_on_error: bool,
}

/// Report returned by a `restore` operation.
#[derive(Debug, Default)]
pub struct RestoreReport {
    /// Paths that were successfully restored.
    pub restored: Vec<PathBuf>,
    /// Paths skipped, with the reason (dry-run or error message).
    pub skipped: Vec<(PathBuf, String)>,
}

/// Lightweight snapshot info for listing (does not unpack the archive).
#[derive(Debug, Clone)]
pub struct SnapshotSummary {
    /// Snapshot identifier.
    pub id: SnapshotId,
    /// Profile name.
    pub profile: String,
    /// What triggered the snapshot.
    pub trigger: SnapshotTrigger,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
    /// Number of target entries in the manifest.
    pub target_count: usize,
}

/// Policy for pruning snapshots.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum PrunePolicy {
    /// Keep only the `n` most recent snapshots (across all profiles).
    KeepLast(usize),
    /// Remove all snapshots older than `duration`.
    OlderThan(Duration),
    /// Keep the `n` most recent snapshots **per profile**.
    KeepLastPerProfile(usize),
    /// Remove exactly one snapshot by its id. No-op when the id is not found.
    OnlyId(SnapshotId),
}

/// Report returned by a `prune` operation.
#[derive(Debug, Default)]
pub struct PruneReport {
    /// IDs of every snapshot that was removed.
    pub removed: Vec<SnapshotId>,
    /// Total bytes freed (sum of removed `.tar.gz` and `.manifest.json` sizes).
    pub freed_bytes: u64,
}

// ─── Internal type aliases ───────────────────────────────────────────────────

type PayloadCache = Vec<(usize, Vec<u8>)>;
type ExtractResult = (Vec<(PathBuf, TargetEntry)>, HashMap<String, Vec<u8>>);

// ─── SnapshotManager ──────────────────────────────────────────────────────────

/// Manages the snapshot directory: create, restore, list, get, and prune.
pub struct SnapshotManager {
    root: PathBuf,
}

impl SnapshotManager {
    const SCHEMA_VERSION: u32 = 1;

    /// Open (and create if needed) the snapshot directory at `data_home/archdots/snapshots`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError`] if the directory cannot be created.
    pub fn open(data_home: &Path) -> Result<Self, CoreError> {
        let root = data_home.join("archdots").join("snapshots");
        fs::create_dir_all(&root).map_err(|e| SnapshotError::Io {
            path: root.clone(),
            source: e,
        })?;
        Ok(Self { root })
    }

    /// Create a snapshot of the given absolute target paths.
    ///
    /// Write order (crash-safe):
    /// 1. tmp tarball → `fsync`
    /// 2. tmp sidecar → `fsync`
    /// 3. atomic rename: sidecar to final name
    /// 4. atomic rename: tarball to final name
    /// 5. `fsync` the snapshot directory
    ///
    /// # Errors
    ///
    /// Returns [`CoreError`] on I/O errors, non-UTF-8 paths, or JSON serialization failures.
    pub fn create(&self, req: CreateRequest<'_>) -> Result<Snapshot, CoreError> {
        let id = SnapshotId::new();
        let home = home_dir()?;

        let mut target_entries: Vec<TargetEntry> = Vec::with_capacity(req.targets.len());
        for abs_path in req.targets {
            target_entries.push(capture_target(abs_path, &home)?);
        }

        let (payload_sha256, file_cache) =
            compute_payload_hash_and_cache(&target_entries, req.targets)?;

        let manifest = Manifest {
            schema_version: Self::SCHEMA_VERSION,
            id: id.clone(),
            created_at: now_iso8601(),
            profile: req.profile.to_owned(),
            trigger: req.trigger,
            host: collect_host_info(&home),
            targets: target_entries,
            payload_sha256,
            compression: "gzip".to_owned(),
            format: "tar".to_owned(),
        };

        // Tarball temp file (same dir → same FS → atomic rename).
        let tmp_tar =
            tempfile::NamedTempFile::new_in(&self.root).map_err(|e| SnapshotError::Io {
                path: self.root.clone(),
                source: e,
            })?;
        emit_tarball(tmp_tar.path(), &manifest, &file_cache, &home)?;

        // Sidecar temp file.
        let tmp_sidecar =
            tempfile::NamedTempFile::new_in(&self.root).map_err(|e| SnapshotError::Io {
                path: self.root.clone(),
                source: e,
            })?;
        {
            let json = serde_json::to_vec_pretty(&manifest).map_err(SnapshotError::Json)?;
            let f = tmp_sidecar.as_file();
            let mut f = f;
            f.write_all(&json).map_err(|e| SnapshotError::Io {
                path: tmp_sidecar.path().to_owned(),
                source: e,
            })?;
            f.sync_all().map_err(|e| SnapshotError::Io {
                path: tmp_sidecar.path().to_owned(),
                source: e,
            })?;
        }

        // Atomic renames — sidecar first, then tarball.
        let sidecar_path = self.sidecar_path(&id);
        let archive_path = self.archive_path(&id);

        tmp_sidecar
            .persist(&sidecar_path)
            .map_err(|e| SnapshotError::Io {
                path: sidecar_path.clone(),
                source: e.error,
            })?;

        tmp_tar
            .persist(&archive_path)
            .map_err(|e| SnapshotError::Io {
                path: archive_path.clone(),
                source: e.error,
            })?;

        fsync_dir(&self.root)?;

        Ok(Snapshot {
            id,
            manifest,
            archive_path,
        })
    }

    /// Restore a snapshot.
    ///
    /// All checksums are verified **before** any file is written (all-or-nothing
    /// unless `opts.continue_on_error` is `true`). With `opts.dry_run = true`,
    /// no filesystem changes are made.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError`] if the snapshot is not found, the tarball is corrupt,
    /// checksums mismatch, or a file cannot be written.
    pub fn restore(
        &self,
        id: &SnapshotId,
        opts: RestoreOptions,
    ) -> Result<RestoreReport, CoreError> {
        let snapshot = self.get(id)?;
        let home = home_dir()?;

        let (verified_entries, file_data) = extract_and_verify(&snapshot)?;

        let mut report = RestoreReport::default();

        if opts.dry_run {
            for (abs_path, entry) in &verified_entries {
                let reason = match &entry.kind {
                    TargetKind::Absent => "dry_run: would remove",
                    _ => "dry_run: would restore",
                };
                report.skipped.push((abs_path.clone(), reason.to_owned()));
            }
            return Ok(report);
        }

        for (abs_path, entry) in &verified_entries {
            match apply_restore_entry(abs_path, entry, &home, &file_data) {
                Ok(()) => report.restored.push(abs_path.clone()),
                Err(e) => {
                    if opts.continue_on_error {
                        report.skipped.push((abs_path.clone(), e.to_string()));
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        Ok(report)
    }

    /// List all snapshots, ordered descending by creation time (most recent first).
    ///
    /// - Orphan sidecars (no matching tarball) → logged via `tracing` and skipped.
    /// - Orphan tarballs (no matching sidecar) → logged via `tracing`; the internal
    ///   manifest is read directly.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError`] if the snapshot directory cannot be read.
    pub fn list(&self) -> Result<Vec<SnapshotSummary>, CoreError> {
        let dir_entries = fs::read_dir(&self.root).map_err(|e| SnapshotError::Io {
            path: self.root.clone(),
            source: e,
        })?;

        let mut tarball_ids: Vec<SnapshotId> = Vec::new();
        let mut sidecar_ids: Vec<SnapshotId> = Vec::new();

        for entry in dir_entries {
            let entry = entry.map_err(|e| SnapshotError::Io {
                path: self.root.clone(),
                source: e,
            })?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if let Some(id_str) = name_str.strip_suffix(".tar.gz") {
                if let Ok(id) = id_str.parse::<SnapshotId>() {
                    tarball_ids.push(id);
                }
            } else if let Some(id_str) = name_str.strip_suffix(".manifest.json") {
                if let Ok(id) = id_str.parse::<SnapshotId>() {
                    sidecar_ids.push(id);
                }
            }
        }

        for sid in &sidecar_ids {
            if !tarball_ids.contains(sid) {
                warn!(id = %sid, "orphan sidecar (no matching tarball) — skipping");
            }
        }

        let mut summaries: Vec<SnapshotSummary> = Vec::new();
        for id in &tarball_ids {
            let manifest = if sidecar_ids.contains(id) {
                read_sidecar(&self.sidecar_path(id))?
            } else {
                warn!(id = %id, "orphan tarball (no sidecar) — reading internal manifest");
                read_internal_manifest(&self.archive_path(id), id)?
            };

            summaries.push(SnapshotSummary {
                id: id.clone(),
                profile: manifest.profile,
                trigger: manifest.trigger,
                created_at: manifest.created_at,
                target_count: manifest.targets.len(),
            });
        }

        summaries.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(summaries)
    }

    /// Load a single snapshot by id (reads the internal manifest from the tarball).
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Snapshot(SnapshotError::NotFound)`] if the id is unknown,
    /// or a tarball/JSON error if the archive is corrupt.
    pub fn get(&self, id: &SnapshotId) -> Result<Snapshot, CoreError> {
        let archive_path = self.archive_path(id);
        if !archive_path.exists() {
            return Err(SnapshotError::NotFound(id.clone()).into());
        }
        let manifest = read_internal_manifest(&archive_path, id)?;
        Ok(Snapshot {
            id: id.clone(),
            manifest,
            archive_path,
        })
    }

    /// Prune snapshots according to `policy`, deleting both the archive and its sidecar.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError`] if listing or deleting files fails.
    pub fn prune(&self, policy: PrunePolicy) -> Result<PruneReport, CoreError> {
        let summaries = self.list()?;

        let ids_to_remove: Vec<SnapshotId> = match policy {
            PrunePolicy::KeepLast(n) => summaries.into_iter().skip(n).map(|s| s.id).collect(),
            PrunePolicy::OnlyId(target_id) => {
                if summaries.iter().any(|s| s.id == target_id) {
                    vec![target_id]
                } else {
                    vec![]
                }
            }
            PrunePolicy::OlderThan(max_age) => {
                let now = std::time::SystemTime::now();
                summaries
                    .into_iter()
                    .filter(|s| match parse_iso8601(&s.created_at) {
                        Some(t) => now.duration_since(t).unwrap_or(Duration::ZERO) > max_age,
                        None => false,
                    })
                    .map(|s| s.id)
                    .collect()
            }
            PrunePolicy::KeepLastPerProfile(n) => {
                let mut per_profile: HashMap<String, Vec<SnapshotSummary>> = HashMap::new();
                for s in summaries {
                    per_profile.entry(s.profile.clone()).or_default().push(s);
                }
                per_profile
                    .into_values()
                    .flat_map(|group| group.into_iter().skip(n).map(|s| s.id).collect::<Vec<_>>())
                    .collect()
            }
        };

        let mut report = PruneReport::default();
        for id in &ids_to_remove {
            let archive = self.archive_path(id);
            let sidecar = self.sidecar_path(id);
            let mut freed = 0u64;
            if let Ok(meta) = fs::metadata(&archive) {
                freed += meta.len();
            }
            if let Ok(meta) = fs::metadata(&sidecar) {
                freed += meta.len();
            }
            if archive.exists() {
                fs::remove_file(&archive).map_err(|e| SnapshotError::Io {
                    path: archive.clone(),
                    source: e,
                })?;
            }
            if sidecar.exists() {
                fs::remove_file(&sidecar).map_err(|e| SnapshotError::Io {
                    path: sidecar.clone(),
                    source: e,
                })?;
            }
            report.freed_bytes += freed;
            report.removed.push(id.clone());
        }
        Ok(report)
    }

    fn archive_path(&self, id: &SnapshotId) -> PathBuf {
        self.root.join(format!("{}.tar.gz", id.as_str()))
    }

    fn sidecar_path(&self, id: &SnapshotId) -> PathBuf {
        self.root.join(format!("{}.manifest.json", id.as_str()))
    }
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

fn home_dir() -> Result<PathBuf, CoreError> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        SnapshotError::Io {
            path: PathBuf::from("$HOME"),
            source: io::Error::new(io::ErrorKind::NotFound, "$HOME not set"),
        }
        .into()
    })
}

fn now_iso8601() -> String {
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn gethostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map_or_else(|_| "unknown".to_owned(), |s| s.trim().to_owned())
}

fn collect_host_info(home: &Path) -> HostInfo {
    HostInfo {
        hostname: gethostname(),
        user: std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned()),
        home: home.to_string_lossy().into_owned(),
        archdots_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// Stat `abs_path` (without following symlinks) and build a `TargetEntry`.
fn capture_target(abs_path: &Path, home: &Path) -> Result<TargetEntry, CoreError> {
    let rel = rel_path(abs_path, home)?;

    match abs_path.symlink_metadata() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(TargetEntry {
            path: rel,
            kind: TargetKind::Absent,
            symlink_to: None,
            size: None,
            mode: None,
            mtime: None,
            sha256: None,
        }),
        Err(e) => Err(SnapshotError::Io {
            path: abs_path.to_owned(),
            source: e,
        }
        .into()),
        Ok(meta) => {
            use std::os::unix::fs::MetadataExt;
            let mode = meta.mode();
            let mtime = meta.modified().ok().and_then(|t| {
                let secs_u64 = t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
                let secs = i64::try_from(secs_u64).unwrap_or(i64::MAX);
                OffsetDateTime::from_unix_timestamp(secs)
                    .ok()?
                    .format(&time::format_description::well_known::Rfc3339)
                    .ok()
            });

            if meta.file_type().is_symlink() {
                let dest = fs::read_link(abs_path).map_err(|e| SnapshotError::Io {
                    path: abs_path.to_owned(),
                    source: e,
                })?;
                let dest_str = dest
                    .to_str()
                    .ok_or_else(|| CoreError::NonUtf8Path(dest.clone()))?
                    .to_owned();
                Ok(TargetEntry {
                    path: rel,
                    kind: TargetKind::Symlink,
                    symlink_to: Some(dest_str),
                    size: None,
                    mode: Some(mode),
                    mtime,
                    sha256: None,
                })
            } else if meta.is_dir() {
                Ok(TargetEntry {
                    path: rel,
                    kind: TargetKind::Dir,
                    symlink_to: None,
                    size: None,
                    mode: Some(mode),
                    mtime,
                    sha256: None,
                })
            } else {
                let sha256 = hash_file_hex(abs_path)?;
                Ok(TargetEntry {
                    path: rel,
                    kind: TargetKind::File,
                    symlink_to: None,
                    size: Some(meta.len()),
                    mode: Some(mode),
                    mtime,
                    sha256: Some(sha256),
                })
            }
        }
    }
}

/// Convert `abs_path` to a UTF-8 string, stripping the `home` prefix when possible.
fn rel_path(abs_path: &Path, home: &Path) -> Result<String, CoreError> {
    let p = abs_path.strip_prefix(home).unwrap_or(abs_path);
    p.to_str()
        .ok_or_else(|| CoreError::NonUtf8Path(abs_path.to_owned()))
        .map(str::to_owned)
}

/// Stream-read each `File` target to compute `payload_sha256`.
///
/// Returns `(hex_sha256, [(entry_index, bytes), …])`.
fn compute_payload_hash_and_cache(
    entries: &[TargetEntry],
    abs_paths: &[PathBuf],
) -> Result<(String, PayloadCache), CoreError> {
    let mut hasher = Sha256::new();
    let mut cache: PayloadCache = Vec::new();

    for (i, entry) in entries.iter().enumerate() {
        if entry.kind == TargetKind::File {
            let data = fs::read(&abs_paths[i]).map_err(|e| SnapshotError::Io {
                path: abs_paths[i].clone(),
                source: e,
            })?;
            hasher.update(&data);
            cache.push((i, data));
        }
    }

    Ok((hex_encode(hasher.finalize()), cache))
}

/// Write the tarball to `dest`.
///
/// Structure: `manifest.json` first, then `payload/<normalized_path>` per target.
fn emit_tarball(
    dest: &Path,
    manifest: &Manifest,
    file_cache: &PayloadCache,
    home: &Path,
) -> Result<(), CoreError> {
    let file = File::create(dest).map_err(|e| SnapshotError::Io {
        path: dest.to_owned(),
        source: e,
    })?;
    let enc = GzEncoder::new(file, Compression::default());
    let mut ar = tar::Builder::new(enc);

    // 1. manifest.json — always the first entry.
    let manifest_bytes = serde_json::to_vec_pretty(manifest).map_err(SnapshotError::Json)?;
    let mut hdr = tar::Header::new_gnu();
    hdr.set_size(manifest_bytes.len() as u64);
    hdr.set_mode(0o644);
    hdr.set_cksum();
    ar.append_data(&mut hdr, "manifest.json", manifest_bytes.as_slice())
        .map_err(|e| SnapshotError::Io {
            path: dest.to_owned(),
            source: e,
        })?;

    // 2. payload/* entries.
    let mut cache_iter = file_cache.iter().peekable();
    for (i, entry) in manifest.targets.iter().enumerate() {
        // Normalize: strip leading '/' so tar paths are always relative.
        let rel_for_tar = entry.path.trim_start_matches('/');
        let tar_path = format!("payload/{rel_for_tar}");

        match &entry.kind {
            TargetKind::Absent => {}
            TargetKind::Symlink => {
                let link_dest = entry.symlink_to.as_deref().unwrap_or("");
                let mut hdr = tar::Header::new_gnu();
                hdr.set_entry_type(tar::EntryType::Symlink);
                hdr.set_size(0);
                hdr.set_mode(entry.mode.unwrap_or(0o777));
                hdr.set_link_name(link_dest)
                    .map_err(|e| SnapshotError::Io {
                        path: dest.to_owned(),
                        source: e,
                    })?;
                hdr.set_cksum();
                ar.append_data(&mut hdr, &tar_path, &[] as &[u8])
                    .map_err(|e| SnapshotError::Io {
                        path: dest.to_owned(),
                        source: e,
                    })?;
            }
            TargetKind::Dir => {
                let abs = home.join(&entry.path);
                ar.append_dir_all(&tar_path, &abs)
                    .map_err(|e| SnapshotError::Io {
                        path: abs.clone(),
                        source: e,
                    })?;
            }
            TargetKind::File => {
                if let Some(&(cached_idx, ref data)) = cache_iter.peek() {
                    if *cached_idx == i {
                        cache_iter.next();
                        let mut hdr = tar::Header::new_gnu();
                        hdr.set_size(data.len() as u64);
                        hdr.set_mode(entry.mode.unwrap_or(0o644));
                        hdr.set_cksum();
                        ar.append_data(&mut hdr, &tar_path, data.as_slice())
                            .map_err(|e| SnapshotError::Io {
                                path: dest.to_owned(),
                                source: e,
                            })?;
                    }
                }
            }
        }
    }

    let enc = ar.into_inner().map_err(|e| SnapshotError::Io {
        path: dest.to_owned(),
        source: e,
    })?;
    let file = enc.finish().map_err(|e| SnapshotError::Io {
        path: dest.to_owned(),
        source: e,
    })?;
    file.sync_all().map_err(|e| SnapshotError::Io {
        path: dest.to_owned(),
        source: e,
    })?;
    Ok(())
}

fn fsync_dir(dir: &Path) -> Result<(), CoreError> {
    let f = File::open(dir).map_err(|e| SnapshotError::Io {
        path: dir.to_owned(),
        source: e,
    })?;
    f.sync_all().map_err(|e| SnapshotError::Io {
        path: dir.to_owned(),
        source: e,
    })?;
    Ok(())
}

fn read_sidecar(path: &Path) -> Result<Manifest, CoreError> {
    let data = fs::read(path).map_err(|e| SnapshotError::Io {
        path: path.to_owned(),
        source: e,
    })?;
    Ok(serde_json::from_slice(&data).map_err(SnapshotError::Json)?)
}

fn read_internal_manifest(archive_path: &Path, id: &SnapshotId) -> Result<Manifest, CoreError> {
    let file = File::open(archive_path).map_err(|e| SnapshotError::Io {
        path: archive_path.to_owned(),
        source: e,
    })?;
    let dec = GzDecoder::new(BufReader::new(file));
    let mut ar = tar::Archive::new(dec);

    for entry in ar
        .entries()
        .map_err(|_| SnapshotError::TarballCorrupt(id.clone()))?
    {
        let mut entry = entry.map_err(|_| SnapshotError::TarballCorrupt(id.clone()))?;
        let entry_path = entry
            .path()
            .map_err(|_| SnapshotError::TarballCorrupt(id.clone()))?
            .into_owned();

        if entry_path.to_str() == Some("manifest.json") {
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .map_err(|_| SnapshotError::TarballCorrupt(id.clone()))?;
            let m: Manifest = serde_json::from_slice(&buf)
                .map_err(|_| SnapshotError::TarballCorrupt(id.clone()))?;
            return Ok(m);
        }
    }

    Err(SnapshotError::InvalidArchive("manifest.json not found in archive".to_owned()).into())
}

/// Unpack all entries from the tarball and verify SHA-256 checksums.
///
/// All verification happens before any file is written so that restore
/// is all-or-nothing. Returns `(entries_with_abs_paths, file_data_map)`.
fn extract_and_verify(snapshot: &Snapshot) -> Result<ExtractResult, CoreError> {
    let home = home_dir()?;

    let file = File::open(&snapshot.archive_path).map_err(|e| SnapshotError::Io {
        path: snapshot.archive_path.clone(),
        source: e,
    })?;
    let dec = GzDecoder::new(BufReader::new(file));
    let mut ar = tar::Archive::new(dec);

    let mut file_data: HashMap<String, Vec<u8>> = HashMap::new();

    for entry in ar
        .entries()
        .map_err(|_| SnapshotError::TarballCorrupt(snapshot.id.clone()))?
    {
        let mut entry = entry.map_err(|_| SnapshotError::TarballCorrupt(snapshot.id.clone()))?;
        let raw_path = entry
            .path()
            .map_err(|_| SnapshotError::TarballCorrupt(snapshot.id.clone()))?
            .into_owned();
        let path_str = raw_path.to_string_lossy();

        if let Some(rel) = path_str.strip_prefix("payload/") {
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .map_err(|_| SnapshotError::TarballCorrupt(snapshot.id.clone()))?;
            // Tar strips leading slashes; normalize the key to match manifest path lookup.
            let key = rel.trim_start_matches('/').to_owned();
            file_data.insert(key, buf);
        }
    }

    let mut result: Vec<(PathBuf, TargetEntry)> = Vec::new();
    for entry in &snapshot.manifest.targets {
        // On Unix, joining with an absolute path replaces the base; this correctly
        // resolves both relative paths (normal case) and absolute paths (test case).
        let abs = home.join(&entry.path);
        if entry.kind == TargetKind::File {
            if let Some(expected) = &entry.sha256 {
                let key = entry.path.trim_start_matches('/');
                let data = file_data
                    .get(key)
                    .ok_or_else(|| SnapshotError::TarballCorrupt(snapshot.id.clone()))?;
                let mut hasher = Sha256::new();
                hasher.update(data);
                let actual = hex_encode(hasher.finalize());
                if &actual != expected {
                    return Err(SnapshotError::ChecksumMismatch {
                        expected: expected.clone(),
                        actual,
                        path: abs.clone(),
                    }
                    .into());
                }
            }
        }
        result.push((abs, entry.clone()));
    }

    Ok((result, file_data))
}

/// Apply one entry to the filesystem during a restore.
fn apply_restore_entry(
    abs_path: &Path,
    entry: &TargetEntry,
    _home: &Path,
    file_data: &HashMap<String, Vec<u8>>,
) -> Result<(), CoreError> {
    match &entry.kind {
        TargetKind::Absent => {
            if abs_path.symlink_metadata().is_ok() {
                fs::remove_file(abs_path).map_err(|e| SnapshotError::Io {
                    path: abs_path.to_owned(),
                    source: e,
                })?;
            }
        }
        TargetKind::Symlink => {
            let dest = entry.symlink_to.as_deref().unwrap_or("");
            if abs_path.symlink_metadata().is_ok() {
                fs::remove_file(abs_path).map_err(|e| SnapshotError::Io {
                    path: abs_path.to_owned(),
                    source: e,
                })?;
            }
            symlink(dest, abs_path).map_err(|e| SnapshotError::Io {
                path: abs_path.to_owned(),
                source: e,
            })?;
            // chmod(2) on a symlink on Linux operates on the symlink target, not the
            // symlink itself (lchmod is not supported). Mode bits for symlinks are
            // meaningless here; skip.
        }
        TargetKind::Dir => {
            fs::create_dir_all(abs_path).map_err(|e| SnapshotError::Io {
                path: abs_path.to_owned(),
                source: e,
            })?;
            if let Some(mode) = entry.mode {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(abs_path, fs::Permissions::from_mode(mode)).map_err(|e| {
                    SnapshotError::Io {
                        path: abs_path.to_owned(),
                        source: e,
                    }
                })?;
            }
        }
        TargetKind::File => {
            let key = entry.path.trim_start_matches('/');
            let data = file_data.get(key).ok_or_else(|| {
                SnapshotError::InvalidArchive(format!("file data missing for {}", entry.path))
            })?;

            // Atomic write: temp sibling → fsync → rename → fsync(parent dir).
            let parent = abs_path.parent().unwrap_or(abs_path);
            let mut tmp =
                tempfile::NamedTempFile::new_in(parent).map_err(|e| SnapshotError::Io {
                    path: parent.to_owned(),
                    source: e,
                })?;
            tmp.write_all(data).map_err(|e| SnapshotError::Io {
                path: tmp.path().to_owned(),
                source: e,
            })?;
            tmp.as_file().sync_all().map_err(|e| SnapshotError::Io {
                path: tmp.path().to_owned(),
                source: e,
            })?;
            tmp.persist(abs_path).map_err(|e| SnapshotError::Io {
                path: abs_path.to_owned(),
                source: e.error,
            })?;
            if let Some(mode) = entry.mode {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(abs_path, fs::Permissions::from_mode(mode)).map_err(|e| {
                    SnapshotError::Io {
                        path: abs_path.to_owned(),
                        source: e,
                    }
                })?;
            }
            fsync_dir(parent)?;
        }
    }
    Ok(())
}

fn parse_iso8601(s: &str) -> Option<std::time::SystemTime> {
    OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|dt| {
            let unix = dt.unix_timestamp();
            if unix >= 0 {
                std::time::UNIX_EPOCH + Duration::from_secs(unix.unsigned_abs())
            } else {
                std::time::UNIX_EPOCH
            }
        })
}

fn hash_file_hex(path: &Path) -> Result<String, CoreError> {
    let f = File::open(path).map_err(|e| SnapshotError::Io {
        path: path.to_owned(),
        source: e,
    })?;
    let mut reader = BufReader::new(f);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = reader.read(&mut buf).map_err(|e| SnapshotError::Io {
            path: path.to_owned(),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_encode(hasher.finalize()))
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").expect("write to String is infallible");
    }
    s
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_manager() -> (TempDir, SnapshotManager) {
        let tmp = TempDir::new().unwrap();
        let mgr = SnapshotManager::open(tmp.path()).unwrap();
        (tmp, mgr)
    }

    fn write_file(base: &Path, name: &str, content: &[u8]) -> PathBuf {
        let p = base.join(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, content).unwrap();
        p
    }

    // ── 1. zero targets ──────────────────────────────────────────────────────

    #[test]
    fn create_zero_targets() {
        let (_tmp, mgr) = make_manager();
        let snap = mgr
            .create(CreateRequest {
                profile: "empty",
                trigger: SnapshotTrigger::Manual,
                targets: &[],
            })
            .unwrap();
        assert_eq!(snap.manifest.schema_version, 1);
        assert_eq!(snap.manifest.targets.len(), 0);
        assert_eq!(snap.manifest.profile, "empty");
        assert!(snap.archive_path.exists());
    }

    // ── 2. single regular file ───────────────────────────────────────────────

    #[test]
    fn create_single_file() {
        let home = TempDir::new().unwrap();
        let (_data, mgr) = make_manager();
        let f = write_file(home.path(), "dot.conf", b"hello world");

        let snap = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::PreApply,
                targets: &[f],
            })
            .unwrap();

        assert_eq!(snap.manifest.targets.len(), 1);
        assert_eq!(snap.manifest.targets[0].kind, TargetKind::File);
        assert!(snap.manifest.targets[0].sha256.is_some());
    }

    // ── 3. symlink — not followed ─────────────────────────────────────────────

    #[test]
    fn create_symlink_not_followed() {
        let home = TempDir::new().unwrap();
        let (_data, mgr) = make_manager();
        let actual = write_file(home.path(), "real", b"data");
        let link = home.path().join("link");
        symlink(&actual, &link).unwrap();

        let snap = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[link.clone()],
            })
            .unwrap();

        assert_eq!(snap.manifest.targets[0].kind, TargetKind::Symlink);
        assert_eq!(
            snap.manifest.targets[0].symlink_to.as_deref(),
            Some(actual.to_str().unwrap())
        );
    }

    // ── 4. non-existent target → absent ──────────────────────────────────────

    #[test]
    fn create_absent_target() {
        let home = TempDir::new().unwrap();
        let (_data, mgr) = make_manager();
        let missing = home.path().join("does_not_exist");

        let snap = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[missing],
            })
            .unwrap();

        assert_eq!(snap.manifest.targets[0].kind, TargetKind::Absent);
    }

    // ── 5. directory — recursive ──────────────────────────────────────────────

    #[test]
    fn create_directory() {
        let home = TempDir::new().unwrap();
        let (_data, mgr) = make_manager();
        let dir = home.path().join("mydir");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("a.txt"), b"a").unwrap();
        fs::write(dir.join("b.txt"), b"b").unwrap();

        let snap = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[dir],
            })
            .unwrap();

        assert_eq!(snap.manifest.targets[0].kind, TargetKind::Dir);
    }

    // ── 6. path with spaces ───────────────────────────────────────────────────

    #[test]
    fn create_path_with_spaces() {
        let home = TempDir::new().unwrap();
        let (_data, mgr) = make_manager();
        let f = write_file(home.path(), "my config/my file.conf", b"data");

        let snap = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[f],
            })
            .unwrap();

        assert_eq!(snap.manifest.targets.len(), 1);
        assert!(snap.manifest.targets[0].path.contains(' '));
    }

    // ── 7. non-UTF-8 path → CoreError::NonUtf8Path ───────────────────────────

    #[test]
    fn create_rejects_non_utf8_path() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let (_data, mgr) = make_manager();
        let bad = PathBuf::from(OsStr::from_bytes(b"\xff\xfe"));

        let result = mgr.create(CreateRequest {
            profile: "p",
            trigger: SnapshotTrigger::Manual,
            targets: &[bad],
        });
        assert!(matches!(result, Err(CoreError::NonUtf8Path(_))));
    }

    // ── 8. restore dry_run — no FS changes ───────────────────────────────────

    #[test]
    fn restore_dry_run_no_changes() {
        let home = TempDir::new().unwrap();
        let (_data, mgr) = make_manager();
        let f = write_file(home.path(), "cfg", b"original");

        let snap = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[f.clone()],
            })
            .unwrap();

        fs::write(&f, b"modified").unwrap();

        mgr.restore(
            &snap.id,
            RestoreOptions {
                dry_run: true,
                continue_on_error: false,
            },
        )
        .unwrap();

        assert_eq!(fs::read(&f).unwrap(), b"modified");
    }

    // ── 9. restore file — bit-for-bit identical ───────────────────────────────

    #[test]
    fn restore_file_bit_for_bit() {
        let home = TempDir::new().unwrap();
        let (_data, mgr) = make_manager();
        let original = b"bit-for-bit content";
        let f = write_file(home.path(), "cfg.conf", original);

        let snap = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[f.clone()],
            })
            .unwrap();

        fs::write(&f, b"corrupted").unwrap();

        mgr.restore(
            &snap.id,
            RestoreOptions {
                dry_run: false,
                continue_on_error: false,
            },
        )
        .unwrap();

        assert_eq!(fs::read(&f).unwrap(), original.as_slice());
    }

    // ── 10. restore symlink → recreated as symlink ────────────────────────────

    #[test]
    fn restore_symlink_as_symlink() {
        let home = TempDir::new().unwrap();
        let (_data, mgr) = make_manager();
        let actual = write_file(home.path(), "actual", b"data");
        let link = home.path().join("link");
        symlink(&actual, &link).unwrap();

        let snap = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[link.clone()],
            })
            .unwrap();

        fs::remove_file(&link).unwrap();
        fs::write(&link, b"now a file").unwrap();

        mgr.restore(
            &snap.id,
            RestoreOptions {
                dry_run: false,
                continue_on_error: false,
            },
        )
        .unwrap();

        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), actual);
    }

    // ── 11. restore absent → removes file if it existed ──────────────────────

    #[test]
    fn restore_absent_removes_file() {
        let home = TempDir::new().unwrap();
        let (_data, mgr) = make_manager();
        let f = home.path().join("will_be_created_later");

        let snap = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[f.clone()],
            })
            .unwrap();

        fs::write(&f, b"new data").unwrap();
        assert!(f.exists());

        mgr.restore(
            &snap.id,
            RestoreOptions {
                dry_run: false,
                continue_on_error: false,
            },
        )
        .unwrap();

        assert!(!f.exists());
    }

    // ── 12. restore corrupt tarball → TarballCorrupt ──────────────────────────

    #[test]
    fn restore_corrupt_tarball() {
        let (_tmp, mgr) = make_manager();
        let snap = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[],
            })
            .unwrap();

        fs::write(&snap.archive_path, b"not a gzip").unwrap();

        let result = mgr.restore(
            &snap.id,
            RestoreOptions {
                dry_run: false,
                continue_on_error: false,
            },
        );
        assert!(matches!(
            result,
            Err(CoreError::Snapshot(SnapshotError::TarballCorrupt(_)))
        ));
    }

    // ── 13. checksum mismatch → error, nothing restored (all-or-nothing) ──────

    #[test]
    fn restore_checksum_mismatch_atomic() {
        let home = TempDir::new().unwrap();
        let (_data, mgr) = make_manager();
        let f = write_file(home.path(), "important.conf", b"real content");

        let snap = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[f.clone()],
            })
            .unwrap();

        // Rewrite the tarball with a bad sha256 in the embedded manifest.
        let mut bad_manifest = snap.manifest.clone();
        bad_manifest.targets[0].sha256 = Some("00".repeat(32));
        let bad_json = serde_json::to_vec_pretty(&bad_manifest).unwrap();

        let out = File::create(&snap.archive_path).unwrap();
        let enc = GzEncoder::new(out, Compression::default());
        let mut ar = tar::Builder::new(enc);

        let mut hdr = tar::Header::new_gnu();
        hdr.set_size(bad_json.len() as u64);
        hdr.set_mode(0o644);
        hdr.set_cksum();
        ar.append_data(&mut hdr, "manifest.json", bad_json.as_slice())
            .unwrap();

        // Use the same path the snapshot recorded (may be absolute; tar strips leading /).
        let payload_path = format!(
            "payload/{}",
            snap.manifest.targets[0].path.trim_start_matches('/')
        );
        let real_data = b"real content";
        let mut hdr2 = tar::Header::new_gnu();
        hdr2.set_size(real_data.len() as u64);
        hdr2.set_mode(0o644);
        hdr2.set_cksum();
        ar.append_data(&mut hdr2, &payload_path, real_data.as_slice())
            .unwrap();

        ar.into_inner().unwrap().finish().unwrap();

        let result = mgr.restore(
            &snap.id,
            RestoreOptions {
                dry_run: false,
                continue_on_error: false,
            },
        );
        assert!(
            matches!(
                result,
                Err(CoreError::Snapshot(SnapshotError::ChecksumMismatch { .. }))
            ),
            "expected ChecksumMismatch, got {result:?}"
        );
    }

    // ── 14. list — descending order ───────────────────────────────────────────

    #[test]
    fn list_descending_order() {
        let (_tmp, mgr) = make_manager();

        let s1 = mgr
            .create(CreateRequest {
                profile: "a",
                trigger: SnapshotTrigger::Manual,
                targets: &[],
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(2));
        let s2 = mgr
            .create(CreateRequest {
                profile: "b",
                trigger: SnapshotTrigger::Manual,
                targets: &[],
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(2));
        let s3 = mgr
            .create(CreateRequest {
                profile: "c",
                trigger: SnapshotTrigger::Manual,
                targets: &[],
            })
            .unwrap();

        let list = mgr.list().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].id, s3.id);
        assert_eq!(list[1].id, s2.id);
        assert_eq!(list[2].id, s1.id);
    }

    // ── 15. list ignores orphan sidecars ──────────────────────────────────────

    #[test]
    fn list_ignores_orphan_sidecars() {
        let (_tmp, mgr) = make_manager();
        let snap = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[],
            })
            .unwrap();

        let orphan_id = SnapshotId::new();
        let orphan_sidecar = mgr.sidecar_path(&orphan_id);
        let json = serde_json::to_vec_pretty(&snap.manifest).unwrap();
        fs::write(&orphan_sidecar, json).unwrap();

        let list = mgr.list().unwrap();
        assert_eq!(list.len(), 1, "orphan sidecar must be skipped");
        assert_eq!(list[0].id, snap.id);
    }

    // ── 16. list handles orphan tarball via internal manifest ─────────────────

    #[test]
    fn list_orphan_tarball_reads_internal_manifest() {
        let (_tmp, mgr) = make_manager();
        let snap = mgr
            .create(CreateRequest {
                profile: "orphan-profile",
                trigger: SnapshotTrigger::Manual,
                targets: &[],
            })
            .unwrap();

        fs::remove_file(mgr.sidecar_path(&snap.id)).unwrap();

        let list = mgr.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].profile, "orphan-profile");
    }

    // ── 17. prune KeepLast(2) with 5 snapshots ────────────────────────────────

    #[test]
    fn prune_keep_last_2() {
        let (_tmp, mgr) = make_manager();
        let mut ids = Vec::new();
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(2));
            let s = mgr
                .create(CreateRequest {
                    profile: "p",
                    trigger: SnapshotTrigger::Manual,
                    targets: &[],
                })
                .unwrap();
            ids.push(s.id);
        }

        let report = mgr.prune(PrunePolicy::KeepLast(2)).unwrap();
        assert_eq!(report.removed.len(), 3);

        let remaining = mgr.list().unwrap();
        assert_eq!(remaining.len(), 2);
        let remaining_ids: Vec<_> = remaining.iter().map(|s| &s.id).collect();
        assert!(remaining_ids.contains(&&ids[4]));
        assert!(remaining_ids.contains(&&ids[3]));
    }

    // ── 18. prune KeepLastPerProfile(2) ──────────────────────────────────────

    #[test]
    fn prune_keep_last_per_profile() {
        let (_tmp, mgr) = make_manager();
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(2));
            mgr.create(CreateRequest {
                profile: "foo",
                trigger: SnapshotTrigger::Manual,
                targets: &[],
            })
            .unwrap();
            mgr.create(CreateRequest {
                profile: "bar",
                trigger: SnapshotTrigger::Manual,
                targets: &[],
            })
            .unwrap();
        }

        let report = mgr.prune(PrunePolicy::KeepLastPerProfile(2)).unwrap();
        assert_eq!(report.removed.len(), 2, "1 foo + 1 bar removed");

        let remaining = mgr.list().unwrap();
        assert_eq!(remaining.len(), 4);
        let foo = remaining.iter().filter(|s| s.profile == "foo").count();
        let bar = remaining.iter().filter(|s| s.profile == "bar").count();
        assert_eq!(foo, 2);
        assert_eq!(bar, 2);
    }

    // ── 19. prune OlderThan ───────────────────────────────────────────────────

    #[test]
    fn prune_older_than() {
        let (_tmp, mgr) = make_manager();
        mgr.create(CreateRequest {
            profile: "p",
            trigger: SnapshotTrigger::Manual,
            targets: &[],
        })
        .unwrap();

        std::thread::sleep(Duration::from_millis(15));

        let report = mgr
            .prune(PrunePolicy::OlderThan(Duration::from_millis(5)))
            .unwrap();
        assert_eq!(report.removed.len(), 1);
        assert!(mgr.list().unwrap().is_empty());
    }

    // ── 20. get unknown id → NotFound ────────────────────────────────────────

    #[test]
    fn get_unknown_id() {
        let (_tmp, mgr) = make_manager();
        let id = SnapshotId::new();
        let result = mgr.get(&id);
        assert!(matches!(
            result,
            Err(CoreError::Snapshot(SnapshotError::NotFound(_)))
        ));
    }

    // ── Roundtrip: create + get + manifest identical ──────────────────────────

    #[test]
    fn roundtrip_create_get() {
        let (_tmp, mgr) = make_manager();
        let snap = mgr
            .create(CreateRequest {
                profile: "rt",
                trigger: SnapshotTrigger::PreApply,
                targets: &[],
            })
            .unwrap();
        let got = mgr.get(&snap.id).unwrap();
        assert_eq!(got.manifest.id, snap.manifest.id);
        assert_eq!(got.manifest.profile, snap.manifest.profile);
        assert_eq!(got.manifest.payload_sha256, snap.manifest.payload_sha256);
    }

    // ── 21. prune OnlyId — removes exactly one ───────────────────────────────

    #[test]
    fn prune_only_id_removes_exactly_one() {
        let (_tmp, mgr) = make_manager();
        let s1 = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[],
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(2));
        let s2 = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[],
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(2));
        let s3 = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[],
            })
            .unwrap();

        let report = mgr.prune(PrunePolicy::OnlyId(s2.id.clone())).unwrap();
        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.removed[0], s2.id);

        let remaining = mgr.list().unwrap();
        assert_eq!(remaining.len(), 2);
        let ids: Vec<_> = remaining.iter().map(|s| &s.id).collect();
        assert!(ids.contains(&&s1.id));
        assert!(ids.contains(&&s3.id));
    }

    // ── 22. prune OnlyId with unknown id → empty report, no error ─────────────

    #[test]
    fn prune_only_id_nonexistent_is_noop() {
        let (_tmp, mgr) = make_manager();
        mgr.create(CreateRequest {
            profile: "p",
            trigger: SnapshotTrigger::Manual,
            targets: &[],
        })
        .unwrap();

        let fake_id = SnapshotId::new();
        let report = mgr.prune(PrunePolicy::OnlyId(fake_id)).unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(report.freed_bytes, 0);
        assert_eq!(mgr.list().unwrap().len(), 1, "original snapshot untouched");
    }

    // ── Two rapid snapshots → distinct ULIDs ──────────────────────────────────

    #[test]
    fn two_rapid_snapshots_distinct_ulids() {
        let (_tmp, mgr) = make_manager();
        let s1 = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[],
            })
            .unwrap();
        let s2 = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[],
            })
            .unwrap();
        assert_ne!(s1.id, s2.id);
    }

    // ── Mode bits preserved on file restore ──────────────────────────────────

    #[test]
    fn restore_preserves_mode_bits() {
        use std::os::unix::fs::PermissionsExt;

        let home = TempDir::new().unwrap();
        let (_data, mgr) = make_manager();

        for &mode in &[0o600u32, 0o755u32] {
            let f = write_file(home.path(), &format!("file_{mode:o}"), b"content");
            fs::set_permissions(&f, fs::Permissions::from_mode(mode)).unwrap();

            let snap = mgr
                .create(CreateRequest {
                    profile: "p",
                    trigger: SnapshotTrigger::Manual,
                    targets: &[f.clone()],
                })
                .unwrap();

            // Overwrite with different mode.
            fs::write(&f, b"changed").unwrap();
            fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();

            mgr.restore(
                &snap.id,
                RestoreOptions {
                    dry_run: false,
                    continue_on_error: false,
                },
            )
            .unwrap();

            let restored_mode = fs::symlink_metadata(&f).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                restored_mode, mode,
                "mode 0o{mode:o} not preserved after restore (got 0o{restored_mode:o})"
            );
        }
    }

    // ── Mode bits preserved on directory restore ──────────────────────────────

    #[test]
    fn restore_preserves_mode_bits_for_directory() {
        use std::os::unix::fs::PermissionsExt;

        let home = TempDir::new().unwrap();
        let (_data, mgr) = make_manager();

        let dir = home.path().join("private_dir");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();

        let snap = mgr
            .create(CreateRequest {
                profile: "p",
                trigger: SnapshotTrigger::Manual,
                targets: &[dir.clone()],
            })
            .unwrap();

        // Change permissions.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

        mgr.restore(
            &snap.id,
            RestoreOptions {
                dry_run: false,
                continue_on_error: false,
            },
        )
        .unwrap();

        let restored_mode = fs::symlink_metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            restored_mode, 0o700,
            "directory mode 0o700 not preserved after restore (got 0o{restored_mode:o})"
        );
    }
}
