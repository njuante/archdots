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
