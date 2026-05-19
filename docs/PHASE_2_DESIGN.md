# Phase 2 — Safe Apply with Rollback (Design)

Status: **approved** — implementation target for Phase 2.

This document specifies the design of the four components that make Phase 2
deliver a robust, reversible `apply`:

1. `archdots-core::snapshot::SnapshotManager`
2. `archdots-core::journal::Journal`
3. `archdots-core::linker::Linker`
4. `archdots-core::lock::ApplyLock`

All decisions below are final unless an ADR supersedes them.

---

## 1. Apply flow — states and transitions

```
            ┌──────────────────────────────────────────────┐
            │              archdots apply <profile>        │
            └──────────────────────────────────────────────┘
                                   │
                                   ▼
                         ┌──────────────────┐
                         │ RECOVERY CHECK   │  Journal::orphaned_in_progress()
                         │ (refuse if any)  │  → abort with "run archdots recover"
                         └──────────────────┘
                                   │
                                   ▼
                         ┌──────────────────┐
                         │   ACQUIRE LOCK   │  rustix::fs::flock(LOCK_EX|LOCK_NB)
                         └──────────────────┘
                                   │
                       fail ◄──────┤──────► ok
                         │                       │
                         ▼                       ▼
                  exit Busy{pid}          ┌────────────┐
                                          │   LOAD     │  parse profile.toml
                                          └────────────┘
                                                │
                                                ▼
                                          ┌────────────┐
                                          │   PLAN     │  read-only LinkPlan
                                          │            │  classify each link
                                          └────────────┘
                                                │
                            ┌───────────────────┼───────────────────┐
                       conflicts                no-op              ok
                            │                    │                  │
                            ▼                    ▼                  │
                    exit Conflict         exit 0 (nothing)          │
                                                                    │
                            (--dry-run terminates here, no lock     │
                             was needed — see §4.2)                 │
                                                                    ▼
                                                    ┌───────────────────────┐
                                                    │ JOURNAL APPEND        │
                                                    │ status=InProgress     │
                                                    │ snapshot_id=null      │
                                                    └───────────────────────┘
                                                                    │
                                                                    ▼
                                                    ┌───────────────────────┐
                                                    │ SNAPSHOT CREATE       │
                                                    │ tarball + sidecar     │
                                                    │ fsync(payload, dir)   │
                                                    └───────────────────────┘
                                                          │
                                  fail ──────────────────┤───────── ok
                                    │                              │
                                    ▼                              ▼
                            ┌──────────────┐              ┌───────────────────────┐
                            │ JOURNAL PATCH│              │ JOURNAL PATCH         │
                            │ status=Failed│              │ snapshot_id=<ULID>    │
                            │ (no snapshot)│              │ (still InProgress)    │
                            └──────────────┘              └───────────────────────┘
                                    │                              │
                                    ▼                              ▼
                            RELEASE LOCK                  ┌───────────────────────┐
                            exit IO                       │   APPLY (per link)    │
                                                          │ 1. symlinkat(tmp)     │
                                                          │ 2. renameat(tmp→tgt)  │
                                                          │ 3. fsync(parent dir)  │
                                                          │ 4. record outcome    │
                                                          └───────────────────────┘
                                                                    │
                                          ┌─────────────────────────┤
                                          │                         │
                                        fail                       ok
                                          │                         │
                                          ▼                         ▼
                                  ┌──────────────┐         ┌───────────────────────┐
                                  │  ROLLBACK    │         │ JOURNAL PATCH         │
                                  │  restore     │         │ status=Success        │
                                  │  snapshot    │         │ links=[...]           │
                                  └──────────────┘         └───────────────────────┘
                                          │                         │
                                          ▼                         ▼
                                  ┌──────────────┐             RELEASE LOCK
                                  │ JOURNAL PATCH│                 │
                                  │ status=      │                 ▼
                                  │ Partial|Fail │              exit 0
                                  └──────────────┘
                                          │
                                          ▼
                                    RELEASE LOCK
                                          │
                                          ▼
                                    exit non-zero
```

### Ordering invariant

`JournalAppend(InProgress)` happens **before** `SnapshotCreate`. Rationale:
if the snapshot creation crashes, the journal still records the intent.
The snapshot ULID is patched into the journal entry after a successful
snapshot. Failures between the two are recoverable.

### Recovery on startup

`archdots apply` (and any subcommand that touches the journal in a
mutating way) calls `Journal::orphaned_in_progress()` first. If it
returns any entry, `apply` aborts with a message instructing the user
to run `archdots recover`. `recover` reads the orphan, finds the
referenced snapshot (if any), restores it, and patches the entry with
`status: Failed`.

---

## 2. Journal — `journal.jsonl`

Append-only NDJSON at `$XDG_STATE_HOME/archdots/journal.jsonl`. Each
entry is exactly one line. `O_APPEND` writes ≤ `PIPE_BUF` (4096 on
Linux) are atomic per POSIX § 2.9.7.

### Hard limit

A single entry **must** stay below `PIPE_BUF`. To guarantee this, the
linker enforces a hard cap of **200 `LinkRecord` per entry**. Exceeding
the cap returns `CoreError::TooManyLinks`. Profiles that need more
links must be split into multiple profiles (or the cap can be revisited
once we have data showing real users hitting it).

### Entry schema (v1)

```jsonc
{
  "schema_version": 1,
  "id":             "01HV...ULID",
  "ts":             "2026-05-18T14:32:11Z",
  "profile":        "hyprland-rice",
  "action":         "apply",                 // "apply" | "rollback"
  "snapshot_id":    "01HV...ULID",           // may be null on InProgress / pure rollback
  "links": [
    {
      "source":       "/home/u/dots/hypr/hyprland.conf",
      "target":       "/home/u/.config/hypr/hyprland.conf",
      "prior_state":  "file",                // "absent"|"file"|"dir"|"symlink"|"symlink_owned"
      "prior_target": null,                  // present iff prior_state is a symlink variant
      "outcome":      "linked"               // "linked"|"skipped"|"failed"
    }
  ],
  "status":         "success",               // "in_progress"|"success"|"partial"|"failed"
  "error":          null,
  "supersedes":     null                     // id of the in_progress entry being closed
}
```

### Example (two entries)

```jsonl
{"schema_version":1,"id":"01HVAB7K9ZQ1ABCD","ts":"2026-05-18T14:32:11Z","profile":"hyprland-rice","action":"apply","snapshot_id":"01HVAB7K8XYZ0001","links":[{"source":"/home/u/dots/hypr/hyprland.conf","target":"/home/u/.config/hypr/hyprland.conf","prior_state":"file","prior_target":null,"outcome":"linked"},{"source":"/home/u/dots/waybar/config","target":"/home/u/.config/waybar/config","prior_state":"absent","prior_target":null,"outcome":"linked"}],"status":"success","error":null,"supersedes":null}
{"schema_version":1,"id":"01HVAC2QM4P9EFGH","ts":"2026-05-18T15:01:44Z","profile":"hyprland-rice","action":"rollback","snapshot_id":"01HVAB7K8XYZ0001","links":[{"source":"/home/u/dots/hypr/hyprland.conf","target":"/home/u/.config/hypr/hyprland.conf","prior_state":"file","prior_target":null,"outcome":"linked"}],"status":"success","error":null,"supersedes":null}
```

### Patching pattern

To "patch" an `InProgress` entry, we append a **new entry** with the
same `id` and an updated `status`, and `supersedes` pointing back at the
prior entry. Readers fold the journal forward: the latest entry per
`id` wins. This preserves append-only semantics and audit trail.

### Corruption handling

`iter()` yields `Err` for unparseable lines but continues. Readers
must tolerate corrupt lines (log via `tracing` and skip). Never abort
on corrupt journal — it is advisory state.

---

## 3. Snapshot — manifest and storage

### Layout

```
$XDG_DATA_HOME/archdots/snapshots/
├── 01HVAB7K8XYZ0001.tar.gz             # archive
├── 01HVAB7K8XYZ0001.manifest.json      # sidecar (derived, for fast list)
├── 01HVAC9PQ2RT0002.tar.gz
└── 01HVAC9PQ2RT0002.manifest.json
```

Inside `01HVAB7K8XYZ0001.tar.gz`:

```
manifest.json                            # authoritative
payload/.config/hypr/hyprland.conf       # paths relative to $HOME
payload/.config/waybar/config
```

### Sidecar contract

The sidecar is **derived**, not authoritative. If sidecar and embedded
manifest disagree, the embedded one wins and the sidecar is regenerated
on next `list`/`get`. The sidecar exists solely so that `snapshot list`
does not have to untar anything.

### Manifest schema (v1)

```jsonc
{
  "schema_version": 1,
  "id":             "01HVAB7K8XYZ0001",
  "created_at":     "2026-05-18T14:32:10Z",
  "profile":        "hyprland-rice",
  "trigger":        "pre_apply",            // "pre_apply"|"manual"|"pre_rollback"
  "host": {
    "hostname":         "archbox",
    "user":             "u",
    "home":             "/home/u",
    "archdots_version": "0.2.0"
  },
  "targets": [
    {
      "path":       ".config/hypr/hyprland.conf",  // relative to $HOME
      "kind":       "file",                        // "file"|"dir"|"symlink"|"absent"
      "symlink_to": null,
      "size":       1842,
      "mode":       33188,
      "mtime":      "2026-05-10T08:22:13Z",
      "sha256":     "9af3c2..."
    }
  ],
  "payload_sha256": "f8c0e1...",
  "compression":    "gzip",
  "format":         "tar"
}
```

`kind: "absent"` is mandatory and not optional: targets that did not
exist before apply must be recorded so rollback can `unlink` them
rather than create empty files.

uid/gid and xattrs/ACLs are **not** captured in v1. archdots is
per-user; revisit if a use case appears.

### Retention

**Manual.** `apply` does not prune. If the snapshot directory contains
more than **20** snapshots, `apply` (and `snapshot list`) emit a
warning recommending `archdots snapshot prune`. The threshold is
informational, not enforced.

---

## 4. Atomicity strategy

### 4.1 Symlinks (the dominant case)

```
1. let tmp = format!("{target}.archdots.{rand}");
2. symlinkat(source, AT_FDCWD, tmp)?;
3. renameat(AT_FDCWD, tmp, AT_FDCWD, target)?;
4. fsync(parent_dir_fd)?;
```

Because `tmp` is created in the same directory as `target`, the rename
is always within one filesystem and therefore atomic per `rename(2)`.
`RENAME_EXCHANGE` is not used: it requires both paths to exist, and
our planner explicitly supports `prior_state == absent`.

`symlinkat` is preferred over `symlink` because it accepts an explicit
directory FD, eliminating TOCTOU on the parent directory if we ever
open it once and operate via FD.

### 4.2 Why `--dry-run` does not take any lock

`--dry-run` only builds a `LinkPlan` (per-path `lstat` calls). It does
not read the link set as a *coherent transactional state* — there is
no "torn read" problem here. Taking any lock (even `LOCK_SH`) would
serialize dry-runs against real applies and pessimize the inspection
workflow. Skipped.

### 4.3 Regular file writes (manifest, journal patches, lockfile content)

```
1. tmp = NamedTempFile::new_in(parent_dir)?;
2. tmp.write_all(bytes)?;
3. tmp.as_file().sync_all()?;
4. tmp.persist(final_path)?;          // renameat
5. File::open(parent_dir)?.sync_all()?;
```

Step 5 (directory fsync) is the durability barrier most code forgets.

### 4.4 Relevant man pages

- `rename(2)` — atomicity within one filesystem; `EXDEV` across.
- `renameat2(2)` — `RENAME_NOREPLACE` / `RENAME_EXCHANGE` flags (not
  used by us, documented for context).
- `symlinkat(2)` — fails with `EEXIST` if target exists, hence the
  tmp-then-rename pattern.
- `fsync(2)` — and the note that durability of a `rename` requires
  fsync of the *containing directory*.
- `flock(2)` — advisory, per-FD, released on `close()`/exit. Used by
  the lockfile.
- POSIX § 2.9.7 — `O_APPEND` write atomicity for writes ≤ `PIPE_BUF`.

---

## 5. Public APIs

```rust
// archdots-core::snapshot
pub struct SnapshotManager { /* ... */ }
pub struct SnapshotId(Ulid);

pub struct Snapshot {
    pub id: SnapshotId,
    pub manifest: Manifest,
    pub archive_path: PathBuf,
}

pub struct CreateRequest<'a> {
    pub profile: &'a str,
    pub trigger: SnapshotTrigger,
    pub targets: &'a [PathBuf],
}
pub enum SnapshotTrigger { PreApply, Manual, PreRollback }

impl SnapshotManager {
    pub fn open(data_home: &Path) -> Result<Self, CoreError>;
    pub fn create(&self, req: CreateRequest<'_>) -> Result<Snapshot, CoreError>;
    pub fn restore(&self, id: &SnapshotId, opts: RestoreOptions) -> Result<RestoreReport, CoreError>;
    pub fn list(&self) -> Result<Vec<SnapshotSummary>, CoreError>;
    pub fn get(&self, id: &SnapshotId) -> Result<Snapshot, CoreError>;
    pub fn prune(&self, policy: PrunePolicy) -> Result<PruneReport, CoreError>;
}

pub struct RestoreOptions { pub dry_run: bool, pub continue_on_error: bool }
pub struct RestoreReport { pub restored: Vec<PathBuf>, pub skipped: Vec<(PathBuf, String)> }
pub enum PrunePolicy { KeepLast(usize), OlderThan(Duration), KeepLastPerProfile(usize) }
pub struct PruneReport { pub removed: Vec<SnapshotId>, pub freed_bytes: u64 }

// archdots-core::journal
pub struct Journal { /* ... */ }
pub struct JournalId(Ulid);

#[derive(Serialize, Deserialize)]
pub struct JournalEntry {
    pub schema_version: u32,
    pub id: JournalId,
    pub ts: OffsetDateTime,
    pub profile: String,
    pub action: JournalAction,
    pub snapshot_id: Option<SnapshotId>,
    pub links: Vec<LinkRecord>,
    pub status: JournalStatus,
    pub error: Option<String>,
    pub supersedes: Option<JournalId>,
}
pub enum JournalAction { Apply, Rollback }
pub enum JournalStatus { InProgress, Success, Partial, Failed }

impl Journal {
    pub fn open(state_home: &Path) -> Result<Self, CoreError>;
    pub fn append(&self, entry: &JournalEntry) -> Result<(), CoreError>;
    pub fn last_for_profile(&self, profile: &str) -> Result<Option<JournalEntry>, CoreError>;
    pub fn find(&self, id: &JournalId) -> Result<Option<JournalEntry>, CoreError>;
    pub fn iter(&self) -> Result<JournalIter, CoreError>;
    pub fn orphaned_in_progress(&self) -> Result<Vec<JournalEntry>, CoreError>;
}

// archdots-core::linker
pub struct Linker<'a> { /* ... */ }

pub struct LinkSpec { pub source: PathBuf, pub target: PathBuf }

pub struct LinkPlan {
    pub profile: String,
    pub items: Vec<PlannedLink>,
}
pub struct PlannedLink {
    pub spec: LinkSpec,
    pub disposition: LinkDisposition,
    pub prior_state: PriorState,
}
pub enum LinkDisposition {
    Create,
    ReplaceFile,
    ReplaceSymlink { current_target: PathBuf },
    AlreadyOwned,
    Conflict(ConflictReason),
    SkipDir,
}
pub enum ConflictReason { ParentMissing, PermissionDenied, OutsideHome, SourceMissing }
pub enum PriorState { Absent, File, Dir, Symlink { points_to: PathBuf } }

pub struct ApplyReport {
    pub journal_id: JournalId,
    pub snapshot_id: Option<SnapshotId>,
    pub applied: Vec<LinkRecord>,
    pub rolled_back: bool,
}

#[derive(Serialize, Deserialize)]
pub struct LinkRecord {
    pub source: PathBuf,
    pub target: PathBuf,
    pub prior_state: PriorState,
    pub outcome: LinkOutcome,
}
pub enum LinkOutcome { Linked, Skipped, Failed(String) }

impl<'a> Linker<'a> {
    pub fn new(snapshots: &'a SnapshotManager, journal: &'a Journal) -> Self;
    pub fn plan(&self, profile: &str, specs: &[LinkSpec]) -> Result<LinkPlan, CoreError>;
    pub fn apply(&self, plan: LinkPlan) -> Result<ApplyReport, CoreError>;
    pub fn rollback(&self, profile: &str) -> Result<ApplyReport, CoreError>;
}

// archdots-core::lock
pub struct ApplyLock { /* RAII */ }
impl ApplyLock {
    pub fn acquire(state_home: &Path) -> Result<Self, CoreError>;
    pub fn acquire_blocking(state_home: &Path, timeout: Duration) -> Result<Self, CoreError>;
}
// Drop releases the flock via close(fd).
```

Constraints enforced by `Linker::apply`:

- `plan.items.len() <= 200` — else `CoreError::TooManyLinks`.
- Calls `Journal::orphaned_in_progress()` and refuses if any.
- Acquires `ApplyLock` (non-blocking) before any mutation.

---

## 6. Edge cases and handling

| # | Case | Handling |
|---|---|---|
| 1 | Target is a regular file with user content | `prior_state: File`, captured in snapshot, `ReplaceFile`. Rollback restores byte-for-byte. |
| 2 | Target is a symlink to a foreign path | `ReplaceSymlink { current_target }`. Snapshot stores the symlink *as a symlink* (`kind: "symlink"`, `symlink_to: ...`); we do not follow it. |
| 3 | Target is a symlink already pointing at our source | `AlreadyOwned`. No-op. Idempotent apply. |
| 4 | Target is a non-empty directory | `SkipDir` + `CoreError::TargetIsDirectory`. MVP refuses. |
| 5 | Parent of target does not exist | `Conflict(ParentMissing)`. MVP refuses. Future flag `--create-parents`. |
| 6 | No write permission on parent dir | `Conflict(PermissionDenied)` via `faccessat(W_OK)`. Never reaches rename. |
| 7 | Target outside `$HOME` | `Conflict(OutsideHome)`. Hard MVP policy. |
| 8 | Concurrent apply | `flock(LOCK_EX|LOCK_NB)` → `CoreError::Busy { pid }`. `--dry-run` takes no lock (§4.2). |
| 9 | `EXDEV` on rename | Should not occur (tmp is sibling of target). If it does, fail and roll back. |
| 10 | Process killed mid-apply (SIGKILL, crash) | Next subcommand sees orphan via `Journal::orphaned_in_progress()`. `apply` refuses; user runs `archdots recover`. |
| 11 | Snapshot creation fails after journal `InProgress` | Patch entry → `Failed`; orphan flow not triggered (entry is closed). |
| 12 | Source disappears between plan and apply | Re-`lstat` before each `symlinkat`; if missing → `LinkOutcome::Failed`, trigger rollback. |
| 13 | Journal line corrupted | `iter()` yields `Err` for that line and continues. Log via `tracing`. Never abort. |
| 14 | Non-UTF-8 path | `CoreError::NonUtf8Path`. Documented MVP limitation. |
| 15 | Snapshot tarball corrupt on restore | Verify `payload_sha256`; mismatch → `CoreError::SnapshotCorrupt(id)`. Sidecar disagreement → regenerate sidecar from embedded manifest. |
| 16 | More than 200 links in one apply | `CoreError::TooManyLinks`. Split profile. |

---

## 7. Concurrency, async, and locking decisions

### 7.1 No tokio in `core`

`archdots-core` is 100% synchronous. Apply is a strictly ordered local
filesystem transaction; latency is dominated by `fsync`, not by CPU or
network I/O. Async would add function-color viral complexity, worse
stack traces, and a large dependency surface for zero throughput
benefit. If the TUI (Phase 4) needs non-blocking calls, the binary
crate wraps them in `spawn_blocking`.

### 7.2 `rustix` for locking

`rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive)`.

- `std`: no file locking API. Rejected.
- `fs2`: unmaintained since 2020. Rejected.
- `fs4`: maintained fork, viable but adds a single-purpose dep.
- `rustix`: actively maintained, frequently already a transitive dep,
  exposes `flock` directly. Chosen.

`flock(2)` (advisory, per-FD) is the correct primitive here — it
releases on `close()` or process death, which gives us automatic
cleanup if `apply` is killed.

### 7.3 Pinned dependency versions for Phase 2

```toml
ulid   = "1"
time   = "0.3"
rustix = { version = "0.38", features = ["fs"] }
```

---

## 8. Out-of-scope for Phase 2

- Capturing uid/gid, xattrs, ACLs in snapshots.
- `--create-parents` flag for missing parent dirs.
- `--force` flag for replacing non-empty directories.
- `--allow-system` flag for targets outside `$HOME`.
- Automatic prune policies on `apply`.
- Cross-filesystem rename fallback (`copy + fsync + unlink`).

These remain explicit non-goals until a real user need surfaces.
