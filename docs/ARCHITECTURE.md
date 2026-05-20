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
