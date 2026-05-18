# archdots

Dotfile manager for Arch Linux and derivatives, focused on tiling WM users and the ricing culture.

## Features

- Atomic apply with rollback (journal-based transaction)
- Automatic pre-apply snapshot
- Sandbox: test a rice without touching `$HOME`
- Dependency validation against `pacman -Q` and AUR
- Auto-generated README with screenshots and install commands

## Install

```sh
cargo install archdots
```

## Usage

```sh
archdots --help
```

## Status

Early development — see [CHANGELOG.md](CHANGELOG.md) for release notes.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
