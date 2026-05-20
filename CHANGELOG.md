# Changelog

All notable changes to archdots will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] - 2026-05-20

### Added

- `archdots check <profile>` subcommand for dependency validation against `pacman -Q` / `pacman -Qm`.
- Config parsers for bspwm, sxhkd, hyprland, i3, sway, zsh, bash, and shell dotfiles.
- `PackageDB` wrapping `pacman -Q` / `pacman -Qm` / `pacman -F` with a curated binary→package table.
- `ValidationReport` with stable JSON schema (`schema_version: 1`).
- `--strict`, `--deep`, `--verbose`, `--json` flags for `archdots check`.
- Exit codes documented: 0 = all required installed, 1 = required missing, 2 = optional/implicit missing, 3 = indeterminate.

### Fixed

- Profile errors (broken TOML, unresolvable `$VAR` in target paths) now exit 3 instead of 1.
- `$VAR` resolution failures in profile file targets surface as `ValidatorError::Profile` instead of being silently swallowed.
- AUR dependencies are deduplicated in the validator (same package listed twice → one entry, one warning).

### Changed

- Public API: `MentionSource::HyprlandBind` distinguishes `bind* = MOD, KEY, exec, X` from `exec = X`.

## [0.2.0] - 2026-05-19

### Added

- `archdots apply <profile>`: atomically symlinks dotfiles with automatic rollback on failure.
- `archdots diff <profile>`: previews planned link changes without touching the filesystem.
- `archdots rollback <profile>`: restores files from the snapshot taken before the last apply.
- `archdots history [profile]`: prints the apply journal in human-readable form.
- `archdots snapshots list|get|prune`: lists and manages stored file-state snapshots.
- `archdots recover`: resolves orphaned in-progress journal entries left by a crash or kill.
- `SnapshotManager`: captures pre-apply file state as `gzip` tarballs with a SHA-256 manifest.
- Append-only `journal.jsonl` (NDJSON) records every apply, rollback, and their outcomes.
- `ApplyLock`: exclusive `flock(2)` via `rustix` prevents concurrent `apply` invocations.
- Atomic symlink placement via `symlinkat` + `renameat` with parent-directory `fsync`.
- `#[non_exhaustive]` on all public enums for SemVer-safe library evolution.

### Changed

- Restructured `cmd/` module; `init.rs` and `profile_cmds.rs` now live under `cmd/`.
- `CoreError::NonUtf8Path` now carries the offending `PathBuf` for actionable error messages.

### Fixed

- Orphan check now runs inside the apply lock, closing a TOCTOU race on startup.
- Snapshot restore now faithfully restores Unix mode bits (`stat.st_mode`) on each file.
- Removed `unwrap()` in `cmd/diff.rs`; errors propagate via the standard `Result` chain.

### Security

- Snapshot restore preserves original file permissions (e.g. `0o600` for `~/.ssh/config`).

## [0.1.0] — Unreleased

### Added

- Workspace structure with `archdots-core` library crate and `archdots` binary crate.
- `archdots --version` works.
- Skeleton modules in `core`: `detector`, `profile`, `linker`, `snapshot`, `error`.
- CI pipeline: `cargo fmt`, `cargo clippy -D warnings`, `cargo test`.
