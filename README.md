# archdots

Dotfile manager for Arch Linux ricers.

![CI](https://github.com/njuante/archdots/actions/workflows/ci.yml/badge.svg)

> archdots is at v0.4.0. Core apply/rollback pipeline and dependency validation are functional. CLI offers init, profile, apply, diff, rollback, history, snapshots, recover, check, and an interactive TUI.

## Roadmap

| Phase | Description | Status |
|-------|-------------|--------|
| 1 | Detection, profiles, CLI scaffolding | ✅ done |
| 2 | Atomic apply with rollback, snapshots, journal, recovery | ✅ done |
| 3 | Dependency validation (`archdots check`) | ✅ done |
| 4 | TUI | ✅ done |
| 5 | README export | planned |

## What works today

**`archdots init`** — scan `$HOME` for known dotfiles and generate a starter profile:

```sh
archdots init --name my-rice
archdots init --name my-rice --force   # overwrite existing
archdots init --output /path/to/profile.toml
```

**`archdots profile list`** — list all saved profiles:

```sh
archdots profile list
```

**`archdots profile show <name>`** — print a profile's contents:

```sh
archdots profile show my-rice
```

**`archdots profile delete <name>`** — delete a profile with confirmation:

```sh
archdots profile delete my-rice
archdots profile delete my-rice --yes   # skip prompt
```

Profiles are stored as TOML files under `$XDG_CONFIG_HOME/archdots/profiles/`
(defaults to `~/.config/archdots/profiles/`).

**`archdots apply <profile>`** — deploy a profile's dotfiles to their target paths. Always run with `--dry-run` first to review what would change:

```sh
archdots apply my-rice --dry-run
archdots apply my-rice
```

**`archdots diff <profile>`** — show the diff between the profile's sources and what is currently on disk:

```sh
archdots diff my-rice
```

**`archdots rollback`** — restore the state captured by the pre-apply snapshot:

```sh
archdots rollback
archdots rollback --to <snapshot-id>
```

**`archdots history`** — list all apply operations recorded in the journal:

```sh
archdots history
```

**`archdots snapshots list`** — list all saved snapshots:

```sh
archdots snapshots list
```

**`archdots snapshots show <id>`** — inspect the contents of a snapshot:

```sh
archdots snapshots show 20240501-120000
```

**`archdots snapshots prune`** — remove old snapshots beyond the configured retention limit:

```sh
archdots snapshots prune
archdots snapshots prune --keep 5
```

**`archdots recover`** — attempt recovery after a failed or interrupted apply:

```sh
archdots recover
```

**`archdots tui`** — launch the interactive terminal UI:

```sh
archdots tui
```

The TUI provides four tab views. Log output is written to `$XDG_STATE_HOME/archdots/tui.log`.

### TUI keybindings

| Key | Action |
|-----|--------|
| `1` | Profiles view |
| `2` | Snapshots view |
| `3` | Deps view |
| `4` | Diff view |
| `Tab` | Next tab |
| `?` | Toggle help overlay |
| `q` / `Ctrl+C` | Quit |

**Profiles view**

| Key | Action |
|-----|--------|
| `j` / `k` | Move cursor down / up |
| `Enter` | Apply selected profile |
| `r` | Rollback selected profile |
| `c` | Show deps for profile |
| `d` | Show diff for profile |
| `x` | Delete profile |
| `/` | Fuzzy search |
| `Esc` | Clear search |

**Snapshots view**

| Key | Action |
|-----|--------|
| `j` / `k` | Move cursor down / up |
| `r` | Restore (rollback to) snapshot |
| `x` | Prune (delete) snapshot |
| `i` | Toggle detail panel |
| `/` | Fuzzy search |

**Deps view**

| Key | Action |
|-----|--------|
| `j` / `k` | Navigate within section |
| `J` / `K` | Jump to next / previous section |
| `g` / `G` | First / last entry |
| `c` | Copy install command for selected missing dep |
| `R` | Re-run check |
| `D` | Toggle `--deep` and re-run |

**Diff view**

| Key | Action |
|-----|--------|
| `j` / `k` | Move file cursor |
| `J` / `K` | Scroll diff body down / up |
| `g` / `G` | First / last file |
| `a` | Apply this profile |
| `q` | Back to Profiles view |

**`archdots check <profile>`** — validate a profile's dependency declarations against the installed package database:

```sh
archdots check my-rice               # text report
archdots check my-rice --json        # machine-readable JSON
archdots check my-rice --strict      # promote implicit-missing to required
archdots check my-rice --deep        # use pacman -F for unknown binaries
archdots check my-rice --verbose     # show all mentions
```

## Dependency validation

`archdots check` cross-references the packages declared in a profile with
`pacman -Q` output and infers additional dependencies by parsing config files
(bspwmrc, hyprland.conf, i3/sway config, .zshrc, .bashrc, and others).

### Exit codes

| Code | Meaning |
|------|---------|
| 0 | No missing required deps |
| 1 | One or more required deps missing |
| 2 | Only optional or implicit deps missing (or implicit missing under `--strict` → 1) |
| 3 | Indeterminate: pacman not found, database locked, or profile broken |

## Safety guarantees

- **Atomic apply**: a snapshot of all target files is taken before any write occurs; the journal records every operation so a partial apply can be detected and reversed.
- **Rollback restores content and Unix permissions**: file mode bits are captured in the snapshot and restored verbatim on rollback.
- **Process lockfile prevents concurrent applies**: a lockfile under the archdots data directory ensures only one apply runs at a time.
- **Conflict detection before touching the filesystem**: archdots checks for conflicts between the profile and current disk state before writing any file.
- **`--dry-run` touches nothing**: passing `--dry-run` to `apply` prints every planned action without modifying the filesystem.

## Install

Not yet on crates.io. Build from source:

```sh
git clone https://github.com/njuante/archdots
cd archdots
cargo build --release
# binary at target/release/archdots
```

Or install directly with Cargo:

```sh
cargo install --git https://github.com/njuante/archdots archdots
```

Requires Rust 1.75 or later.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.

## Contributing

Issues and PRs welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for details.
