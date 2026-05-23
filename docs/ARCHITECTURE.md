# Architecture

## Overview

`archdots` is a Cargo workspace with two crates:

- **`archdots-core`** — pure logic library, no terminal I/O. All business logic lives here and is fully unit-testable using `tempfile::TempDir`.
- **`archdots`** — binary; wires up `clap` CLI, `ratatui` TUI, and `tracing-subscriber` logging.

## Module map (core)

| Module | Responsibility |
|---|---|
| `error` | Typed `CoreError` enum via `thiserror` |
| `detector` | Scan `$HOME` for known dotfile paths |
| `profile` | TOML schema for a named rice profile |
| `linker` | Atomic symlink create / remove / audit |
| `snapshot` | Gzip-compressed tarball snapshots in `$XDG_DATA_HOME/archdots/snapshots/` |

## ADR-001 — Two-crate workspace (Fase 0)

**Decision:** separate library (`archdots-core`) from binary (`archdots`).

**Reason:** keeps all testable logic in a crate that has no UI dependencies. This prevents coupling between `ratatui`/`crossterm` and core logic, and lets the test suite run without a terminal.

**Consequences:** the binary depends on `archdots-core`; the core crate never depends on the binary. All cross-crate error bridging goes through `anyhow` at the binary boundary.

## ADR-002 — Safe apply: storage, locking, and recovery (Fase 2)

**Context:** Phase 2 introduces the transactional `apply` pipeline
(snapshot → apply → journal, with rollback). The full design lives in
[`PHASE_2_DESIGN.md`](./PHASE_2_DESIGN.md). This ADR records the eight
load-bearing decisions so the rationale is greppable from
`ARCHITECTURE.md` without opening the full design doc.

1. **Journal format: JSONL, not TOML.** Append-only NDJSON at
   `$XDG_STATE_HOME/archdots/journal.jsonl`. `O_APPEND` writes ≤
   `PIPE_BUF` are atomic per POSIX § 2.9.7. TOML would require
   rewriting the whole document on every entry (O(n²) over the
   journal's lifetime) and has no append story. A hard cap of 200
   `LinkRecord` per entry keeps each line under `PIPE_BUF`.

2. **Identifiers: ULID, not UUIDv4.** Both `snapshot_id` and
   `journal_id` are ULIDs. Lexicographic order matches creation order
   for free; `ls -1` over the snapshot directory is already sorted.

3. **Lockfile: `rustix::fs::flock`.** Advisory, per-FD, released on
   `close()` or process death. `std` has no file-locking API; `fs2` is
   unmaintained; `fs4` works but `rustix` is often already a
   transitive dep and is actively maintained.

4. **`core` is 100% synchronous.** No `tokio`. Apply is an ordered
   local-FS transaction dominated by `fsync` latency; async would add
   function-color complexity for no throughput gain. The binary crate
   may use `spawn_blocking` for the TUI in Phase 4.

5. **Atomicity via `symlinkat` + `renameat` with sibling tmp.** Create
   the symlink at `<target>.archdots.<rand>` in the same directory as
   the target, then `renameat` over the target. This guarantees same-FS
   rename (no `EXDEV`) and atomic replacement per `rename(2)`. Regular
   file writes follow `tempfile → fsync → rename → fsync(parent_dir)`.

6. **Snapshot retention is manual.** `apply` never prunes. If the
   snapshot directory holds more than 20 snapshots, `apply` and
   `snapshot list` emit a warning. The user chooses when to run
   `archdots snapshot prune`. Automatic prune was rejected to avoid
   surprising data loss.

7. **Recovery flow for orphaned `InProgress` entries.** `apply` checks
   `Journal::orphaned_in_progress()` at startup and refuses to run if
   any are present, instructing the user to run `archdots recover`.
   The journal entry is written *before* the snapshot (with
   `snapshot_id: null`, patched on success), so a crash always leaves
   a trail.

8. **Schema versioning from v1.** Both `JournalEntry` and the snapshot
   `manifest.json` carry `schema_version: 1`. Snapshots are
   long-lived; treating versioning as optional later would force a
   migration we can avoid by writing the field from day one.

**Consequences:**

- New dependencies in `archdots-core`: `ulid = "1"`, `time = "0.3"`,
  `rustix = { version = "0.38", features = ["fs"] }`.
- A new `archdots recover` subcommand is required before Phase 2 ships.
- `--dry-run` is read-only and takes **no** lock (not even shared) — it
  builds a `LinkPlan` from per-path `lstat`s and cannot tear.

## ADR-003 — Dependency validation: Arch-only, struct, lenient parsing (Fase 3)

**Context:** Phase 3 introduces `archdots check <profile>`: it reports
which packages a profile needs, which are installed, and which are
missing. The full design lives in [`PHASE_3_DESIGN.md`](./PHASE_3_DESIGN.md).
This ADR records the seven load-bearing decisions so the rationale is
greppable from `ARCHITECTURE.md` without opening the full design doc.

1. **Arch only; no `trait PackageDB`.** v0.3 hard-codes `pacman` in a
   concrete `PackageDB` struct. Multi-distro support was rejected as
   premature abstraction: archdots is brand-coded as Arch-only, and a
   neutral trait would either leak distro concerns or paper over them.
   Refactoring to a trait if a real second-distro user appears later is
   a bounded refactor across a handful of callers.

2. **`CommandRunner` trait for subprocess testability.** `PackageDB`
   stores `Box<dyn CommandRunner>`. Production uses `SystemRunner`
   (real `std::process::Command`); tests inject `MockRunner` (lives in
   `crates/archdots-core/tests/`, not in `src/`). This is orthogonal to
   distro: the trait abstracts the *subprocess*, not the *package
   manager*. No `ARCHDOTS_RUNNER` env var or production-side feature
   flag for tests — every test uses `with_runner` directly.

3. **Per-format parsers, no generic engine.** Five parsers
   (`Bspwm`, `Sxhkd`, `Hyprland`, `I3Sway`, `Shell`) share two helpers
   (comment stripping, line-continuation joining) but each owns its
   syntax-specific extraction. A table-driven engine would either be
   too rigid (Hyprland's `bind = ..., exec, X` needs structured field
   selection) or too configurable (mini-DSL nobody maintains).
   `bspc rule` lines are explicitly **not** parsed: they reference X11
   class names, not binaries.

4. **Curated binary → package table + `--deep` fallback.** ~40 entries
   in `data/binary_providers.toml` (embedded via `include_str!`) cover
   the common ricing case. Unknown binaries return
   `ProviderHit::Unknown` unless `--deep` is passed, in which case
   `pacman -F <binary>` is consulted. `pacman -F` requires the user to
   have run `pacman -Fy` once as root; archdots **never** runs that
   command itself. A `data/builtin_filter.toml` of ~60 names (shell
   builtins + always-installed base utilities) prevents shell-config
   parsing from drowning the report in noise.

5. **Parsers preserve every mention; the validator groups.** Parsers
   emit one `Mention` per occurrence, never deduplicating. The
   `Validator` is the layer that groups by binary, deduplicates, and
   constructs one `DepEntry` per package — keeping all source
   locations attached for the `--verbose` report. This shifts the
   policy of "what is a distinct dependency?" out of parsers (which
   are dumb) and into a single, testable place.

6. **JSON output is a stable API from v0.3.0.** `archdots check
   --json` carries `schema_version: 1`. Versioning is *per-output*,
   independent of crate semver: the crate can move 0.3 → 1.0 with
   `schema_version` staying at `1` as long as the JSON shape is
   backwards-compatible. Additive changes stay on v1; breaking
   changes bump to v2. Downstream CI tooling may rely on this.

7. **Exit codes `0/1/2/3` with strict precedence.** `0` = all required
   installed; `1` = required missing (or implicit missing under
   `--strict`); `2` = optional or implicit missing without `--strict`;
   `3` = indeterminate (pacman absent, db locked, profile broken).
   Precedence `3 > 1 > 2 > 0`. Profile-resolution errors (e.g.
   `ProfileError::UnknownEnvVar` in a target path) propagate as
   `ValidatorError::Profile` and exit `3`.

**Consequences:**

- New data files in `archdots-core`:
  `data/binary_providers.toml`, `data/builtin_filter.toml`.
- A new `archdots check` subcommand with flags `--json`, `--strict`,
  `--deep`, `--verbose`.
- No new heavyweight dependencies — `PackageDB` uses
  `std::process::Command` plus the existing serde stack.
- CI continues to run on Ubuntu and never invokes `pacman`. One
  `#[ignore]`d integration test exercises real `pacman` on Arch hosts,
  same pattern as Phase 2's cross-process lock test.
- Granularity of `ParserKind` and `MentionSource` variants is part of
  the stable public API from v0.3.0. Downstream consumers may match on
  these values; adding variants is a breaking change from that point.

## ADR-004 — TUI architecture (Fase 4)

**Context:** Phase 4 introduces the interactive TUI (`archdots tui`). The
full design lives in [`PHASE_4_DESIGN.md`](./PHASE_4_DESIGN.md). This ADR
records the five load-bearing decisions so the rationale is greppable from
`ARCHITECTURE.md` without opening the full design doc.

1. **Threads + `mpsc`, no tokio in the binary.** ADR-002 stands: `core`
   stays sync. The TUI spawns a fresh `std::thread` per background
   operation (apply, rollback, check, prune) and routes completions through
   a single `mpsc::Sender<TaskMessage>` owned by `App`. We expect at most
   one concurrent task; thread cost is trivially affordable. Tokio's value
   (cheap async tasks, I/O composition) is irrelevant for an interactive
   single-user FS-bound transactional workflow. Workers receive
   everything by value (`PathBuf`, `String`, options) — no references into
   `App` — so user navigation during a task is invisible to the worker.

2. **Per-view structs + `Action` enum.** Each tab (`ProfilesView`,
   `SnapshotsView`, `DepsView`, `DiffView`) owns its state. `handle_event`
   returns an `Action` consumed by `App::dispatch`. No global reducer, no
   whole-state clones. Cross-view coordination (e.g. "Profiles → Deps with
   this name") is an explicit `Action` variant, which keeps per-view tests
   independent of `App` internals.

3. **`ConfirmKind` enum, not `FnOnce` closures, inside `Modal::Confirm`.**
   `Modal::Confirm { prompt, kind: ConfirmKind }` carries one of
   `ApplyProfile(String)` / `RollbackProfile(String)` /
   `RollbackToSnapshot(SnapshotId)` / `DeleteProfile(String)` /
   `PruneSnapshot(SnapshotId)` / `QuitWithRunningTask`. `App::execute_confirm`
   is a single `match` returning the appropriate `Action`. Trade-off
   accepted: ~25 LOC of boilerplate buys `Modal: Clone`, lets tests assert
   `matches!(modal, Modal::Confirm { kind: ConfirmKind::ApplyProfile(_) })`
   without invoking anything, and keeps the dispatch table greppable.

4. **`archdots tui` is an explicit subcommand.** `archdots` with no args
   keeps `arg_required_else_help = true` and prints the help. No clap
   default-subcommand magic — that would make `archdots` enter raw mode on
   piped/CI invocations. Discoverable via `archdots --help`; future
   completion (clap_complete) gets it for free. `tracing-subscriber` is
   reconfigured to write `$XDG_STATE_HOME/archdots/tui.log` (truncated per
   launch, `INFO` default, override via `ARCHDOTS_LOG`) — never stderr,
   which is owned by ratatui.

5. **Two new public APIs in `archdots-core`, both additive.**
   - `Linker::rollback_to_snapshot(&SnapshotId, ApplyOptions) -> Result<ApplyReport, CoreError>`
     restores an arbitrary snapshot (not just the latest success per
     profile). The profile is read from `manifest.profile`. The journal
     chain is `InProgress(snapshot_id=None) → InProgress(snapshot_id=Some) → Success|Partial|Failed`
     with `action = Rollback`, so orphan recovery works identically to
     `apply`.
   - `Profile::list_names(&Path) -> Result<Vec<String>, ProfileError>`
     enumerates profile names in a directory (sorted, `.toml` stripped,
     non-TOML and invalid names silently skipped). Replaces the open-coded
     loop in `cmd::profile::run_list`; the TUI uses the same source.

**Consequences:**

- New dependencies in `archdots`: `fuzzy-matcher = "0.3"` (search ranking
  in Profiles/Snapshots), `arboard = "3"` (clipboard for errors and
  install commands; falls back to a hint message on init failure),
  `tracing-appender = "0.2"` (non-blocking file writer for `tui.log`).
- No new dependencies in `archdots-core`.
- The TUI process **does not** acquire `ApplyLock`. Only per-task workers
  do, when performing apply / rollback / `rollback_to_snapshot` / prune.
  Two `archdots tui` processes can browse the same state concurrently;
  the second mutating worker receives `LockError::Busy` and a modal.
- Cancellation is out of scope for v0.4. Force-quitting during a running
  task (Ctrl+C twice within 3 s) leaves a journal orphan that
  `archdots recover` reconciles on next launch — same flow as a crashed
  CLI invocation.
- New module tree under `crates/archdots/src/tui/` (`app`, `action`,
  `events`, `tasks`, `theme`, `ui/`, `views/`). The TUI never reaches into
  `cmd/*::run` (those write to stdout); both presenters call the same
  core functions.
- Tick rate is **20 Hz hardcoded** (50 ms poll). Redraw only on dirty
  state or while a task is animating — battery-friendly idle.
- `ApplyReport` and `ValidationReport` are unchanged; `rollback_to_snapshot`
  reuses `ApplyReport`.

**Design deviations (v0.4.0 implementation vs. PHASE_4_DESIGN.md):**

- **Ctrl+C during `Running` (§3.4):** Implemented as
  `Modal::Confirm { kind: ConfirmKind::QuitWithRunningTask }` rather than
  the "double Ctrl+C within 3 s → `exit(130)`" flow described in the
  design. The UX outcome is equivalent; the modal is more explicit and
  consistent with the rest of the confirm-flow. Decision: **modal**.

- **`Modal::Confirm` carries `details: Option<String>` (§5.6):** The
  design's `Modal::Confirm { prompt, kind }` was extended with an
  optional `details` field for supplementary context (e.g. "this will
  replace N symlinks"). Accepted — improves UX, no impact on `Modal: Clone`
  or the `ConfirmKind` match table.

- **`SnapshotsView` search filters by profile name only (§7.3):** The
  design specifies "search by id-prefix / profile". The implementation
  filters only by profile name, not by id-prefix. Sufficient for v0.4;
  id-prefix search is a v0.5 candidate.

- **Stale-completion check (`TaskId`, §3.5):** `BackgroundState::Running`
  carries an `id` field reserved for comparing against `Completed.id`
  before transitioning to `Idle`, but the comparison is not yet performed.
  The receiver does not survive a process restart, so there is no
  real-world impact. Noted as technical debt for a future cleanup.

- **"Still running…" hints after 10 s / 30 s (§11 case 8):** Not
  implemented in v0.4. Candidate for v0.4.1.

## ADR-005 — Export design: fail-secure publishing (Fase 5)

**Context:** Phase 5 introduces `archdots export <profile>`: it turns a
profile into a publishable dotfiles directory (README, dotfiles copy,
re-rooted profile TOML, standalone installer, `.gitignore`). The full
design lives in [`PHASE_5_DESIGN.md`](./PHASE_5_DESIGN.md). This ADR
records the seven load-bearing decisions so the rationale is greppable
from `ARCHITECTURE.md` without opening the full design doc.

1. **Three independent safety layers, each sufficient to abort alone.**
   `export` runs a sensitive-path filter, a size/binary filter, and an
   embedded `SecretScanner` — in that order, each capable of blocking the
   export without the others. Relying on a single layer would mean a bypass
   of any one mechanism (e.g., an innocent-looking target name whose source
   symlinks into `~/.ssh/`) silently ships a secret. Redundancy is the
   design, not an accident. `High`-severity scanner hits abort the whole
   export even with `--yes`; the only override is an explicit opt-in
   (`--allow-secret <rule_id>` per-rule or `--include-secrets` global) that
   requires a typed TTY confirmation.

2. **Dual-path check (target AND `canonicalize(source)`) in the
   sensitive-path filter.** The path filter checks the declared target path
   relative to `$HOME` **and** the canonicalized source path relative to
   `$HOME`. Either match excludes the item. A profile entry with an innocent
   target name (`~/foo`) whose source is a symlink into `~/.ssh/` is caught
   at the path layer, before the content scan ever reads the bytes. This is
   the same dual-check principle as Phase 2's TOCTOU mitigation: we stat
   what we're actually operating on, not the pointer.

3. **`export` is read-only on the archdots state directories.** It never
   modifies `$XDG_DATA_HOME/archdots/` or `$XDG_STATE_HOME/archdots/`,
   takes no `ApplyLock`, and does not invoke `pacman`. The `Validator` from
   Phase 3 is intentionally not used: the README's dependency sections are
   rendered straight from `Profile.dependencies`, keeping `export` runnable
   on non-Arch hosts and decoupling "publish" from "audit".

4. **Atomic write via staging-dir rename (same pattern as Phase 2).** The
   output is built under a sibling `.archdots-export.tmp.<rand>` directory,
   each file fsynced, then the staging dir itself fsynced, then
   `rename`d over the final destination. On any error the staging dir is
   removed and the destination is left untouched. The same atomicity
   guarantee as `apply` (ADR-002 decision 5) applies: the user never sees a
   half-written export directory.

5. **`--format full|profile-only` via a flag, not two subcommands.**
   `full` (default) produces the complete shareable repo structure;
   `profile-only` produces only `README.md` and `archdots-profile.toml`.
   Separate subcommands (`export-full` / `export-profile`) would share 95%
   of their code and complicate shell completions; a flag keeps the surface
   minimal. `--include-secrets` combined with `profile-only` is rejected at
   flag-parse time (exit 3) because `profile-only` skips the scan phase and
   has no file bytes to override.

6. **Embedded template, no third-party engine.** The README is rendered from
   a `include_str!` template with a ~60-line substitutor handling `{var}`,
   `{IF cond}…{ENDIF}`, and `{FOR item IN list}…{ENDFOR}`. Tera /
   handlebars / askama would introduce a new dependency class and a new
   error class for what is ultimately a ~200-line output with a fixed set of
   substitutions. User-overridable templates (`--readme <PATH>`) are left as
   a non-breaking future addition behind `Exporter::render_readme`.

7. **JSON output: `classification` values are always objects, never bare
   strings.** `ItemClassification` variants are declared in struct form
   (including empty `{}` variants) so serde external tagging emits a
   consistent object shape for every variant. This is a contract from
   v0.5.0: downstream tooling can match a single shape without branching on
   `typeof`. Simple scalar enums (`format`, `severity`, `kind`) stay as
   strings — they have no parameterised variants and their shape cannot need
   to change without a breaking schema bump. The JSON output carries
   `schema_version: 1` with the same versioning policy as Phase 3's
   `check --json`: additive changes stay on v1, breaking changes bump to v2.

**Consequences:**

- New module `archdots_core::exporter` with sub-modules `scanner`, `glob`,
  and `template`. No changes to `Profile`, `Linker`, `Validator`,
  `Snapshot`, `Journal`, or `Lock`.
- New embedded data files in `archdots-core/data/`: `sensitive_paths.toml`,
  `secret_patterns.toml`, `readme_template.md`, `install_template.sh`,
  `gitignore_template`.
- New `ExportError` variant in `CoreError` (`#[non_exhaustive]`; additive).
- No new workspace-level dependencies: `regex = "1"` was already present
  (added in Phase 3's `archdots-core` dep list).
- `archdots export` is a CLI-only surface in v0.5; no TUI integration (see
  PHASE_5_DESIGN.md §F). A future `[E]xport profile` action in
  `ProfilesView` is a non-breaking addition.
- Exit codes `0/1/2/3` with the same precedence rule as Phase 3 (`check`):
  `3 > 2 > 1 > 0`.
