# Changelog

All notable changes to archdots will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] - 2026-05-23

### Added

- `archdots export <profile>`: turn a profile into a publishable dotfiles directory ready to push to GitHub.
- **Three independent safety layers** — fail-secure by default:
  - **Sensitive-path filter**: prefix / exact / suffix denylist (`~/.ssh/`, `~/.gnupg/`, `~/.aws/`, `.pem`, `.key`, shell history, etc.) matched against both the declared target path and `canonicalize(source)`. A symlink whose canonical source resolves into `~/.ssh/` is caught at the path layer.
  - **Size / binary filter**: files larger than 1 MiB or whose first 8 KiB fail a printable-text sniff are excluded. Catches `.kdbx`, compiled blobs, fonts, and similar.
  - **Content scan (`SecretScanner`)**: embedded regex ruleset (AWS key, GitHub/GitLab/Slack tokens, private-key headers, Stripe secrets, Google API keys, npm auth tokens, JWTs, generic `key=value` assignments). `High`-severity hits **abort the export** unless explicitly overridden; `Medium` hits warn but do not block.
- `--include-secrets`: global override that bypasses the content-scan abort. Requires a TTY and a non-skippable typed confirmation (`I UNDERSTAND`); rejected on non-TTY stdin (exit 3). Never implied by `--yes`.
- `--allow-path <GLOB>`, `--allow-binary <GLOB>`, `--allow-secret <ID[:GLOB]>`: per-path and per-rule allowlists for surgical overrides without the nuclear option.
- `--format full|profile-only`:
  - `full` (default): `README.md` + `dotfiles/` + `archdots-profile.toml` + `install.sh` + `.gitignore`. The recipient does not need archdots.
  - `profile-only`: `README.md` + `archdots-profile.toml` only. Skips the scan phase. Incompatible with `--include-secrets` (exit 3).
- `--check`: runs plan + scan and prints the report without writing anything. Exit 0 if clean, 2 if findings.
- `--output <DIR>`, `--force`, `--yes`, `--no-readme`, `--no-install-script`, `--max-bytes <N>`, `--json` flags.
- Atomic write: output is staged under a sibling `.archdots-export.tmp.<rand>` directory, fsynced, then `rename`d over the destination — same atomicity model as the apply pipeline (Phase 2).
- Generated `README.md` from an embedded template (no third-party template engine): metadata table, dependency install commands, file manifest, excluded-files section, standalone `./install.sh` instructions, and archdots import recipe.
- Generated POSIX `install.sh`: symlinks `dotfiles/` into `$HOME` with backup-on-conflict; lists pacman deps; notes AUR deps for manual install.
- `archdots export --json`: stable JSON output with `schema_version: 1`. `classification` values are always objects (`{"include": {}}`, `{"exclude_sensitive_path": {...}}`), never bare strings — part of the v0.5.0 wire contract.
- `findings_overridden` tracked in the JSON summary and printed report when `--allow-secret` or `--include-secrets` is used.
- New `archdots-core::exporter` module: `Exporter`, `ExportPlan`, `PlannedExportItem`, `ItemClassification`, `SecretScanner`, `SecretFinding`, `ExportReport`, `ExportError`.
- New embedded data files in `archdots-core/data/`: `sensitive_paths.toml`, `secret_patterns.toml`, `readme_template.md`, `install_template.sh`, `gitignore_template`.

### Out of scope for v0.5 (not promised)

- Git init / GitHub push / release creation.
- Tar / zip bundle output.
- Snapshot-id-based export.
- User-overridable README or `install.sh` templates.
- Multi-profile export.
- TUI surface for export.
- Recursive directory inclusion.
- Entropy-based or binary secret detection.

## [0.4.0] - 2026-05-22

### Added

- `archdots tui`: interactive terminal UI with four tab views (Profiles, Snapshots, Deps, Diff).
- **Profiles view**: fuzzy search, lazy summary loading, apply/rollback/deps/diff actions.
- **Snapshots view**: fuzzy search, lazy detail panel, restore and prune actions.
- **Deps view**: per-profile dependency report with sectioned output (missing, implicit, optional).
- **Diff view**: per-profile symlink plan preview with disposition glyphs and detail panel.
- Help overlay (`?`) showing global and view-specific keybindings.
- Last-apply indicator in top bar reading from the journal (e.g. "last applied: laptop (2d ago)").
- `Linker::rollback_to_snapshot`: restore to a specific snapshot by id, bypassing the journal.
- `Profile::list_names`: list all profile names from a directory without loading their contents.
- `PrunePolicy::OnlyId`: prune a specific snapshot by id.
- Single-column layout when terminal width < 60 columns; minimum-size guard at 40×10.
- File-based TUI logging to `$XDG_STATE_HOME/archdots/tui.log` (degraded gracefully on error).
- New `archdots` binary dependencies: `fuzzy-matcher = "0.3"` (profile/snapshot fuzzy search),
  `arboard = "3"` (clipboard; falls back to a hint on init failure),
  `tracing-appender = "0.2"` (non-blocking file writer for `tui.log`).

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
