# archdots — Project Brief for Claude Code

## Qué es archdots

Gestor de dotfiles para Arch Linux y derivadas, enfocado en usuarios de tiling WMs (bspwm, Hyprland, i3, sway) y la cultura ricing. Resuelve lo que stow/chezmoi/yadm hacen mal: **probar rices ajenos sin romper el sistema**, validar dependencias contra pacman/AUR, snapshots reversibles, perfiles por contexto.

Stack: **Rust + Ratatui**, un solo binario con subcomandos CLI y TUI.

## Killer features (no perder el foco)

1. Apply atómico con rollback (transacción tipo journal).
2. Snapshot pre-apply automático.
3. Sandbox: probar un rice sin tocar `$HOME` real.
4. Validación de dependencias contra `pacman -Q` y AUR.
5. README autogenerado con screenshots y comandos de instalación.

## Arquitectura

Workspace con dos crates:

- `archdots-core`: library, lógica pura, sin I/O de UI. Testeable al 100%.
- `archdots`: binary, CLI con `clap` + TUI con `ratatui`.

La regla es: si una pieza de lógica no puede testearse sin abrir terminal, va en `core` mal escrita. Refactor.

## Reglas estrictas de código

1. **Cero `unwrap()` y cero `expect()` en código de producción.** Solo permitidos en tests y en `main.rs` para errores fatales muy específicos. Usa `?` y `thiserror`.
2. **Cero `panic!` fuera de invariantes claramente documentadas.**
3. **`#![deny(clippy::all)]` y `#![warn(clippy::pedantic)]`** en cada crate. Las excepciones se justifican con `#[allow(...)]` y un comentario.
4. **`rustfmt` antes de cada commit.** El CI falla si no está formateado.
5. **Tests por cada módulo público.** Cobertura objetivo > 70% en `core`.
6. **Errores tipados con `thiserror` en `core`, `anyhow` solo en el binario.**
7. **Logging con `tracing`, no `println!`.** El binario configura `tracing-subscriber` con nivel ajustable por flag.
8. **Operaciones de FS atómicas.** Cualquier escritura usa el patrón "write to tempfile, then rename". Los symlinks usan `symlinkat` cuando es posible.
9. **Sin dependencias innecesarias.** Antes de añadir una crate, justifícalo. Preferir `std` cuando sea razonable.
10. **Documentación con ejemplos en items públicos.** `#![warn(missing_docs)]` en `core`.

## Dependencias permitidas de partida

```toml
# core
serde = { version = "1", features = ["derive"] }
toml = "0.8"
thiserror = "1"
tracing = "0.1"
walkdir = "2"
tempfile = "3"
sha2 = "0.10"           # para hashing de snapshots
flate2 = "1"            # tarballs comprimidos
tar = "0.4"

# binary
clap = { version = "4", features = ["derive"] }
ratatui = "0.28"
crossterm = "0.28"
anyhow = "1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
color-eyre = "0.6"
```

Cualquier otra dep requiere justificación explícita en el PR/commit.

## Roadmap por fases

Cada fase termina con un release etiquetado y notas de cambios.

### Fase 0 — Fundación
- Workspace Cargo, CI con clippy + fmt + test
- Estructura de módulos vacíos en `core`
- `archdots --version` funciona

### Fase 1 — Detección y perfiles
- `Detector` con lista curada de paths conocidos
- Schema TOML del perfil (ver `docs/PROFILE_SCHEMA.md`)
- `archdots init` genera perfil base escaneando `~/.config`
- `archdots profile list|show|delete`

### Fase 2 — Apply seguro (núcleo crítico)
- `Linker` con symlinks atómicos
- `SnapshotManager` (tarball gzipped en `$XDG_DATA_HOME/archdots/snapshots/`)
- Journal de transacción en `$XDG_STATE_HOME/archdots/journal.toml`
- `archdots apply --dry-run` con diff coloreado
- `archdots apply` con confirmación y snapshot automático
- `archdots rollback` reaplica el snapshot más reciente

### Fase 3 — Validación de dependencias
- Wrapper sobre `pacman -Q` (un solo proceso, parsea stdout)
- Detección de AUR vía `pacman -Qm`
- Parsers ligeros para bspwmrc, hyprland.conf, i3 config, .zshrc, .bashrc
- `archdots check <profile>` reporta missing deps

### Fase 4 — TUI básica
- Vistas: Profiles, Diff, Dependencies, Snapshots
- Navegación con vim keys (`hjkl`) y tab
- Apply interactivo con preview

### Fase 5 — README autogenerado
- `archdots export <profile>` genera carpeta lista para subir a GitHub
- README.md con paquetes, comandos de instalación, estructura

### Post-MVP
- Sandbox real (XDG redirect + sesión WM aislada)
- Templates con variables (`{{hostname}}`, `{{user}}`)
- Sync con repo git remoto
- Plugin system

## Convenciones de commits

Conventional Commits estrictos:
- `feat(core): ...`
- `fix(tui): ...`
- `refactor(linker): ...`
- `docs: ...`
- `test(profile): ...`
- `chore: ...`

## Cómo trabaja Claude Code en este repo

1. **Lee este archivo siempre antes de empezar una sesión.**
2. **Antes de implementar, propone un plan corto en el chat.** No empieces a escribir código hasta confirmación si la tarea supera ~50 líneas.
3. **TDD cuando sea razonable.** Para lógica de `core`, escribe el test antes.
4. **Ejecuta `cargo clippy --all-targets -- -D warnings` y `cargo test` antes de declarar terminada una tarea.**
5. **Si una decisión arquitectónica afecta a más de un módulo, documéntala en `docs/ARCHITECTURE.md` con un mini-ADR (Architecture Decision Record).**
6. **No introduzcas dependencias nuevas sin preguntar.**
7. **Commits pequeños y atómicos.** Un commit = un cambio lógico.

## Lo que NO debe hacer Claude Code

- No usar `unwrap`/`expect` en producción.
- No hacer "drive-by refactors" en archivos que no son el objetivo de la tarea actual.
- No añadir features fuera del roadmap actual sin discutirlo.
- No tocar `$HOME` del usuario en tests. Usar `tempfile::TempDir` siempre.
- No asumir que `pacman` o un WM concreto está instalado al testear.