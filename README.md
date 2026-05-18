# archdots

Dotfile manager for Arch Linux ricers.

![CI](https://github.com/njuante/archdots/actions/workflows/ci.yml/badge.svg)

> archdots is in early development (v0.1.x). Only `init` and `profile` commands work today.

## Roadmap

| Phase | Description | Status |
|-------|-------------|--------|
| 1 | Detection, profiles, CLI scaffolding | done |
| 2 | Atomic apply with rollback | in progress |
| 3 | Dependency validation | planned |
| 4 | TUI | planned |
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

## Install

Not yet on crates.io. Build from source:

```sh
cargo install --git https://github.com/njuante/archdots archdots
```

Requires Rust 1.75 or later.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.

## Contributing

Issues and PRs welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for details.
