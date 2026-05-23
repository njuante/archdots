//! Append-only JSONL transaction journal.
//!
//! One entry per line at `$XDG_STATE_HOME/archdots/journal.jsonl`.
//! `O_APPEND` writes ≤ `PIPE_BUF` (4096 on Linux) are atomic per POSIX §2.9.7,
//! so concurrent appenders can share the same file without a lock.

use std::{
    collections::HashSet,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Lines, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use ulid::Ulid;

use crate::{
    error::{CoreError, JournalError},
    snapshot::SnapshotId,
};

// ─── Constants ────────────────────────────────────────────────────────────────

const SCHEMA_VERSION: u32 = 1;
/// Hard cap on links per entry so a serialized line stays below `MAX_LINE_BYTES`.
pub const MAX_LINKS: usize = 200;
/// Byte ceiling for one serialized entry (json + `\n`). Set high enough that
/// realistic profiles (~20 entries with absolute paths into the v0.6 staging
/// directory, each with a `PriorState::Symlink { points_to }` field) fit. The
/// original 4000 was sized for PIPE_BUF atomicity across concurrent writers,
/// but the journal is always written under `ApplyLock` so single-writer
/// serialization is guaranteed by the lock, not by per-write atomicity.
const MAX_LINE_BYTES: usize = 16384;

// ─── Serde helper: PathBuf ↔ UTF-8 string ────────────────────────────────────

mod serde_path {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::path::PathBuf;

    pub fn serialize<S: Serializer>(path: &std::path::Path, s: S) -> Result<S::Ok, S::Error> {
        match path.to_str() {
            Some(str) => s.serialize_str(str),
            None => Err(serde::ser::Error::custom("path contains non-UTF-8 bytes")),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
        let s = String::deserialize(d)?;
        Ok(PathBuf::from(s))
    }
}

// ─── JournalId ────────────────────────────────────────────────────────────────

/// Opaque, time-ordered journal entry identifier (ULID).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct JournalId(pub Ulid);

impl JournalId {
    /// Generate a new unique `JournalId`.
    #[must_use]
    pub fn new() -> Self {
        Self(Ulid::new())
    }
}

impl Default for JournalId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for JournalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for JournalId {
    type Err = ulid::DecodeError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

// ─── Domain types ─────────────────────────────────────────────────────────────

/// Whether the journal entry describes an apply or a rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum JournalAction {
    /// Forward apply: create/replace symlinks.
    Apply,
    /// Rollback: restore a previous snapshot.
    Rollback,
}

/// Lifecycle status of a journal entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum JournalStatus {
    /// Operation started but not yet concluded (crash-safe sentinel).
    InProgress,
    /// All links were applied or rolled back successfully.
    Success,
    /// Some links succeeded, some failed; snapshot was partially restored.
    Partial,
    /// Operation failed entirely.
    Failed,
}

/// State of the target path before the operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PriorState {
    /// The path did not exist.
    Absent,
    /// The path was a regular file.
    File,
    /// The path was a directory.
    Dir,
    /// The path was a symbolic link pointing to `points_to`.
    Symlink {
        #[serde(with = "serde_path")]
        /// Destination the symlink pointed to.
        points_to: PathBuf,
    },
}

/// Outcome of a single link operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LinkOutcome {
    /// Symlink was created or replaced.
    Linked,
    /// Link was skipped (e.g. already owned, dry-run).
    Skipped,
    /// Link failed with the given reason.
    Failed(String),
}

/// Record of one symlink operation inside a journal entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkRecord {
    /// Dotfile source path.
    #[serde(with = "serde_path")]
    pub source: PathBuf,
    /// Filesystem target path.
    #[serde(with = "serde_path")]
    pub target: PathBuf,
    /// State of `target` before this operation.
    pub prior_state: PriorState,
    /// What happened to this link.
    pub outcome: LinkOutcome,
}

/// One entry in the append-only journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Always `1` for this schema version.
    pub schema_version: u32,
    /// Unique entry identifier (ULID).
    pub id: JournalId,
    /// UTC timestamp when the entry was written.
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    /// Profile name this entry belongs to.
    pub profile: String,
    /// Apply or rollback.
    pub action: JournalAction,
    /// Associated snapshot, if any.
    pub snapshot_id: Option<SnapshotId>,
    /// Per-link outcomes.
    pub links: Vec<LinkRecord>,
    /// Lifecycle status.
    pub status: JournalStatus,
    /// Human-readable error, present on failures.
    pub error: Option<String>,
    /// Id of the `InProgress` entry this entry closes (patch pattern).
    pub supersedes: Option<JournalId>,
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

fn validate_entry_paths(entry: &JournalEntry) -> Result<(), CoreError> {
    for link in &entry.links {
        if link.source.to_str().is_none() {
            return Err(JournalError::NonUtf8Path(link.source.clone()).into());
        }
        if link.target.to_str().is_none() {
            return Err(JournalError::NonUtf8Path(link.target.clone()).into());
        }
        if let PriorState::Symlink { ref points_to } = link.prior_state {
            if points_to.to_str().is_none() {
                return Err(JournalError::NonUtf8Path(points_to.clone()).into());
            }
        }
    }
    Ok(())
}

// ─── Journal ──────────────────────────────────────────────────────────────────

/// Handle to the append-only JSONL journal file.
///
/// The file is opened (and synced) on every [`append`][Journal::append] call
/// and closed immediately after. The struct itself is stateless — only the
/// path is stored — so it is `Send + Sync` and can be wrapped in `Arc`.
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    /// Open the journal rooted at `state_home`.
    ///
    /// Creates `state_home/archdots/` if it does not exist.
    /// The `journal.jsonl` file is **not** created here; it appears on the
    /// first [`append`][Journal::append].
    ///
    /// # Errors
    ///
    /// Returns [`CoreError`] if the parent directory cannot be created.
    pub fn open(state_home: &Path) -> Result<Self, CoreError> {
        let dir = state_home.join("archdots");
        fs::create_dir_all(&dir).map_err(|e| JournalError::Io {
            path: dir.clone(),
            source: e,
        })?;
        Ok(Self {
            path: dir.join("journal.jsonl"),
        })
    }

    /// Append `entry` to the journal as a single JSONL line.
    ///
    /// The write is atomic for lines ≤ `PIPE_BUF` (4096 bytes on Linux).
    /// A hard limit of [`MAX_LINKS`] entries per record keeps every line
    /// comfortably below that ceiling.
    ///
    /// # Errors
    ///
    /// - [`JournalError::TooManyLinks`] — `entry.links.len() > 200` or the
    ///   serialized line exceeds 4000 bytes.
    /// - [`JournalError::NonUtf8Path`] — a `PathBuf` field is not valid UTF-8.
    /// - [`JournalError::Io`] — the file could not be opened or written.
    pub fn append(&self, entry: &JournalEntry) -> Result<(), CoreError> {
        if entry.links.len() > MAX_LINKS {
            return Err(JournalError::TooManyLinks {
                found: entry.links.len(),
                max: MAX_LINKS,
            }
            .into());
        }

        // Validate all PathBuf fields for UTF-8 before serialization.
        validate_entry_paths(entry)?;

        let json = serde_json::to_string(entry).map_err(|e| JournalError::Io {
            path: self.path.clone(),
            source: io::Error::new(io::ErrorKind::InvalidData, e.to_string()),
        })?;

        // Extra byte-size guard: json + '\n' must stay below MAX_LINE_BYTES.
        if json.len() + 1 > MAX_LINE_BYTES {
            return Err(JournalError::TooManyLinks {
                found: entry.links.len(),
                max: MAX_LINKS,
            }
            .into());
        }

        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)
            .map_err(|e| JournalError::Io {
                path: self.path.clone(),
                source: e,
            })?;

        // Combine json + '\n' so the entire line is one atomic write() syscall.
        let mut line = json;
        line.push('\n');
        file.write_all(line.as_bytes())
            .map_err(|e| JournalError::Io {
                path: self.path.clone(),
                source: e,
            })?;
        file.sync_all().map_err(|e| JournalError::Io {
            path: self.path.clone(),
            source: e,
        })?;

        Ok(())
    }

    /// Return the last entry in the journal for `profile`.
    ///
    /// Returns `None` if the journal is empty or has no entry for `profile`.
    /// Corrupted lines are silently skipped.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError`] only if the journal file cannot be opened.
    pub fn last_for_profile(&self, profile: &str) -> Result<Option<JournalEntry>, CoreError> {
        let mut last: Option<JournalEntry> = None;
        for entry in (self.iter()?).flatten() {
            if entry.profile == profile {
                last = Some(entry);
            }
        }
        Ok(last)
    }

    /// Find the latest state of entry `id` (last physical line with that id).
    ///
    /// Returns `None` if no entry with that id exists. Never returns an error
    /// just because the id is missing. Corrupted lines are silently skipped.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError`] only if the journal file cannot be opened.
    pub fn find(&self, id: &JournalId) -> Result<Option<JournalEntry>, CoreError> {
        let mut found: Option<JournalEntry> = None;
        for entry in (self.iter()?).flatten() {
            if &entry.id == id {
                found = Some(entry);
            }
        }
        Ok(found)
    }

    /// Return a streaming iterator over all journal entries, oldest first.
    ///
    /// If the journal file does not exist, returns an empty iterator (not an
    /// error). Corrupted lines yield `Err` items but iteration continues.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError`] only if the file exists but cannot be opened.
    #[allow(clippy::iter_not_returning_iterator)]
    pub fn iter(&self) -> Result<JournalIter, CoreError> {
        if !self.path.exists() {
            return Ok(JournalIter {
                inner: None,
                path: self.path.clone(),
                line_number: 0,
            });
        }
        let file = File::open(&self.path).map_err(|e| JournalError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        Ok(JournalIter {
            inner: Some(BufReader::new(file).lines()),
            path: self.path.clone(),
            line_number: 0,
        })
    }

    /// Return all `InProgress` entries not closed by a later `supersedes`.
    ///
    /// An `InProgress` entry is orphaned if no other entry in the journal has
    /// `supersedes == that_entry.id`. Corrupted lines are silently skipped.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError`] only if the journal file cannot be opened.
    pub fn orphaned_in_progress(&self) -> Result<Vec<JournalEntry>, CoreError> {
        let mut all: Vec<JournalEntry> = Vec::new();
        let mut superseded: HashSet<String> = HashSet::new();

        for entry in (self.iter()?).flatten() {
            if let Some(ref sup) = entry.supersedes {
                superseded.insert(sup.to_string());
            }
            all.push(entry);
        }

        Ok(all
            .into_iter()
            .filter(|e| {
                e.status == JournalStatus::InProgress && !superseded.contains(&e.id.to_string())
            })
            .collect())
    }
}

// ─── JournalIter ──────────────────────────────────────────────────────────────

/// Streaming iterator over [`JournalEntry`] items.
///
/// Backed by `BufRead::lines()` — never loads the whole file into memory.
/// Yields `Err` for unparseable lines but continues iteration.
pub struct JournalIter {
    inner: Option<Lines<BufReader<File>>>,
    path: PathBuf,
    line_number: u64,
}

impl Iterator for JournalIter {
    type Item = Result<JournalEntry, CoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        let lines = self.inner.as_mut()?;

        loop {
            self.line_number += 1;
            match lines.next()? {
                Err(e) => {
                    return Some(Err(JournalError::Io {
                        path: self.path.clone(),
                        source: e,
                    }
                    .into()));
                }
                Ok(line) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<JournalEntry>(trimmed) {
                        Err(e) => {
                            return Some(Err(JournalError::DeserializeLine {
                                line_number: self.line_number,
                                source: e,
                            }
                            .into()));
                        }
                        Ok(entry) => {
                            if entry.schema_version > SCHEMA_VERSION {
                                return Some(Err(JournalError::SchemaTooNew {
                                    found: entry.schema_version,
                                    supported: SCHEMA_VERSION,
                                }
                                .into()));
                            }
                            return Some(Ok(entry));
                        }
                    }
                }
            }
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use time::OffsetDateTime;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn open(dir: &Path) -> Journal {
        Journal::open(dir).expect("journal::open")
    }

    fn make_entry(profile: &str) -> JournalEntry {
        JournalEntry {
            schema_version: 1,
            id: JournalId::new(),
            ts: OffsetDateTime::now_utc(),
            profile: profile.to_owned(),
            action: JournalAction::Apply,
            snapshot_id: None,
            links: Vec::new(),
            status: JournalStatus::Success,
            error: None,
            supersedes: None,
        }
    }

    fn make_entry_with_links(profile: &str, n: usize) -> JournalEntry {
        let links = (0..n)
            .map(|i| LinkRecord {
                source: PathBuf::from(format!("/dots/file{i}")),
                target: PathBuf::from(format!("/home/u/file{i}")),
                prior_state: PriorState::Absent,
                outcome: LinkOutcome::Linked,
            })
            .collect();
        JournalEntry {
            schema_version: 1,
            id: JournalId::new(),
            ts: OffsetDateTime::now_utc(),
            profile: profile.to_owned(),
            action: JournalAction::Apply,
            snapshot_id: None,
            links,
            status: JournalStatus::Success,
            error: None,
            supersedes: None,
        }
    }

    fn journal_path(dir: &Path) -> PathBuf {
        dir.join("archdots").join("journal.jsonl")
    }

    // ── 1. open on non-existent directory → mkdir -p ─────────────────────────

    #[test]
    fn open_creates_parent_dir() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a").join("b").join("c");
        open(&nested);
        assert!(nested.join("archdots").is_dir());
    }

    // ── 2. open does NOT create journal.jsonl ─────────────────────────────────

    #[test]
    fn open_does_not_create_journal_file() {
        let tmp = TempDir::new().unwrap();
        open(tmp.path());
        assert!(!journal_path(tmp.path()).exists());
    }

    // ── 3. first append creates the file with one line ────────────────────────

    #[test]
    fn append_creates_file_single_line() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());
        j.append(&make_entry("p")).unwrap();
        let content = fs::read_to_string(journal_path(tmp.path())).unwrap();
        assert_eq!(content.lines().count(), 1);
    }

    // ── 4. multiple appends preserve insertion order ──────────────────────────

    #[test]
    fn append_preserves_order() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());
        let e1 = make_entry("a");
        let e2 = make_entry("b");
        let e3 = make_entry("c");
        j.append(&e1).unwrap();
        j.append(&e2).unwrap();
        j.append(&e3).unwrap();

        let entries: Vec<JournalEntry> = j.iter().unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(entries[0].profile, "a");
        assert_eq!(entries[1].profile, "b");
        assert_eq!(entries[2].profile, "c");
    }

    // ── 5. each line is independently valid JSON ──────────────────────────────

    #[test]
    fn each_line_is_valid_json() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());
        for _ in 0..5 {
            j.append(&make_entry("p")).unwrap();
        }
        let content = fs::read_to_string(journal_path(tmp.path())).unwrap();
        for line in content.lines() {
            assert!(
                serde_json::from_str::<serde_json::Value>(line).is_ok(),
                "line is not valid JSON: {line}"
            );
        }
    }

    // ── 6. each line ends with exactly one '\n' ───────────────────────────────

    #[test]
    fn each_line_ends_with_single_newline() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());
        j.append(&make_entry("p")).unwrap();
        j.append(&make_entry("q")).unwrap();
        let raw = fs::read(journal_path(tmp.path())).unwrap();
        // Split on '\n'; last segment should be empty (trailing newline).
        let parts: Vec<&[u8]> = raw.split(|&b| b == b'\n').collect();
        assert!(parts.last().unwrap().is_empty(), "file must end with \\n");
        // Every segment except the last must be non-empty JSON.
        for part in &parts[..parts.len() - 1] {
            assert!(!part.is_empty(), "empty line found");
            assert!(
                serde_json::from_slice::<serde_json::Value>(part).is_ok(),
                "segment is not JSON"
            );
        }
    }

    // ── 7. find existing entry ────────────────────────────────────────────────

    #[test]
    fn find_existing() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());
        let entry = make_entry("x");
        let id = entry.id.clone();
        j.append(&entry).unwrap();
        let found = j.find(&id).unwrap().expect("should find entry");
        assert_eq!(found.id, id);
    }

    // ── 8. find non-existent → None, not Err ─────────────────────────────────

    #[test]
    fn find_nonexistent_returns_none() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());
        j.append(&make_entry("p")).unwrap();
        let missing_id = JournalId::new();
        assert!(j.find(&missing_id).unwrap().is_none());
    }

    // ── 9. last_for_profile with mixed entries ────────────────────────────────

    #[test]
    fn last_for_profile_mixed() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());
        j.append(&make_entry("foo")).unwrap();
        j.append(&make_entry("bar")).unwrap();
        let last_foo = make_entry("foo");
        let last_id = last_foo.id.clone();
        j.append(&last_foo).unwrap();
        j.append(&make_entry("bar")).unwrap();

        let result = j.last_for_profile("foo").unwrap().unwrap();
        assert_eq!(result.id, last_id);
    }

    // ── 10. last_for_profile on empty journal → None ──────────────────────────

    #[test]
    fn last_for_profile_empty() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());
        assert!(j.last_for_profile("foo").unwrap().is_none());
    }

    // ── 11. iter returns entries in ascending insertion order ─────────────────

    #[test]
    fn iter_ascending_order() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());
        let ids: Vec<JournalId> = (0..4)
            .map(|_| {
                let e = make_entry("p");
                let id = e.id.clone();
                j.append(&e).unwrap();
                id
            })
            .collect();

        let read_ids: Vec<JournalId> = j.iter().unwrap().map(|r| r.unwrap().id).collect();
        assert_eq!(read_ids, ids);
    }

    // ── 12. iter over non-existent file → empty iterator, no error ───────────

    #[test]
    fn iter_nonexistent_file_empty() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());
        // Journal file has not been created yet.
        let items: Vec<_> = j.iter().unwrap().collect();
        assert!(items.is_empty());
    }

    // ── 13. iter over corrupt line yields Err then continues ──────────────────

    #[test]
    fn iter_corrupt_line_yields_err_and_continues() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());

        let e1 = make_entry("before");
        let e3 = make_entry("after");
        j.append(&e1).unwrap();

        // Inject a corrupt line directly.
        let mut f = OpenOptions::new()
            .append(true)
            .open(journal_path(tmp.path()))
            .unwrap();
        f.write_all(b"NOT_VALID_JSON\n").unwrap();
        drop(f);

        j.append(&e3).unwrap();

        let results: Vec<Result<JournalEntry, CoreError>> = j.iter().unwrap().collect();
        assert_eq!(results.len(), 3);
        assert!(results[0].is_ok());
        assert!(results[1].is_err());
        assert!(results[2].is_ok());
        assert_eq!(results[0].as_ref().unwrap().profile, "before");
        assert_eq!(results[2].as_ref().unwrap().profile, "after");
    }

    // ── 14. iter over 100 entries is streaming and preserves order ────────────

    #[test]
    fn iter_100_entries_ordered() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());
        let ids: Vec<JournalId> = (0..100)
            .map(|_| {
                let e = make_entry("p");
                let id = e.id.clone();
                j.append(&e).unwrap();
                id
            })
            .collect();

        let read: Vec<JournalId> = j.iter().unwrap().map(|r| r.unwrap().id).collect();
        assert_eq!(read, ids);
    }

    // ── 15. consecutive appends produce distinct ULIDs ────────────────────────

    #[test]
    fn distinct_ulids_on_consecutive_appends() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());
        let e1 = make_entry("p");
        let e2 = make_entry("p");
        assert_ne!(e1.id, e2.id);
        j.append(&e1).unwrap();
        j.append(&e2).unwrap();
    }

    // ── 16. roundtrip: append then find returns an equal entry ────────────────

    #[test]
    fn roundtrip_append_find() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());
        let entry = JournalEntry {
            schema_version: 1,
            id: JournalId::new(),
            ts: OffsetDateTime::now_utc(),
            profile: "hyprland".to_owned(),
            action: JournalAction::Apply,
            snapshot_id: None,
            links: vec![LinkRecord {
                source: PathBuf::from("/dots/hypr.conf"),
                target: PathBuf::from("/home/u/.config/hypr.conf"),
                prior_state: PriorState::Symlink {
                    points_to: PathBuf::from("/old/target"),
                },
                outcome: LinkOutcome::Linked,
            }],
            status: JournalStatus::Success,
            error: None,
            supersedes: None,
        };
        let id = entry.id.clone();
        j.append(&entry).unwrap();

        let got = j.find(&id).unwrap().unwrap();
        assert_eq!(got, entry);
    }

    // ── 17. TooManyLinks: 201 links → error, file not modified ───────────────

    #[test]
    fn too_many_links_rejects_and_does_not_modify_file() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());

        // Write one valid entry first.
        j.append(&make_entry("p")).unwrap();
        let size_before = fs::metadata(journal_path(tmp.path())).unwrap().len();

        // 201 links exceeds the hard cap.
        let fat = make_entry_with_links("p", MAX_LINKS + 1);
        let err = j.append(&fat).unwrap_err();
        assert!(
            matches!(err, CoreError::Journal(JournalError::TooManyLinks { .. })),
            "expected TooManyLinks, got {err:?}"
        );

        let size_after = fs::metadata(journal_path(tmp.path())).unwrap().len();
        assert_eq!(size_before, size_after, "file must not be modified");
    }

    // ── 18. orphaned_in_progress detects the right orphan ────────────────────

    #[test]
    fn orphaned_in_progress_detects_orphan() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());

        // Entry A: InProgress (will be closed).
        let mut ea = make_entry("foo");
        ea.status = JournalStatus::InProgress;
        let id_a = ea.id.clone();
        j.append(&ea).unwrap();

        // Entry B: InProgress (orphan — never closed).
        let mut eb = make_entry("foo");
        eb.status = JournalStatus::InProgress;
        let id_b = eb.id.clone();
        j.append(&eb).unwrap();

        // Entry C: closes A via supersedes.
        let mut ec = make_entry("foo");
        ec.id = id_a.clone();
        ec.status = JournalStatus::Success;
        ec.supersedes = Some(id_a.clone());
        j.append(&ec).unwrap();

        let orphans = j.orphaned_in_progress().unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].id, id_b);
    }

    // ── 19. orphaned_in_progress with no orphans → empty Vec ─────────────────

    #[test]
    fn orphaned_in_progress_no_orphans() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());

        let mut ea = make_entry("foo");
        ea.status = JournalStatus::InProgress;
        let id_a = ea.id.clone();
        j.append(&ea).unwrap();

        let mut ec = make_entry("foo");
        ec.id = id_a.clone();
        ec.status = JournalStatus::Success;
        ec.supersedes = Some(id_a);
        j.append(&ec).unwrap();

        assert!(j.orphaned_in_progress().unwrap().is_empty());
    }

    // ── 20. schema_version > 1 → SchemaTooNew ────────────────────────────────

    #[test]
    fn schema_too_new() {
        let tmp = TempDir::new().unwrap();
        let j = open(tmp.path());

        // Serialize a valid entry, then bump schema_version to 2.
        let entry = make_entry("p");
        let mut raw_value: serde_json::Value = serde_json::to_value(&entry).unwrap();
        raw_value["schema_version"] = serde_json::json!(2);
        let raw = serde_json::to_string(&raw_value).unwrap();

        {
            let path = journal_path(tmp.path());
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut f = OpenOptions::new()
                .append(true)
                .create(true)
                .open(&path)
                .unwrap();
            f.write_all(raw.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }

        let results: Vec<_> = j.iter().unwrap().collect();
        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            Err(CoreError::Journal(JournalError::SchemaTooNew {
                found: 2,
                supported: 1
            }))
        ));
    }

    // ── 21. concurrent appends: all entries present, no mixing ───────────────
    //
    // Relies on POSIX O_APPEND atomicity for writes ≤ PIPE_BUF (4096 bytes).
    // The test is not marked #[ignore] because it exercises a genuine invariant,
    // but it could theoretically be flaky on non-POSIX or slow CI systems.

    #[test]
    fn concurrent_appends_all_present() {
        use std::sync::Arc;
        use std::thread;

        let tmp = TempDir::new().unwrap();
        let j = Arc::new(open(tmp.path()));

        let handles: Vec<_> = (0..10)
            .map(|i| {
                let j = Arc::clone(&j);
                let profile = format!("thread-{i}");
                thread::spawn(move || {
                    let entry = make_entry(&profile);
                    j.append(&entry).unwrap();
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }

        // All 10 entries must be present and each line must be valid JSON.
        let entries: Vec<JournalEntry> = j
            .iter()
            .unwrap()
            .map(|r| r.expect("unexpected corrupt line in concurrent test"))
            .collect();
        assert_eq!(entries.len(), 10);

        let raw = fs::read_to_string(journal_path(tmp.path())).unwrap();
        for line in raw.lines() {
            assert!(
                serde_json::from_str::<serde_json::Value>(line).is_ok(),
                "mixed/corrupt line: {line}"
            );
        }
    }
}
