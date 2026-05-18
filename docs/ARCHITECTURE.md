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
