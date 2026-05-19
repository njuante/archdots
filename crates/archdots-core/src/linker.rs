//! Atomic linker: orchestrates snapshot + journal + lock to materialize
//! a profile's symlinks transactionally.
//!
//! High-level flow (`apply`):
//!
//! 1. Check `Journal::orphaned_in_progress` for the same profile.
//! 2. Acquire the per-state-home [`ApplyLock`].
//! 3. Append an `InProgress` journal entry (`snapshot_id` = null).
//! 4. Create the snapshot covering every target the apply will touch.
//! 5. Append a second `InProgress` entry chained via `supersedes`,
//!    now carrying the snapshot id.
//! 6. Iterate the plan, replacing each target atomically with
//!    `symlinkat` + `renameat` + `fsync(parent)`.
//! 7. On any failure, restore the snapshot, append a `Partial`/`Failed`
//!    closing entry, and report `rolled_back: true`.
//! 8. On success, append a `Success` closing entry and report.
//!
//! The journal is append-only; "patching" an entry is implemented as a
//! new entry whose `supersedes` field points at the prior one. Readers
//! reconstruct the current state by walking that chain.

use std::{
    fs, io,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};

use time::OffsetDateTime;
use ulid::Ulid;

use crate::{
    error::{CoreError, JournalError, LinkerError},
    journal::{
        Journal, JournalAction, JournalEntry, JournalId, JournalStatus, LinkOutcome, LinkRecord,
        PriorState, MAX_LINKS,
    },
    lock::ApplyLock,
    snapshot::{CreateRequest, RestoreOptions, SnapshotId, SnapshotManager, SnapshotTrigger},
};

// ─── Public types ─────────────────────────────────────────────────────────────

/// One source → target link request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpec {
    /// Absolute path inside the dotfile repository.
    pub source: PathBuf,
    /// Absolute filesystem path to be linked.
    pub target: PathBuf,
}

/// Classification of one link computed by [`Linker::plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedLink {
    /// Original spec.
    pub spec: LinkSpec,
    /// What `apply` will do with this target.
    pub disposition: LinkDisposition,
    /// Filesystem state of `target` at plan time.
    pub prior_state: PriorState,
}

/// What `apply` will do with a planned target.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkDisposition {
    /// Target does not exist; a new symlink will be created.
    Create,
    /// Target is a regular file that will be replaced.
    ReplaceFile,
    /// Target is a symlink to another location and will be replaced.
    ReplaceSymlink {
        /// Current symlink destination.
        current_target: PathBuf,
    },
    /// Target is already a symlink to our source — idempotent no-op.
    AlreadyOwned,
    /// The plan refuses this item; see [`ConflictReason`].
    Conflict(ConflictReason),
    /// Reserved: never emitted by the current planner.
    SkipDir,
}

/// Reasons a [`PlannedLink`] is in conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConflictReason {
    /// The parent directory of `target` does not exist.
    ParentMissing,
    /// The parent directory of `target` is not writable.
    PermissionDenied,
    /// `target` is outside the user's `$HOME`.
    OutsideHome,
    /// `source` does not exist.
    SourceMissing,
    /// `target` exists and is a directory.
    TargetIsDirectory,
}

/// Result of [`Linker::plan`].
#[derive(Debug, Clone)]
pub struct LinkPlan {
    /// Profile this plan was built for.
    pub profile: String,
    /// One entry per input [`LinkSpec`], in the same order.
    pub items: Vec<PlannedLink>,
}

/// Options driving [`Linker::apply`] and [`Linker::rollback`].
#[derive(Debug, Clone)]
pub struct ApplyOptions {
    /// If `true`, classify only — never mutate the filesystem, the journal,
    /// the snapshot store, or the lock.
    pub dry_run: bool,
    /// If `true`, ignore conflicts surfaced by the planner and proceed.
    pub force: bool,
    /// User `$HOME`, used by the planner to detect `OutsideHome`.
    pub home: PathBuf,
    /// State directory hosting the lockfile (typically `$XDG_STATE_HOME`).
    pub state_home: PathBuf,
}

/// Outcome of [`Linker::apply`] or [`Linker::rollback`].
#[derive(Debug)]
pub struct ApplyReport {
    /// Id of the initial `InProgress` journal entry that anchors the
    /// transaction. Subsequent state changes appear as further entries
    /// linked via `supersedes`.
    pub journal_id: JournalId,
    /// Snapshot taken before the mutation, if any.
    pub snapshot_id: Option<SnapshotId>,
    /// One [`LinkRecord`] per planned item (including skipped/failed).
    pub applied: Vec<LinkRecord>,
    /// `true` if the snapshot was restored mid-flight due to a failure.
    pub rolled_back: bool,
}

// ─── Linker ───────────────────────────────────────────────────────────────────

/// Atomic link planner and applier.
pub struct Linker<'a> {
    snapshots: &'a SnapshotManager,
    journal: &'a Journal,
}

impl<'a> Linker<'a> {
    /// Construct a new linker over the given snapshot store and journal.
    #[must_use]
    pub fn new(snapshots: &'a SnapshotManager, journal: &'a Journal) -> Self {
        Self { snapshots, journal }
    }

    /// Read-only classification of every spec.
    ///
    /// Does not touch the journal, the snapshot store, or any lockfile.
    /// `home` is required to detect [`ConflictReason::OutsideHome`].
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Io`] only if a non-`NotFound` filesystem error
    /// arises while stat-ing a target.
    #[allow(clippy::unused_self)]
    pub fn plan(
        &self,
        profile: &str,
        specs: &[LinkSpec],
        home: &Path,
    ) -> Result<LinkPlan, CoreError> {
        let mut items = Vec::with_capacity(specs.len());
        for spec in specs {
            items.push(classify(spec, home)?);
        }
        Ok(LinkPlan {
            profile: profile.to_owned(),
            items,
        })
    }

    /// Consume a plan and execute it as an atomic transaction.
    ///
    /// Acquires the apply lock internally. Releases it when this call
    /// returns (success or error).
    ///
    /// # Errors
    ///
    /// - [`LinkerError::ConflictsDetected`] — plan has conflicts and
    ///   `opts.force` is `false`.
    /// - [`LinkerError::OrphanedTransaction`] — a previous in-progress
    ///   transaction for this profile is still open.
    /// - [`crate::error::LockError::Busy`] — another process holds the lock.
    /// - [`JournalError::TooManyLinks`] — more than [`MAX_LINKS`] items.
    /// - I/O / snapshot / journal errors as appropriate.
    #[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
    pub fn apply(&self, plan: LinkPlan, opts: ApplyOptions) -> Result<ApplyReport, CoreError> {
        // Hard cap matches journal's per-line ceiling.
        if plan.items.len() > MAX_LINKS {
            return Err(JournalError::TooManyLinks {
                found: plan.items.len(),
                max: MAX_LINKS,
            }
            .into());
        }

        let conflicts: Vec<(PathBuf, ConflictReason)> = plan
            .items
            .iter()
            .filter_map(|p| match p.disposition {
                LinkDisposition::Conflict(r) => Some((p.spec.target.clone(), r)),
                _ => None,
            })
            .collect();

        if !conflicts.is_empty() && !opts.force {
            return Err(LinkerError::ConflictsDetected { conflicts }.into());
        }

        // --dry-run terminates here: no lock, no journal, no snapshot.
        if opts.dry_run {
            return Ok(ApplyReport {
                journal_id: JournalId::new(),
                snapshot_id: None,
                applied: Vec::new(),
                rolled_back: false,
            });
        }

        let _lock = ApplyLock::acquire(&opts.state_home)?;

        // Orphan check is intentionally AFTER lock acquire. A concurrent apply
        // that holds the lock would have a live InProgress entry; checking before
        // the lock would misidentify it as orphaned and abort with the wrong error.
        let orphans = self.journal.orphaned_in_progress()?;
        if let Some(o) = orphans.into_iter().find(|e| e.profile == plan.profile) {
            return Err(LinkerError::OrphanedTransaction { id: o.id }.into());
        }

        // 1. Initial InProgress entry — no snapshot yet.
        let initial_id = JournalId::new();
        self.journal.append(&JournalEntry {
            schema_version: 1,
            id: initial_id.clone(),
            ts: OffsetDateTime::now_utc(),
            profile: plan.profile.clone(),
            action: JournalAction::Apply,
            snapshot_id: None,
            links: Vec::new(),
            status: JournalStatus::InProgress,
            error: None,
            supersedes: None,
        })?;

        // 2. Targets to snapshot: everything we may mutate.
        let snapshot_targets: Vec<PathBuf> = plan
            .items
            .iter()
            .filter(|p| !matches!(p.disposition, LinkDisposition::AlreadyOwned))
            .map(|p| p.spec.target.clone())
            .collect();

        let snap = match self.snapshots.create(CreateRequest {
            profile: &plan.profile,
            trigger: SnapshotTrigger::PreApply,
            targets: &snapshot_targets,
        }) {
            Ok(s) => s,
            Err(e) => {
                // Close the transaction as Failed; no snapshot to reference.
                let _ = self.journal.append(&JournalEntry {
                    schema_version: 1,
                    id: JournalId::new(),
                    ts: OffsetDateTime::now_utc(),
                    profile: plan.profile.clone(),
                    action: JournalAction::Apply,
                    snapshot_id: None,
                    links: Vec::new(),
                    status: JournalStatus::Failed,
                    error: Some(format!("snapshot creation failed: {e}")),
                    supersedes: Some(initial_id.clone()),
                });
                return Err(e);
            }
        };

        // 3. Second InProgress entry carries the snapshot id.
        let mid_id = JournalId::new();
        self.journal.append(&JournalEntry {
            schema_version: 1,
            id: mid_id.clone(),
            ts: OffsetDateTime::now_utc(),
            profile: plan.profile.clone(),
            action: JournalAction::Apply,
            snapshot_id: Some(snap.id.clone()),
            links: Vec::new(),
            status: JournalStatus::InProgress,
            error: None,
            supersedes: Some(initial_id.clone()),
        })?;

        // 4. Apply each item in order.
        let mut records: Vec<LinkRecord> = Vec::with_capacity(plan.items.len());
        let mut failure: Option<String> = None;

        for item in &plan.items {
            let needs_link = !matches!(item.disposition, LinkDisposition::AlreadyOwned);
            if !needs_link {
                records.push(LinkRecord {
                    source: item.spec.source.clone(),
                    target: item.spec.target.clone(),
                    prior_state: item.prior_state.clone(),
                    outcome: LinkOutcome::Skipped,
                });
                continue;
            }

            match atomic_symlink(&item.spec.source, &item.spec.target) {
                Ok(()) => records.push(LinkRecord {
                    source: item.spec.source.clone(),
                    target: item.spec.target.clone(),
                    prior_state: item.prior_state.clone(),
                    outcome: LinkOutcome::Linked,
                }),
                Err(e) => {
                    let reason = e.to_string();
                    records.push(LinkRecord {
                        source: item.spec.source.clone(),
                        target: item.spec.target.clone(),
                        prior_state: item.prior_state.clone(),
                        outcome: LinkOutcome::Failed(reason.clone()),
                    });
                    failure = Some(format!(
                        "link failed for {}: {reason}",
                        item.spec.target.display()
                    ));
                    break;
                }
            }
        }

        // 5. Finalize.
        if let Some(err_msg) = failure {
            // Restore everything we touched from the snapshot.
            let _ = self.snapshots.restore(
                &snap.id,
                RestoreOptions {
                    dry_run: false,
                    continue_on_error: true,
                },
            );

            self.journal.append(&JournalEntry {
                schema_version: 1,
                id: JournalId::new(),
                ts: OffsetDateTime::now_utc(),
                profile: plan.profile.clone(),
                action: JournalAction::Apply,
                snapshot_id: Some(snap.id.clone()),
                links: records.clone(),
                status: JournalStatus::Partial,
                error: Some(err_msg),
                supersedes: Some(mid_id),
            })?;

            return Ok(ApplyReport {
                journal_id: initial_id,
                snapshot_id: Some(snap.id),
                applied: records,
                rolled_back: true,
            });
        }

        self.journal.append(&JournalEntry {
            schema_version: 1,
            id: JournalId::new(),
            ts: OffsetDateTime::now_utc(),
            profile: plan.profile.clone(),
            action: JournalAction::Apply,
            snapshot_id: Some(snap.id.clone()),
            links: records.clone(),
            status: JournalStatus::Success,
            error: None,
            supersedes: Some(mid_id),
        })?;

        Ok(ApplyReport {
            journal_id: initial_id,
            snapshot_id: Some(snap.id),
            applied: records,
            rolled_back: false,
        })
    }

    /// Re-apply the snapshot of the most recent successful `Apply` for
    /// `profile`. Takes the lock internally.
    ///
    /// # Errors
    ///
    /// - [`LinkerError::NoTransactionToRollback`] if the profile has no
    ///   prior `Success` entry.
    /// - [`crate::error::LockError::Busy`] / snapshot / journal errors.
    #[allow(clippy::needless_pass_by_value)]
    pub fn rollback(&self, profile: &str, opts: ApplyOptions) -> Result<ApplyReport, CoreError> {
        // Find latest Apply/Success entry for the profile (forward scan;
        // last write wins).
        let mut target: Option<JournalEntry> = None;
        for entry in (self.journal.iter()?).flatten() {
            if entry.profile == profile
                && entry.action == JournalAction::Apply
                && entry.status == JournalStatus::Success
                && entry.snapshot_id.is_some()
            {
                target = Some(entry);
            }
        }

        let target = target.ok_or_else(|| LinkerError::NoTransactionToRollback {
            profile: profile.to_owned(),
        })?;
        let snapshot_id =
            target
                .snapshot_id
                .clone()
                .ok_or_else(|| LinkerError::NoTransactionToRollback {
                    profile: profile.to_owned(),
                })?;

        let _lock = ApplyLock::acquire(&opts.state_home)?;

        // Refuse to roll back if there is an orphaned transaction for this
        // profile — the user must run `archdots recover` first.
        let orphans = self.journal.orphaned_in_progress()?;
        if let Some(o) = orphans.into_iter().find(|e| e.profile == profile) {
            return Err(LinkerError::OrphanedTransaction { id: o.id }.into());
        }

        let initial_id = JournalId::new();
        self.journal.append(&JournalEntry {
            schema_version: 1,
            id: initial_id.clone(),
            ts: OffsetDateTime::now_utc(),
            profile: profile.to_owned(),
            action: JournalAction::Rollback,
            snapshot_id: Some(snapshot_id.clone()),
            links: Vec::new(),
            status: JournalStatus::InProgress,
            error: None,
            supersedes: None,
        })?;

        let restore_result = self.snapshots.restore(
            &snapshot_id,
            RestoreOptions {
                dry_run: false,
                continue_on_error: true,
            },
        );

        match restore_result {
            Ok(rep) => {
                let links: Vec<LinkRecord> = rep
                    .restored
                    .iter()
                    .map(|p| LinkRecord {
                        source: PathBuf::new(),
                        target: p.clone(),
                        prior_state: PriorState::Absent,
                        outcome: LinkOutcome::Linked,
                    })
                    .collect();

                let status = if rep.skipped.is_empty() {
                    JournalStatus::Success
                } else {
                    JournalStatus::Partial
                };

                self.journal.append(&JournalEntry {
                    schema_version: 1,
                    id: JournalId::new(),
                    ts: OffsetDateTime::now_utc(),
                    profile: profile.to_owned(),
                    action: JournalAction::Rollback,
                    snapshot_id: Some(snapshot_id.clone()),
                    links: links.clone(),
                    status,
                    error: if rep.skipped.is_empty() {
                        None
                    } else {
                        Some(format!("{} entries skipped", rep.skipped.len()))
                    },
                    supersedes: Some(initial_id.clone()),
                })?;

                Ok(ApplyReport {
                    journal_id: initial_id,
                    snapshot_id: Some(snapshot_id),
                    applied: links,
                    rolled_back: true,
                })
            }
            Err(e) => {
                self.journal.append(&JournalEntry {
                    schema_version: 1,
                    id: JournalId::new(),
                    ts: OffsetDateTime::now_utc(),
                    profile: profile.to_owned(),
                    action: JournalAction::Rollback,
                    snapshot_id: Some(snapshot_id),
                    links: Vec::new(),
                    status: JournalStatus::Failed,
                    error: Some(e.to_string()),
                    supersedes: Some(initial_id),
                })?;
                Err(e)
            }
        }
    }
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

fn classify(spec: &LinkSpec, home: &Path) -> Result<PlannedLink, CoreError> {
    let prior = read_prior_state(&spec.target)?;

    // 1. Source must exist.
    if !path_lexists(&spec.source) {
        return Ok(PlannedLink {
            spec: spec.clone(),
            disposition: LinkDisposition::Conflict(ConflictReason::SourceMissing),
            prior_state: prior,
        });
    }

    // 2. Target must live under $HOME.
    if !spec.target.starts_with(home) {
        return Ok(PlannedLink {
            spec: spec.clone(),
            disposition: LinkDisposition::Conflict(ConflictReason::OutsideHome),
            prior_state: prior,
        });
    }

    // 3. Parent must exist.
    let parent = match spec.target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => {
            return Ok(PlannedLink {
                spec: spec.clone(),
                disposition: LinkDisposition::Conflict(ConflictReason::ParentMissing),
                prior_state: prior,
            });
        }
    };
    if !parent.exists() {
        return Ok(PlannedLink {
            spec: spec.clone(),
            disposition: LinkDisposition::Conflict(ConflictReason::ParentMissing),
            prior_state: prior,
        });
    }

    // 4. Parent must be writable.
    if !is_writable(parent) {
        return Ok(PlannedLink {
            spec: spec.clone(),
            disposition: LinkDisposition::Conflict(ConflictReason::PermissionDenied),
            prior_state: prior,
        });
    }

    // 5. Classify by target state.
    let disposition = match &prior {
        PriorState::Absent => LinkDisposition::Create,
        PriorState::Dir => LinkDisposition::Conflict(ConflictReason::TargetIsDirectory),
        PriorState::File => LinkDisposition::ReplaceFile,
        PriorState::Symlink { points_to } => {
            if symlink_owned_by(points_to, &spec.source) {
                LinkDisposition::AlreadyOwned
            } else {
                LinkDisposition::ReplaceSymlink {
                    current_target: points_to.clone(),
                }
            }
        }
    };

    Ok(PlannedLink {
        spec: spec.clone(),
        disposition,
        prior_state: prior,
    })
}

fn read_prior_state(target: &Path) -> Result<PriorState, CoreError> {
    match target.symlink_metadata() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(PriorState::Absent),
        Err(e) => Err(e.into()),
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() {
                let dest = fs::read_link(target).map_err(CoreError::Io)?;
                Ok(PriorState::Symlink { points_to: dest })
            } else if ft.is_dir() {
                Ok(PriorState::Dir)
            } else {
                Ok(PriorState::File)
            }
        }
    }
}

fn path_lexists(p: &Path) -> bool {
    p.symlink_metadata().is_ok()
}

fn is_writable(p: &Path) -> bool {
    rustix::fs::access(p, rustix::fs::Access::WRITE_OK).is_ok()
}

/// `true` when an existing symlink's destination matches our intended source.
/// Compares both the raw stored target (relative or absolute) and the canonical
/// resolution, so symlinks created with either form are detected.
fn symlink_owned_by(points_to: &Path, source: &Path) -> bool {
    if points_to == source {
        return true;
    }
    if let (Ok(a), Ok(b)) = (fs::canonicalize(points_to), fs::canonicalize(source)) {
        return a == b;
    }
    false
}

/// Atomically replace `target` with a symlink to `source`.
///
/// 1. Create a sibling tmp symlink (same dir → same FS → rename is atomic).
/// 2. `rename(tmp, target)` — replaces atomically per `rename(2)`.
/// 3. `fsync` the parent directory for durability.
fn atomic_symlink(source: &Path, target: &Path) -> io::Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no parent"))?;
    let filename = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no filename"))?;
    let rand = Ulid::new();
    let tmp_name = format!(".{}.archdots.{rand}", filename.to_string_lossy());
    let tmp_path = parent.join(tmp_name);

    // Defensive: clear a stale tmp leftover (rare; would only occur after a
    // SIGKILL between symlinkat and renameat).
    if tmp_path.symlink_metadata().is_ok() {
        let _ = fs::remove_file(&tmp_path);
    }

    symlink(source, &tmp_path)?;
    if let Err(e) = fs::rename(&tmp_path, target) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    let dir = fs::File::open(parent)?;
    dir.sync_all()?;
    Ok(())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    struct Restore {
        path: PathBuf,
        mode: u32,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode));
        }
    }

    struct Env {
        home: TempDir,
        data: TempDir,
        state: TempDir,
        snaps: SnapshotManager,
        journal: Journal,
    }

    fn env() -> Env {
        let home = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let snaps = SnapshotManager::open(data.path()).unwrap();
        let journal = Journal::open(state.path()).unwrap();
        Env {
            home,
            data,
            state,
            snaps,
            journal,
        }
    }

    fn opts(home: &Path, state_home: &Path) -> ApplyOptions {
        ApplyOptions {
            dry_run: false,
            force: false,
            home: home.to_path_buf(),
            state_home: state_home.to_path_buf(),
        }
    }

    fn write_file(p: &Path, content: &[u8]) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, content).unwrap();
    }

    // ── PLAN ─────────────────────────────────────────────────────────────────

    #[test]
    fn plan_create_for_absent_target() {
        let e = env();
        let src = e.home.path().join("src");
        write_file(&src, b"hi");
        let tgt = e.home.path().join("tgt");

        let linker = Linker::new(&e.snaps, &e.journal);
        let plan = linker
            .plan(
                "p",
                &[LinkSpec {
                    source: src,
                    target: tgt,
                }],
                e.home.path(),
            )
            .unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].disposition, LinkDisposition::Create);
        assert_eq!(plan.items[0].prior_state, PriorState::Absent);
    }

    #[test]
    fn plan_already_owned_when_symlink_matches() {
        let e = env();
        let src = e.home.path().join("src");
        write_file(&src, b"x");
        let tgt = e.home.path().join("tgt");
        symlink(&src, &tgt).unwrap();

        let linker = Linker::new(&e.snaps, &e.journal);
        let plan = linker
            .plan(
                "p",
                &[LinkSpec {
                    source: src,
                    target: tgt,
                }],
                e.home.path(),
            )
            .unwrap();
        assert_eq!(plan.items[0].disposition, LinkDisposition::AlreadyOwned);
    }

    #[test]
    fn plan_replace_symlink_when_pointing_elsewhere() {
        let e = env();
        let src = e.home.path().join("src");
        write_file(&src, b"x");
        let other = e.home.path().join("other");
        write_file(&other, b"y");
        let tgt = e.home.path().join("tgt");
        symlink(&other, &tgt).unwrap();

        let linker = Linker::new(&e.snaps, &e.journal);
        let plan = linker
            .plan(
                "p",
                &[LinkSpec {
                    source: src,
                    target: tgt,
                }],
                e.home.path(),
            )
            .unwrap();
        match &plan.items[0].disposition {
            LinkDisposition::ReplaceSymlink { current_target } => {
                assert_eq!(current_target, &other);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn plan_replace_file() {
        let e = env();
        let src = e.home.path().join("src");
        write_file(&src, b"x");
        let tgt = e.home.path().join("tgt");
        write_file(&tgt, b"old");

        let linker = Linker::new(&e.snaps, &e.journal);
        let plan = linker
            .plan(
                "p",
                &[LinkSpec {
                    source: src,
                    target: tgt,
                }],
                e.home.path(),
            )
            .unwrap();
        assert_eq!(plan.items[0].disposition, LinkDisposition::ReplaceFile);
    }

    #[test]
    fn plan_target_is_directory_conflict() {
        let e = env();
        let src = e.home.path().join("src");
        write_file(&src, b"x");
        let tgt = e.home.path().join("tgt_dir");
        fs::create_dir(&tgt).unwrap();

        let linker = Linker::new(&e.snaps, &e.journal);
        let plan = linker
            .plan(
                "p",
                &[LinkSpec {
                    source: src,
                    target: tgt,
                }],
                e.home.path(),
            )
            .unwrap();
        assert_eq!(
            plan.items[0].disposition,
            LinkDisposition::Conflict(ConflictReason::TargetIsDirectory)
        );
    }

    #[test]
    fn plan_outside_home_conflict() {
        let e = env();
        let src = e.home.path().join("src");
        write_file(&src, b"x");
        let other = TempDir::new().unwrap();
        let tgt = other.path().join("tgt");

        let linker = Linker::new(&e.snaps, &e.journal);
        let plan = linker
            .plan(
                "p",
                &[LinkSpec {
                    source: src,
                    target: tgt,
                }],
                e.home.path(),
            )
            .unwrap();
        assert_eq!(
            plan.items[0].disposition,
            LinkDisposition::Conflict(ConflictReason::OutsideHome)
        );
    }

    #[test]
    fn plan_parent_missing_conflict() {
        let e = env();
        let src = e.home.path().join("src");
        write_file(&src, b"x");
        let tgt = e.home.path().join("nope/sub/tgt");

        let linker = Linker::new(&e.snaps, &e.journal);
        let plan = linker
            .plan(
                "p",
                &[LinkSpec {
                    source: src,
                    target: tgt,
                }],
                e.home.path(),
            )
            .unwrap();
        assert_eq!(
            plan.items[0].disposition,
            LinkDisposition::Conflict(ConflictReason::ParentMissing)
        );
    }

    #[test]
    fn plan_source_missing_conflict() {
        let e = env();
        let src = e.home.path().join("missing");
        let tgt = e.home.path().join("tgt");

        let linker = Linker::new(&e.snaps, &e.journal);
        let plan = linker
            .plan(
                "p",
                &[LinkSpec {
                    source: src,
                    target: tgt,
                }],
                e.home.path(),
            )
            .unwrap();
        assert_eq!(
            plan.items[0].disposition,
            LinkDisposition::Conflict(ConflictReason::SourceMissing)
        );
    }

    // ── APPLY ────────────────────────────────────────────────────────────────

    #[test]
    fn apply_happy_path_three_links() {
        let e = env();
        let specs: Vec<LinkSpec> = (0..3)
            .map(|i| {
                let src = e.home.path().join(format!("src{i}"));
                write_file(&src, format!("content{i}").as_bytes());
                let tgt = e.home.path().join(format!("tgt{i}"));
                LinkSpec {
                    source: src,
                    target: tgt,
                }
            })
            .collect();

        let linker = Linker::new(&e.snaps, &e.journal);
        let plan = linker.plan("p", &specs, e.home.path()).unwrap();
        let opts = opts(e.home.path(), e.state.path());

        let report = linker.apply(plan, opts).unwrap();
        assert!(!report.rolled_back);
        assert!(report.snapshot_id.is_some());
        assert_eq!(report.applied.len(), 3);
        for r in &report.applied {
            assert!(matches!(r.outcome, LinkOutcome::Linked));
        }

        // All targets are symlinks pointing at their source.
        for (i, spec) in specs.iter().enumerate() {
            let meta = fs::symlink_metadata(&spec.target).unwrap();
            assert!(meta.file_type().is_symlink(), "tgt{i} not a symlink");
            assert_eq!(fs::read_link(&spec.target).unwrap(), spec.source);
        }

        // Journal: initial InProgress + mid InProgress + final Success = 3 entries.
        let entries: Vec<JournalEntry> = e.journal.iter().unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].status, JournalStatus::InProgress);
        assert!(entries[0].snapshot_id.is_none());
        assert_eq!(entries[1].status, JournalStatus::InProgress);
        assert!(entries[1].snapshot_id.is_some());
        assert_eq!(entries[2].status, JournalStatus::Success);
    }

    #[test]
    fn apply_dry_run_touches_nothing() {
        let e = env();
        let src = e.home.path().join("src");
        write_file(&src, b"x");
        let tgt = e.home.path().join("tgt");

        let linker = Linker::new(&e.snaps, &e.journal);
        let plan = linker
            .plan(
                "p",
                &[LinkSpec {
                    source: src.clone(),
                    target: tgt.clone(),
                }],
                e.home.path(),
            )
            .unwrap();

        let mut o = opts(e.home.path(), e.state.path());
        o.dry_run = true;

        let report = linker.apply(plan, o).unwrap();
        assert_eq!(report.applied.len(), 0);
        assert!(report.snapshot_id.is_none());

        assert!(!tgt.exists(), "dry-run must not create target");
        let journal_path = e.state.path().join("archdots").join("journal.jsonl");
        assert!(
            !journal_path.exists(),
            "dry-run must not create the journal file"
        );
        assert!(
            fs::read_dir(e.snaps_root()).unwrap().next().is_none(),
            "dry-run must not create a snapshot"
        );
        assert!(
            !e.state.path().join("lock").exists(),
            "dry-run must not touch the lock"
        );
    }

    #[test]
    fn apply_refuses_on_conflict_without_force() {
        let e = env();
        let src = e.home.path().join("src");
        write_file(&src, b"x");
        let other = TempDir::new().unwrap();
        let tgt = other.path().join("tgt"); // OutsideHome

        let linker = Linker::new(&e.snaps, &e.journal);
        let plan = linker
            .plan(
                "p",
                &[LinkSpec {
                    source: src,
                    target: tgt.clone(),
                }],
                e.home.path(),
            )
            .unwrap();

        let result = linker.apply(plan, opts(e.home.path(), e.state.path()));
        assert!(matches!(
            result,
            Err(CoreError::Linker(LinkerError::ConflictsDetected { .. }))
        ));
        assert!(!tgt.exists());
        let journal_path = e.state.path().join("archdots").join("journal.jsonl");
        assert!(!journal_path.exists());
    }

    #[test]
    fn apply_force_proceeds_through_source_missing_conflict() {
        let e = env();
        let src = e.home.path().join("missing");
        let tgt = e.home.path().join("tgt");

        let linker = Linker::new(&e.snaps, &e.journal);
        let plan = linker
            .plan(
                "p",
                &[LinkSpec {
                    source: src.clone(),
                    target: tgt.clone(),
                }],
                e.home.path(),
            )
            .unwrap();

        let mut o = opts(e.home.path(), e.state.path());
        o.force = true;
        let report = linker.apply(plan, o).unwrap();
        assert!(!report.rolled_back);
        // Result: target is a dangling symlink.
        assert!(fs::symlink_metadata(&tgt).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(&tgt).unwrap(), src);
    }

    #[test]
    fn apply_is_idempotent_for_already_owned() {
        let e = env();
        let src = e.home.path().join("src");
        write_file(&src, b"x");
        let tgt = e.home.path().join("tgt");

        let linker = Linker::new(&e.snaps, &e.journal);
        let specs = [LinkSpec {
            source: src,
            target: tgt,
        }];

        let plan1 = linker.plan("p", &specs, e.home.path()).unwrap();
        linker
            .apply(plan1, opts(e.home.path(), e.state.path()))
            .unwrap();

        // Second apply: every item is AlreadyOwned.
        let plan2 = linker.plan("p", &specs, e.home.path()).unwrap();
        assert_eq!(plan2.items[0].disposition, LinkDisposition::AlreadyOwned);
        let report = linker
            .apply(plan2, opts(e.home.path(), e.state.path()))
            .unwrap();
        assert!(!report.rolled_back);
        assert_eq!(report.applied.len(), 1);
        assert!(matches!(report.applied[0].outcome, LinkOutcome::Skipped));
    }

    #[test]
    fn apply_concurrent_lock_returns_busy() {
        let e = env();
        let src = e.home.path().join("src");
        write_file(&src, b"x");
        let tgt = e.home.path().join("tgt");

        // Hold the lock from another thread for the duration of the apply.
        let state_home = e.state.path().to_path_buf();
        let (tx_ready, rx_ready) = std::sync::mpsc::channel();
        let (tx_release, rx_release) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _lock = ApplyLock::acquire(&state_home).unwrap();
            tx_ready.send(()).unwrap();
            rx_release.recv().unwrap();
        });
        rx_ready.recv().unwrap();

        let linker = Linker::new(&e.snaps, &e.journal);
        let plan = linker
            .plan(
                "p",
                &[LinkSpec {
                    source: src,
                    target: tgt,
                }],
                e.home.path(),
            )
            .unwrap();
        let result = linker.apply(plan, opts(e.home.path(), e.state.path()));
        assert!(
            matches!(result, Err(CoreError::Lock(_))),
            "expected Lock error, got {result:?}"
        );

        tx_release.send(()).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn apply_refuses_when_orphan_exists() {
        let e = env();
        // Inject an orphan InProgress entry for profile "p".
        e.journal
            .append(&JournalEntry {
                schema_version: 1,
                id: JournalId::new(),
                ts: OffsetDateTime::now_utc(),
                profile: "p".to_owned(),
                action: JournalAction::Apply,
                snapshot_id: None,
                links: Vec::new(),
                status: JournalStatus::InProgress,
                error: None,
                supersedes: None,
            })
            .unwrap();

        let src = e.home.path().join("src");
        write_file(&src, b"x");
        let tgt = e.home.path().join("tgt");

        let linker = Linker::new(&e.snaps, &e.journal);
        let plan = linker
            .plan(
                "p",
                &[LinkSpec {
                    source: src,
                    target: tgt.clone(),
                }],
                e.home.path(),
            )
            .unwrap();

        let result = linker.apply(plan, opts(e.home.path(), e.state.path()));
        assert!(matches!(
            result,
            Err(CoreError::Linker(LinkerError::OrphanedTransaction { .. }))
        ));
        assert!(!tgt.exists());
    }

    // ── ROLLBACK ─────────────────────────────────────────────────────────────

    #[test]
    fn rollback_restores_original_state() {
        let e = env();
        let src = e.home.path().join("src");
        write_file(&src, b"new content");
        let tgt = e.home.path().join("tgt");
        write_file(&tgt, b"original content");

        let linker = Linker::new(&e.snaps, &e.journal);
        let specs = [LinkSpec {
            source: src,
            target: tgt.clone(),
        }];
        linker
            .apply(
                linker.plan("p", &specs, e.home.path()).unwrap(),
                opts(e.home.path(), e.state.path()),
            )
            .unwrap();
        // Sanity: target is now a symlink.
        assert!(fs::symlink_metadata(&tgt).unwrap().file_type().is_symlink());

        let report = linker
            .rollback("p", opts(e.home.path(), e.state.path()))
            .unwrap();
        assert!(report.rolled_back);

        // Target is back to a regular file with original content.
        let meta = fs::symlink_metadata(&tgt).unwrap();
        assert!(!meta.file_type().is_symlink());
        assert_eq!(fs::read(&tgt).unwrap(), b"original content");
    }

    #[test]
    fn rollback_without_prior_success_errors() {
        let e = env();
        let linker = Linker::new(&e.snaps, &e.journal);
        let result = linker.rollback("nope", opts(e.home.path(), e.state.path()));
        assert!(matches!(
            result,
            Err(CoreError::Linker(
                LinkerError::NoTransactionToRollback { .. }
            ))
        ));
    }

    #[test]
    fn rollback_ignores_post_apply_user_edits() {
        let e = env();
        let src = e.home.path().join("src");
        write_file(&src, b"src content");
        let tgt = e.home.path().join("tgt");
        write_file(&tgt, b"original");

        let linker = Linker::new(&e.snaps, &e.journal);
        let specs = [LinkSpec {
            source: src,
            target: tgt.clone(),
        }];
        linker
            .apply(
                linker.plan("p", &specs, e.home.path()).unwrap(),
                opts(e.home.path(), e.state.path()),
            )
            .unwrap();

        // User manually replaces the link with their own file.
        fs::remove_file(&tgt).unwrap();
        write_file(&tgt, b"user edit");

        linker
            .rollback("p", opts(e.home.path(), e.state.path()))
            .unwrap();
        assert_eq!(fs::read(&tgt).unwrap(), b"original");
    }

    // ── MID-APPLY FAILURE & FULL ROLLBACK ────────────────────────────────────

    #[test]
    fn apply_failure_mid_flight_triggers_rollback() {
        let e = env();

        // Three targets, all originally regular files (so snapshot has content
        // to restore).
        let mut specs = Vec::new();
        let mut parents = Vec::new();
        for i in 0..3 {
            let src = e.home.path().join(format!("src{i}"));
            write_file(&src, format!("src{i}").as_bytes());
            let parent = e.home.path().join(format!("sub{i}"));
            fs::create_dir_all(&parent).unwrap();
            let tgt = parent.join("tgt");
            write_file(&tgt, format!("orig{i}").as_bytes());
            specs.push(LinkSpec {
                source: src,
                target: tgt,
            });
            parents.push(parent);
        }

        let linker = Linker::new(&e.snaps, &e.journal);
        let plan = linker.plan("p", &specs, e.home.path()).unwrap();
        assert!(plan
            .items
            .iter()
            .all(|p| p.disposition == LinkDisposition::ReplaceFile));

        // Make the parent of the second target read-only AFTER plan, so
        // symlinkat fails for that one item but the first item succeeds.
        let _restore = Restore {
            path: parents[1].clone(),
            mode: 0o755,
        };
        fs::set_permissions(&parents[1], fs::Permissions::from_mode(0o555)).unwrap();

        let report = linker
            .apply(plan, opts(e.home.path(), e.state.path()))
            .unwrap();
        assert!(report.rolled_back);
        assert!(report.snapshot_id.is_some());

        // Restore permissions so the first target is reachable for assertions
        // and TempDir cleanup.
        fs::set_permissions(&parents[1], fs::Permissions::from_mode(0o755)).unwrap();

        // First target reverted to its original content (not the symlink).
        let m0 = fs::symlink_metadata(&specs[0].target).unwrap();
        assert!(
            !m0.file_type().is_symlink(),
            "tgt0 must be reverted to a file"
        );
        assert_eq!(fs::read(&specs[0].target).unwrap(), b"orig0");

        // Final journal entry is Partial with snapshot_id set.
        let entries: Vec<JournalEntry> = e.journal.iter().unwrap().map(|r| r.unwrap()).collect();
        let last = entries.last().unwrap();
        assert_eq!(last.status, JournalStatus::Partial);
        assert!(last.snapshot_id.is_some());
    }

    // ── ORPHAN CHECK ORDERING ────────────────────────────────────────────────

    /// Regression test: a live apply (which holds the lock) must NOT be
    /// misidentified as an orphaned transaction.
    ///
    /// With the correct ordering (orphan check AFTER lock acquire):
    ///   - While A holds the lock, B fails with Lock::Busy — not OrphanedTransaction.
    ///   - After A releases the lock, the InProgress entry is a genuine orphan,
    ///     so B fails with OrphanedTransaction.
    #[test]
    fn apply_orphan_check_is_inside_lock() {
        let e = env();
        let src = e.home.path().join("src");
        write_file(&src, b"x");
        let tgt = e.home.path().join("tgt");

        // Thread A manually acquires the lock and injects an InProgress entry,
        // simulating a process that is mid-apply.
        let state_home = e.state.path().to_path_buf();
        let state_home2 = state_home.clone();
        let journal_path = e.state.path().join("archdots").join("journal.jsonl");

        let (tx_ready, rx_ready) = std::sync::mpsc::channel::<()>();
        let (tx_release, rx_release) = std::sync::mpsc::channel::<()>();

        let handle = std::thread::spawn(move || {
            // Acquire lock first, then write InProgress entry.
            let _lock = ApplyLock::acquire(&state_home2).unwrap();
            // Write InProgress entry directly to simulate a mid-apply state.
            let j = Journal::open(&state_home2).unwrap();
            j.append(&JournalEntry {
                schema_version: 1,
                id: JournalId::new(),
                ts: time::OffsetDateTime::now_utc(),
                profile: "p".to_owned(),
                action: JournalAction::Apply,
                snapshot_id: None,
                links: Vec::new(),
                status: JournalStatus::InProgress,
                error: None,
                supersedes: None,
            })
            .unwrap();
            tx_ready.send(()).unwrap();
            rx_release.recv().unwrap();
            // _lock dropped here, releasing the flock.
        });

        rx_ready.recv().unwrap();

        // Thread B tries to apply while A holds the lock.
        // Expected: Lock::Busy — NOT OrphanedTransaction.
        let linker = Linker::new(&e.snaps, &e.journal);
        let plan = linker
            .plan(
                "p",
                &[LinkSpec {
                    source: src.clone(),
                    target: tgt.clone(),
                }],
                e.home.path(),
            )
            .unwrap();
        let result_while_locked = linker.apply(plan, opts(e.home.path(), &state_home));
        assert!(
            matches!(result_while_locked, Err(CoreError::Lock(_))),
            "while A holds the lock, B must get Lock::Busy, got {result_while_locked:?}"
        );

        // Release A's lock.
        tx_release.send(()).unwrap();
        handle.join().unwrap();

        // Now the InProgress entry is a genuine orphan (no process holds the lock).
        // B should now fail with OrphanedTransaction, not Lock::Busy.
        let _ = journal_path; // ensure variable lives long enough for the comment
        let plan2 = linker
            .plan(
                "p",
                &[LinkSpec {
                    source: src,
                    target: tgt,
                }],
                e.home.path(),
            )
            .unwrap();
        let result_after_release = linker.apply(plan2, opts(e.home.path(), &state_home));
        assert!(
            matches!(
                result_after_release,
                Err(CoreError::Linker(LinkerError::OrphanedTransaction { .. }))
            ),
            "after A releases the lock, B must get OrphanedTransaction, got {result_after_release:?}"
        );
    }

    // ── helper accessors on Env ──────────────────────────────────────────────

    impl Env {
        fn snaps_root(&self) -> PathBuf {
            self.data.path().join("archdots").join("snapshots")
        }
    }
}
