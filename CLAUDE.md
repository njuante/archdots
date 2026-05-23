# CLAUDE.md

Project-specific guidance for Claude Code working on archdots.

## What this is

archdots is a CLI + TUI for managing Arch Linux dotfiles via atomic symlink
operations with snapshots and rollback. It's a single workspace with two crates.

## Workspace layout

- `crates/archdots-core/` — pure library. No I/O it doesn't own. All the
  domain logic (profile schema, linker, snapshots, journal, detector, dependency
  validator, exporter, config-file parsers) lives here.
- `crates/archdots/` — the binary. CLI entry points under `src/cmd/`, TUI
  under `src/tui/`. Talks to the world (filesystem, terminal, `pacman`) and
  delegates the rules to `archdots-core`.

The orphaned file `crates/archdots/src/init.rs` is **not** wired up in
`main.rs`. The live implementation is `crates/archdots/src/cmd/init.rs`.

## Path-resolution model (read this before touching profiles)

A profile's `[[files]]` entries each have a `source` and a `target`:

- `target` — unexpanded string, may contain `~` and `$VAR`. Resolved with
  `Profile::resolve_target` + a `ResolveCtx`.
- `source` — relative path, resolved against the profile's **source root**.

The source root comes from the optional `[paths] source_root` field
(introduced in v0.6.0):

- When set, sources resolve against that directory (typically the managed
  staging dir under `$XDG_DATA_HOME/archdots/profiles/<name>/dotfiles/`).
- When unset, sources resolve against `$HOME` for backwards compatibility
  with v0.5.x profiles.

`Profile::source_root(home, ctx)` returns the effective root. `Profile::
resolve_source(entry, root)` joins the relative source path with the root and
validates it does not escape via `..`. `Profile::resolved_entries` is the
high-level iterator that does both for every entry.

`init` creates the staging dir, copies detected dotfiles into it, and writes
the resulting `source_root` into the new profile. `apply` then symlinks back
into `$HOME`. Without that staging step, `source` and `target` collapse to the
same path and the linker rejects the apply as `CircularSymlink`.

## Storage locations

All resolved via `crates/archdots/src/xdg.rs` (no external XDG crate):

- Profiles — `$XDG_CONFIG_HOME/archdots/profiles/<name>.toml`
- Staging dotfiles — `$XDG_DATA_HOME/archdots/profiles/<name>/dotfiles/`
- Snapshots — `$XDG_DATA_HOME/archdots/snapshots/<ulid>.{tar.gz,manifest.json}`
- Journal & TUI log — `$XDG_STATE_HOME/archdots/{journal.jsonl,tui.log}`

The journal is append-only NDJSON. Each apply/rollback writes an
`in_progress` entry, then either a `success` or `failed` entry that
`supersedes` the first.

## Apply pipeline

1. Acquire exclusive `flock(2)` via `ApplyLock` (prevents concurrent applies).
2. Run orphan check inside the lock (closes the v0.2 TOCTOU race).
3. `Linker::plan` classifies each entry (`Create`, `AlreadyOwned`,
   `ReplaceFile`, `ReplaceSymlink`, `Conflict(...)`, `SkipDir`).
4. `SnapshotManager` captures pre-state into a gzip tar + SHA-256 manifest.
5. Apply each link via `symlinkat` + `renameat` with parent-dir `fsync` —
   atomic per-file.
6. On any failure, automatic rollback restores from the snapshot.

## TUI architecture

- `App` in `tui/app.rs` is the top-level state machine. `step(event)` runs
  one event-loop iteration and returns whether a redraw is needed via the
  `dirty` flag.
- Views (`profiles`, `snapshots`, `deps`, `diff`) implement the `View` trait
  and handle their own internal state. They return `Action`s up to `App`.
- Key navigation that only mutates view state returns `Action::None`. The
  outer `step()` sets `dirty = true` after dispatching key events so the
  frame repaints — without this, views appear frozen until the next unrelated
  event. Don't remove that line; it was the v0.5 redraw bug.
- Background work runs via `mpsc` channel + `BackgroundState`. The spinner
  redraws while `is_animating()` is true.

## Commands

- `cargo test --workspace` — unit + integration + e2e
- `cargo clippy --workspace --all-targets -- -D warnings` — CI uses this
- `cargo run -- <subcommand>` — run the binary against the dev build
- `cargo install --path crates/archdots --locked` — install the binary

## When extending the profile schema

If you add a field to `Profile`, every constructor needs it: 2 in
`exporter/mod.rs`, 1 in `cmd/init.rs`, plus the test helpers in
`crates/archdots-core/tests/{profile,validator}.rs` and
`crates/archdots/src/tui/views/diff.rs` (in the `write_profile` test fixture).
Run `cargo build --workspace` after the schema change to surface every
missing-field error at once.

## When adding a CLI subcommand

1. Add the `Commands::` variant in `crates/archdots/src/main.rs`.
2. Add the matching `cmd::<name>::run` in `crates/archdots/src/cmd/<name>.rs`
   and register it in `cmd/mod.rs`.
3. Add an e2e test in `crates/archdots/tests/`.

## Snapshot/journal invariants

- Never mutate a snapshot manifest after writing — they're treated as
  immutable artefacts.
- Journal entries are append-only. To "undo" an apply, write a new rollback
  entry whose `supersedes` field points at the apply's id. Do not delete or
  rewrite history.
- Pre-apply snapshots preserve Unix mode bits (`stat.st_mode`). The v0.2
  security fix was specifically about `0o600` files like `~/.ssh/config`;
  don't regress that.
