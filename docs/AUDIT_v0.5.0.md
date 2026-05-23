# Auditoría archdots v0.5.0

- **Fecha:** 2026-05-23
- **Commit auditado:** `fe0bbde1dac3d7fd6410a36fe205990721271247`
- **Alcance:** todo el workspace (crates/archdots-core, crates/archdots, data/, docs/, Cargo.toml/Cargo.lock, tests/).
- **No incluye:** ejecución sobre un sistema Arch real, ni interacción con un TTY real (ver "Lo que NO pude verificar").
- **Método:** lectura de docs y código, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`. `cargo audit` no está instalado (ver §Estado de build).

---

## Resumen ejecutivo

El proyecto está, en lo grueso, bien hecho. La capa transaccional de `apply`
(snapshot → journal → linker → lock) se sostiene a base de
`tempfile → fsync → rename` y un `flock(2)` advisory por FD, con orphan
recovery explícito; la capa de seguridad de `export` (filtro de rutas
sensibles + tamaño/binario + escáner de secretos, con doble check sobre
target y `canonicalize(source)`) está bien pensada y razonablemente
probada (~605 tests, todos pasando, 5 ignored documentados). `cargo
clippy -D warnings` y `cargo fmt --check` están limpios.

Sin embargo, la auditoría descubre un **fallo crítico end-to-end**: el
flujo `archdots init` + `archdots apply` (la combinación que el README
invita a probar primero) **convierte cada dotfile detectado en un
symlink autoreferencial** (`~/.zshrc → ~/.zshrc`, ELOOP al leerlo). El
contenido original sobrevive en el snapshot pre-apply, por lo que el
daño es reversible vía `archdots rollback`, pero entre `apply` y
`rollback` los ficheros no son utilizables por nada que los lea
(shells, editores, WMs). Adicionalmente, los mismos perfiles
init-generados son inutilizables vía `archdots export` y `archdots
check`. Causa raíz: `FileEntry.source` se resuelve con bases distintas
según el subcomando (`apply`/`diff`/TUI usan `$HOME`;
`export`/`check`/API documentada usan el `profile_dir` real). Los tests
no detectan esto porque cada subcomando se prueba de forma aislada con
su propia convención. Hallazgo **COR-01**, severidad **CRÍTICO**.

Otros bloques pendientes: documentación oficial desincronizada
(README declara v0.4.0 y "Fase 5 planned"; falta cualquier mención de
`archdots export`); merge no-atómico en `--force` que rompe el contrato
"atomic write" de ADR-005; varios `expect()` sobre datos embebidos que
contradicen la regla de Claude.md.

### Veredicto por área

| Área | Estado |
|---|---|
| 1. Seguridad y manejo de datos | **OBSERVACIONES** |
| 2. Corrección | **ACCIÓN REQUERIDA URGENTE** (COR-01 destruye dotfiles en disco) |
| 3. Calidad del código | OBSERVACIONES |
| 4. Cobertura de tests | **ACCIÓN REQUERIDA** (test e2e ausente que ocultó COR-01) |
| 5. Dependencias y build | OK |
| 6. Documentación vs realidad | **ACCIÓN REQUERIDA** (por DOC-01/02) |
| 7. UX y ergonomía | OBSERVACIONES |

---

## Hallazgos

### CRÍTICO

#### COR-01 — `source` se resuelve con bases distintas en `apply` vs `export`/`check`; resultado: `apply` sobre un perfil de `init` crea symlinks autoreferenciales que rompen los dotfiles en disco

- **Ficheros:** `crates/archdots/src/cmd/apply.rs:33`,
  `crates/archdots/src/cmd/diff.rs:25`,
  `crates/archdots/src/tui/tasks.rs:135`,
  `crates/archdots/src/tui/views/diff.rs:122`
  **versus**
  `crates/archdots/src/cmd/export.rs:514,570`,
  `crates/archdots-core/src/exporter/mod.rs:873`,
  `crates/archdots-core/src/validator/engine.rs:122`,
  con la API en `crates/archdots-core/src/profile.rs:394-419` (firma
  `Profile::resolved_entries(profile_dir, ctx)` y
  `Profile::resolve_source(entry, profile_dir)`).
- **Descripción:** la API y el docstring (`profile.rs:9-14` y
  `profile.rs:408`) declaran que `FileEntry.source` es **relativa al
  directorio del perfil**, y `resolve_source` lo une a `profile_dir`.
  Pero todas las llamadas desde `cmd/apply`, `cmd/diff` y la TUI pasan
  **`&home`** como `profile_dir`, en clara contradicción con la API.
  `cmd/export` y `validator::engine` sí pasan el `profile_dir` real
  (`$XDG_CONFIG_HOME/archdots/profiles/`). El comando `init`, además,
  guarda `source = rel.to_path_buf()` donde `rel = path.strip_prefix(home)`
  (`cmd/init.rs:122`), por lo que las rutas que produce solo resuelven
  correctamente si la base es `$HOME` — es decir, contra la
  interpretación de `apply`, no la de `export`/`check`.
- **Impacto:** un perfil generado por `archdots init` (el flujo
  *canónico* que documenta el README) tiene `source = ".bashrc"`,
  `.config/...`, etc. Resultado por subcomando:
  - `apply`/`diff`/TUI: funciona, porque pasan `&home` y los ficheros
    sí existen ahí.
  - `validator` / `archdots check`: lee `$XDG_CONFIG_HOME/archdots/
    profiles/.bashrc`, no existe, emite warning `ConfigUnreadable` por
    cada entrada y produce un reporte vacío de mentions (no detecta
    deps implícitas).
  - `archdots export`: `plan_entry` canonicaliza
    `$XDG_CONFIG_HOME/archdots/profiles/.bashrc`; con `required=true`
    devuelve `ExportError::Io` (exit 3); con `required=false` clasifica
    como `MissingSource`. Es decir, **`archdots export` no funciona
    sobre perfiles generados por `archdots init`**, que es la combinación
    natural que el README invita a probar.
  Ningún test ejercita `init → apply`, `init → export` ni `init → check`
  en cadena, por lo que el fallo es invisible para la suite (cobertura
  aparente vs. real — el problema que el brief pedía buscar).
- **Reproducción literal (verificada durante esta auditoría):**

  ```bash
  # Sandbox HOME para no tocar tu $HOME real.
  SB=/tmp/archdots-sandbox/home
  mkdir -p $SB/.config/bspwm
  echo '#!/bin/sh' > $SB/.config/bspwm/bspwmrc
  echo '# my zshrc' > $SB/.zshrc

  HOME=$SB XDG_CONFIG_HOME=$SB/.config \
    XDG_DATA_HOME=$SB/.local/share \
    XDG_STATE_HOME=$SB/.local/state \
    archdots init --name baseline
  # → profile saved with source=".zshrc", target="~/.zshrc"

  HOME=$SB ...  archdots apply baseline --yes
  # → 4 linked, 0 skipped, 0 failed

  ls -la $SB/.zshrc
  # lrwxrwxrwx ... .zshrc -> /tmp/archdots-sandbox/home/.zshrc
  #                          ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
  #                          symlink a sí mismo
  cat $SB/.zshrc
  # cat: .zshrc: Too many levels of symbolic links (os error 40)
  ```
- **Severidad:** **CRÍTICO**. No es corrupción permanente (el snapshot
  pre-apply contiene el original íntegro y `archdots rollback` lo
  restaura), pero entre el `apply` y el `rollback` cada dotfile queda
  ilegible. Para un usuario que aplica un perfil "y luego abre una
  shell" el sistema es funcionalmente inservible: zsh no carga
  `.zshrc`, bspwm no encuentra `bspwmrc`, etc. Para cualquier flujo
  desatendido (CI, scripts) el daño se manifiesta como fallos
  encadenados antes de poder ejecutar `rollback`.
- **Fix sugerido:** decidir una semántica y alinear. Dos opciones:
  - **(a) "source es relativa a $HOME"** (lo que el flujo real hace):
    actualizar el docstring de `FileEntry.source`, `Profile::resolve_source`
    y `Profile::resolved_entries`; cambiar las llamadas de
    `export.rs:570` y `validator/engine.rs:122` para pasar `&home` en
    vez de `profile_dir`. Aceptar que `Profile::resolve_source` ya no
    valida "no escapa del repo" en su sentido literal (sigue valiendo:
    valida "no escapa de la base que le pasen").
  - **(b) "source es relativa a profile_dir"** (lo que la API
    documenta): cambiar `cmd::init` para que copie/mueva los ficheros
    detectados dentro de `profile_dir/` (gestor de dotfiles real);
    cambiar `cmd/apply.rs`, `cmd/diff.rs`, `tui/tasks.rs` y
    `tui/views/diff.rs` para pasar `profile_dir` en lugar de `&home`;
    documentar la migración en el CHANGELOG (rompe perfiles
    existentes). Esta opción es la más coherente con el resto del
    diseño, pero requiere migrar perfiles.
  - Añadir un test e2e que ejecute `init` y luego `apply`+`check`+`export`
    sobre el perfil resultante, para que la inconsistencia no vuelva a
    pasar inadvertida.

### ALTO

#### DOC-01 — README desactualizado en versión, alcance y comandos disponibles
- **Mitigación adicional dado COR-01:** hasta que se arregle, el README
  induce activamente al usuario a romper sus dotfiles. Añadir un aviso
  al bloque "What works today" (e.g. "v0.5.0 has a known bug — see
  AUDIT_v0.5.0.md COR-01; do not run `archdots apply` outside a
  sandboxed `$HOME` until fixed").

- **Fichero:** `README.md:7,17,216`
- **Descripción:** el README declara "archdots is at v0.4.0" (línea 7),
  marca Fase 5 como "planned" (línea 17), y no menciona el subcomando
  `archdots export` en ningún sitio, pese a que la versión publicada en
  `Cargo.toml` es 0.5.0 y la Fase 5 está mergeada y publicada en
  CHANGELOG. Además, "Requires Rust 1.75 or later" (línea 216)
  contradice `Cargo.toml` (`rust-version = "1.85"`) — quien intente
  compilar con 1.75 verá un error de toolchain, no la versión que el
  README anuncia.
- **Impacto:** quien aterrice en el repo desde crates.io o GitHub no
  encuentra documentado el comando que la release destaca, ni los flags
  de seguridad (`--include-secrets`, `--allow-path`, etc.). La
  inconsistencia "v0.4.0" vs. "0.5.0" socava la credibilidad del resto
  de la documentación.
- **Severidad:** ALTO (para una herramienta cuya tagline principal en
  v0.5.0 *es* el export).
- **Fix sugerido:** actualizar README:
  1. Cambiar "v0.4.0" → "v0.5.0" y mover Fase 5 a "✅ done".
  2. Añadir sección `**archdots export <profile>**` con flags
     principales y referencia a los códigos de salida 0/2/3 igual que
     `check`.
  3. Corregir "Requires Rust 1.75 or later" → "1.85 or later" (o lo
     que `rust-toolchain.toml` indique como fuente única).
  4. Quitar el ejemplo `archdots rollback --to <snapshot-id>`
     (líneas 67-69): ese flag no existe en `main.rs` (ver DOC-04).

#### DOC-02 — `archdots export` no aparece en el README (impacto público)

- **Fichero:** `README.md` (ausencia).
- **Descripción:** la primera línea de la CHANGELOG 0.5.0 anuncia
  `archdots export`. El README no lo enseña, no lista sus exit codes
  (0/2/3), no menciona `--check`, ni `--include-secrets`, ni la pareja
  `--format full|profile-only`.
- **Impacto:** descubrir la feature de "killer" del release requiere
  abrir el CHANGELOG, el ADR-005 o `--help`. Para una utilidad CLI con
  pretensión de adopción es relevante.
- **Severidad:** ALTO.
- **Fix sugerido:** ver DOC-01.

### MEDIO

#### SEC-01 — `Exporter::write` con `--force` y `output_dir` existente: merge no-atómico

- **Fichero:** `crates/archdots-core/src/exporter/mod.rs:1240-1285`
  (`finalize_staging`, `merge_dir_into`).
- **Descripción:** ADR-005 (`docs/ARCHITECTURE.md:312-318`) y CHANGELOG
  prometen "atomic write via staging-dir rename". El código real solo
  hace un `rename` atómico cuando `output_dir` **no existe**. Cuando
  `output_dir` ya existe (caso `--force`), `merge_dir_into` recorre
  recursivamente el staging y hace `rename` archivo a archivo. Si
  cualquier `rename` falla a mitad de camino:
  1. el bucle aborta con `Err`,
  2. el `let _ = std::fs::remove_dir_all(&staging)` en `write` limpia
     el staging,
  3. **`output_dir` queda con un mix de ficheros nuevos (los que ya
     se mergeoron) y viejos (los que aún no se tocaron)**.
  No hay rollback ni log de qué ficheros estaban en cuál estado.
- **Impacto:** la promesa "el usuario nunca verá un export a medias"
  no se sostiene en el camino `--force`. El daño está acotado al
  destino que eligió el usuario, no a `$HOME`, pero el contrato
  documentado se rompe. Adicionalmente, `merge_dir_into` no elimina
  ficheros viejos que el nuevo export ya no incluye (un export más
  pequeño deja basura).
- **Severidad:** MEDIO.
- **Fix sugerido:**
  - Opción simple (alineada con ADR-005): si `output_dir` existe y
    `--force`, hacer `remove_dir_all(output_dir)` primero, luego
    `rename(staging, output_dir)`. Aceptar el riesgo de que un
    `remove_dir_all` también puede fallar a mitad y dejar
    `output_dir` parcialmente borrado — pero al menos el resultado
    es claramente "destruido", no "mezclado".
  - Opción más robusta: `rename(output_dir, output_dir.with_extension("archdots-backup.<rand>"))`,
    luego `rename(staging, output_dir)`, luego eliminar el backup. Si
    falla cualquier paso intermedio, el backup permite restaurar.
  - Mínimo: actualizar ADR-005 y CHANGELOG para describir el
    comportamiento real cuando `--force` y `output_dir` ya existe.

#### SEC-02 — `SnapshotManager::create` lee `$HOME` del proceso, no como parámetro

- **Fichero:** `crates/archdots-core/src/snapshot.rs:274-353,567-575`
  (función `home_dir()`).
- **Descripción:** `SnapshotManager::create` y `restore` llaman a una
  `home_dir()` interna que hace `env::var_os("HOME")`. El parámetro
  `home` no está expuesto en la API de `SnapshotManager`. Esto produce
  dos problemas:
  1. **Tests:** los tests del `Linker` crean un `TempDir` para "home"
     pero el `SnapshotManager` adyacente sigue leyendo el `$HOME` del
     proceso. Pasan porque `strip_prefix(home)` falla y cae al
     `unwrap_or(abs_path)`, devolviendo paths absolutos en lugar de
     relativos. Es decir, el snapshot funciona pero captura paths
     no-relativos al hogar lógico del test.
  2. **Producción:** si `$HOME` cambia entre `create` y `restore`
     (caso patológico: usuario re-loguea con otro home, o ejecuta
     como otro usuario), `restore` reconstruye paths con un home
     distinto. El usuario podría restaurar en sitios inesperados.
- **Impacto:** corrección y robustez. No es vulnerabilidad real
  (requiere control del entorno entre dos invocaciones), pero contradice
  la regla de Claude.md "No tocar $HOME del usuario en tests".
- **Severidad:** MEDIO.
- **Fix sugerido:** añadir `home: PathBuf` a `CreateRequest` y
  `RestoreOptions`, o aceptarlo en `SnapshotManager::open(data_home,
  home)`. Eliminar `home_dir()` interno. Los callers (`Linker`) ya
  tienen el home en sus `ApplyOptions`.

#### COR-02 — `cmd::diff::run` reporta "identical to source" para ficheros que falló leer

- **Fichero:** `crates/archdots/src/cmd/diff.rs:49-54`.
- **Descripción:** ambos lados del diff se leen con
  `fs::read_to_string(...).unwrap_or_default()`. Si el fichero contiene
  bytes no-UTF8 (binario), o no se puede leer por permisos, ambos
  lados quedan a `""` y la comparación `src_content == tgt_content`
  da `true`. El usuario ve `<path>: identical to source` cuando, en
  realidad, no se ha podido leer ni una cosa ni la otra.
- **Impacto:** UX engañosa. El usuario podría aplicar creyendo que no
  hay cambios cuando el contenido sí difiere.
- **Severidad:** MEDIO.
- **Fix sugerido:** distinguir errores: en cada `read_to_string` que
  retorne `Err`, imprimir explícitamente `<path>: binary or unreadable —
  cannot diff` y `continue;` antes de comparar.

#### COD-01 — Ficheros duplicados/muertos: `src/init.rs` y `src/profile_cmds.rs`

- **Ficheros:** `crates/archdots/src/init.rs` (185 líneas) y
  `crates/archdots/src/profile_cmds.rs` (173 líneas).
- **Descripción:** `main.rs` solo declara `mod cmd; mod diff_util; mod
  tui; mod xdg;`. Por lo tanto, `init.rs` y `profile_cmds.rs` **no se
  compilan**: son código muerto que vive en disco. `init.rs` es casi
  idéntica a `cmd/init.rs` (le sobran los comentarios de sección);
  `profile_cmds.rs` mantiene aún la versión sin refactorizar de
  `run_list` (open-coded loop) que ADR-004 dice que se reemplazó por
  `Profile::list_names`. El CHANGELOG 0.2.0 ya menciona "Restructured
  cmd/ module; init.rs and profile_cmds.rs now live under cmd/" — la
  refactorización no acabó de limpiarse.
- **Impacto:** confusión al navegar el repo; las llamadas al método
  refactorizado parecen referenciadas desde dos sitios distintos al
  hacer grep.
- **Severidad:** MEDIO (cosmético-pero-engañoso).
- **Fix sugerido:** `git rm crates/archdots/src/init.rs
  crates/archdots/src/profile_cmds.rs` y verificar que `cargo
  build --workspace` sigue limpio.

#### DOC-03 — Versión mínima de Rust contradictoria

- **Fichero:** `README.md:216` ("Rust 1.75") vs. `Cargo.toml:8`
  (`rust-version = "1.85"`).
- **Impacto:** un usuario en Rust 1.75 (la versión que el README pide)
  no puede compilar — error de toolchain. Cubierto por DOC-01 pero lo
  separo porque tiene fix independiente y obvio.
- **Severidad:** MEDIO.
- **Fix sugerido:** sincronizar con `rust-version` y `rust-toolchain.toml`.

#### DOC-04 — `archdots rollback --to <id>` documentado pero no implementado

- **Fichero:** `README.md:67-69` vs. `crates/archdots/src/main.rs:71-79`
  (`Commands::Rollback` solo expone `--profile` y `--yes`).
- **Descripción:** el README enseña `archdots rollback --to <snapshot-id>`
  como uso normal. `main.rs` no define ese flag; el flag haría falta para
  exponer `Linker::rollback_to_snapshot`, que sí existe en `core` y la
  TUI sí lo usa, pero el CLI no.
- **Impacto:** el usuario intenta `rollback --to ...`, clap responde
  "unexpected argument". Documentación mintiendo.
- **Severidad:** MEDIO.
- **Fix sugerido:** o exponer el flag en `main.rs` (deseable; el core
  ya lo soporta), o quitar el ejemplo del README. Si se expone:
  añadir test CLI que verifique el código de salida y el efecto.

### BAJO

#### COR-03 — `cmd::apply::run` mezcla `std::process::exit` con `anyhow::Result<()>`

- **Fichero:** `crates/archdots/src/cmd/apply.rs:74,125`.
- **Descripción:** `apply` devuelve `anyhow::Result<()>` pero llama
  `std::process::exit(2)` para conflictos y `exit(1)` para rolled_back/
  failed. `cmd::check` y `cmd::export`, en cambio, devuelven
  `Result<i32>` y la rama en `main.rs` hace el `process::exit(code)`.
  La inconsistencia no es funcional (no hay locks vivos en esos puntos)
  pero rompe el patrón.
- **Severidad:** BAJO.
- **Fix sugerido:** alinear con `check`/`export`: cambiar la firma a
  `Result<i32>` y mover el `process::exit` a `main.rs`.

#### COR-04 — `expect()` sobre datos embebidos en código de producción

- **Ficheros:**
  - `crates/archdots-core/src/exporter/mod.rs:343` ("embedded
    sensitive_paths.toml is valid")
  - `crates/archdots-core/src/validator/engine.rs:30` ("builtin_filter.toml
    must be valid TOML")
  - `crates/archdots-core/src/packages/providers.rs:70` ("embedded
    binary_providers.toml must parse cleanly")
  - `crates/archdots-core/src/detector.rs:185` ("catalog must be valid
    TOML")
- **Descripción:** Claude.md dice "Cero `unwrap()` y cero `expect()` en
  código de producción. Solo permitidos en tests y en `main.rs` para
  errores fatales muy específicos". Estos cuatro `expect` están en
  módulos de `core` y se ejecutan en caminos calientes (cada
  `Exporter::new`, cada `Validator::validate`, cada `Detector::new`,
  cada `PackageDB::new`). En teoría no pueden fallar (TOML embedded por
  `include_str!`), pero un cambio descuidado en el data file deja al
  binario en release con un panic sin contexto.
- **Severidad:** BAJO. Defendibles (invariantes verdaderas verificadas
  por los tests de catálogo), pero contradicen la regla del proyecto.
- **Fix sugerido:** convertir a `Result<…, CoreError>` y propagar; o
  bien `OnceLock` + `try_parse` al inicio, devolviendo el error
  tipado. `scanner.rs` ya hace bien esto en `SecretScanner::new()`
  (devuelve `ExportError::ScannerInit`); replicar el patrón.

#### COD-02 — Lógica de exit-code duplicada en `cmd::check`

- **Fichero:** `crates/archdots/src/cmd/check.rs:96-108`, `195-202`,
  `302-320`.
- **Descripción:** la regla "si `--strict` y hay implicit-missing, sube
  exit a 1" está copiada-pegada en tres sitios (la propia `run`, el
  resumen de `render_table`, y el header JSON). Si la política cambia,
  hay que tocar tres lugares. Riesgo de divergencia.
- **Severidad:** BAJO.
- **Fix sugerido:** extraer a `fn effective_exit_code(report,
  strict) -> i32` privado.

#### COD-03 — `exporter/mod.rs` mide 2911 líneas

- **Fichero:** `crates/archdots-core/src/exporter/mod.rs`.
- **Descripción:** ~1360 líneas de código productivo + ~1550 de tests
  inline. La parte productiva podría partirse: `plan` (clasificación
  por entrada), `write`/`finalize_staging`, `render_readme`,
  `build_exported_profile`, helpers `is_text_sniff` / `read_first_bytes`
  podrían vivir en sub-módulos. El submódulo `template` ya lo hace.
- **Severidad:** BAJO.
- **Fix sugerido:** mover tests a un módulo separado (`exporter::tests`
  como fichero) reduce el módulo a una talla legible. Refactor más
  ambicioso: dividir en `exporter::plan`, `exporter::write`,
  `exporter::render`. No urgente.

#### COD-04 — `BackgroundKind::RefreshSnapshots` existe en producción solo para tests

- **Fichero:** `crates/archdots/src/tui/tasks.rs:41-43`.
- **Descripción:** la variante tiene `#[allow(dead_code)]` con comentario
  "spawned in tests; production views refresh synchronously via
  `SnapshotsView::refresh`". Una variante de enum *público* del crate
  binario existe solo para que un test la dispare. Olor a diseño.
- **Severidad:** BAJO.
- **Fix sugerido:** gatear la variante con `#[cfg(test)]`, igual que
  hace `PanicForTest`, y proveer el test desde el módulo en cuestión.

#### COD-05 — `TaskId` reservado para detección stale, nunca comparado

- **Ficheros:** `crates/archdots/src/tui/app.rs:46-47` y
  `crates/archdots/src/tui/tasks.rs:66-68`.
- **Descripción:** ADR-004 lo reconoce explícitamente como deuda
  aceptada. La estructura `Running { id, kind, started }` lleva el `id`
  pero `drain_task_messages` no lo compara con `Completed.id`. Sin
  reinicio del proceso el escenario "task viejo termina cuando ya
  estamos en uno nuevo" es prácticamente imposible (no se puede
  dispararse otro task estando en `Running`), pero la promesa del ADR
  queda colgada.
- **Severidad:** BAJO.
- **Fix sugerido:** comparar en `drain_task_messages`; si los `id` no
  coinciden, descartar el mensaje en lugar de transicionar a `Idle`.

#### TST-01 — `github_token_negative_too_short` está malformulado

- **Fichero:** `crates/archdots-core/src/exporter/scanner.rs:218-228`.
- **Descripción:** la primera aserción es una `OR` extraña:
  `!rule_ids("ghp_AAAA...(36 chars)").is_empty() ||
   !rule_ids("ghp_SHORT1234").contains("github-token")`. Funciona como
  test (es lógicamente: "la regex es razonable: o bien matchea cosas
  largas, o bien no matchea cosas cortas"), pero es confusa y el
  nombre del test es engañoso (dice "too_short" cuando el primer caso
  prueba lo contrario). La segunda aserción aislada (no-match de
  `ghp_SHORT1234`) es la única que de verdad prueba lo del nombre.
- **Severidad:** BAJO.
- **Fix sugerido:** dividir en dos tests: `github_token_36_chars_matches`
  y `github_token_short_does_not_match`.

#### TST-02 — Cero tests end-to-end de `init` → `apply`/`check`/`export`

- **Ficheros:** `crates/archdots/tests/cli.rs`, `cli_phase2.rs`,
  `cli_check.rs`, `cli_phase5.rs`.
- **Descripción:** cada fichero de tests define su propio
  `write_profile_*` con la convención que le conviene al subcomando que
  prueba. No hay ningún test que ejecute `archdots init …` y luego
  `archdots apply` (o `check`, o `export`) sobre el perfil resultante.
  Es exactamente la combinación que falla por COR-01 — y el motivo de
  que no se haya detectado.
- **Severidad:** BAJO (estructural; el bug real es COR-01).
- **Fix sugerido:** añadir al menos un test que recorra el ciclo:
  `touch ~/.zshrc; archdots init --name x; archdots check x; archdots
  export x --check`. Marca explícita: el test debe llamar al binario
  para `init` también, no construir el `.toml` a mano.

#### DEP-01 — `walkdir` y `color-eyre` listados en workspace deps sin consumidores

- **Fichero:** `Cargo.toml:21,28`.
- **Descripción:** ningún `Cargo.toml` de los crates de miembros añade
  `walkdir.workspace = true` ni `color-eyre.workspace = true`. Búsqueda
  por `use walkdir` y `use color_eyre` no devuelve nada. Listar deps en
  `[workspace.dependencies]` sin consumirlas no afecta al binario, pero
  contamina `cargo update` y la documentación de versiones permitidas.
- **Severidad:** BAJO.
- **Fix sugerido:** eliminar las dos líneas.

#### DOC-05 — `CONTRIBUTING.md` enlaza `CLAUDE.md` pero el fichero es `Claude.md`

- **Fichero:** `CONTRIBUTING.md:3`.
- **Descripción:** el enlace `[CLAUDE.md](CLAUDE.md)` no resuelve en
  sistemas de fichero case-sensitive (Linux estándar); el fichero
  existe en disco como `Claude.md`. GitHub trata los enlaces como
  case-sensitive en la UI, así que el click se rompe.
- **Severidad:** BAJO.
- **Fix sugerido:** renombrar el fichero a `CLAUDE.md` (consistente con
  la convención mayúsculas para documentos en raíz, y con cómo otros
  proyectos usan ese nombre), o ajustar el enlace.

#### DOC-06 — Inconsistencia menor `snapshots get` vs `snapshots show`

- **Fichero:** `CHANGELOG.md:91` ("`archdots snapshots list|get|prune`")
  vs. `main.rs:144-167` (subcomandos `List`, `Show`, `Prune`).
- **Severidad:** BAJO.
- **Fix sugerido:** sustituir `get` por `show` en el CHANGELOG de 0.2.0.

#### DOC-07 — README ejemplo de `snapshots show` usa formato fecha, el real espera ULID

- **Fichero:** `README.md:86` (`archdots snapshots show 20240501-120000`).
- **Descripción:** los snapshots se identifican por ULID (no por
  fecha-hora). Además `run_show` (cmd/snapshots.rs:91) exige
  `id_prefix.len() >= 6`. El ejemplo es engañoso.
- **Severidad:** BAJO.
- **Fix sugerido:** usar un prefijo ULID realista, p.ej.
  `archdots snapshots show 01HVAB7K`.

#### SEC-03 — Symlink en `source` puede exfiltrar ficheros fuera de `$HOME` vía `export`

- **Fichero:** `crates/archdots-core/src/exporter/mod.rs:927-995`
  (canonicalize sin verificar contención).
- **Descripción:** si una entrada de perfil tiene `source` que es un
  symlink resolviendo a, p.ej., `/etc/X11/xorg.conf` (fuera de `$HOME`,
  pero world-readable), `plan_entry` lo canonicaliza, el chequeo de
  ruta sensible solo aplica si la canonicalización está bajo `$HOME`
  (porque `strip_prefix(self.home).ok()` devuelve `None`), y el
  fichero se copia al export sin más restricción que el filtro
  binario/tamaño. El operador del perfil termina publicando contenido
  fuera de `$HOME` que no es propiamente "su dotfile".
- **Impacto:** real, pero controlado por el propio usuario: requiere
  un symlink en su `profile_dir`. No es una vulnerabilidad clásica —
  *self-foot-gun*. Comentado como "by design" en PHASE_5_DESIGN.md
  (sólo `$HOME`-relative paths atraviesan el filtro de path
  sensitive), pero el filtro de path sensitive es la única barrera y
  no es de "contención" sino de "denylist".
- **Severidad:** BAJO (riesgo bajo, requiere control del propio
  usuario; pero merece nota en ADR-005).
- **Fix sugerido:** en `plan_entry`, si `source_canonical` no cae bajo
  `$HOME`, clasificar como `OutsideHome {}` igual que cuando lo es el
  target. Documentar el cambio en ADR-005 punto 2.

#### UX-01 — `confirm_write` aborta silenciosamente cuando stdin no es TTY y no se pasó `--yes`

- **Fichero:** `crates/archdots/src/cmd/export.rs:480-492` (y, por
  analogía, `cmd/apply.rs:88` y `cmd/profile.rs:160`).
- **Descripción:** sin TTY, `confirm_write` devuelve `Ok(false)`
  inmediatamente; `run` imprime "Aborted." y exit 1. Razonable, pero
  el mensaje de error no dice por qué (el usuario en CI obtiene
  "Aborted." sin pista de que necesita `--yes`).
- **Severidad:** BAJO.
- **Fix sugerido:** cuando stdin no es TTY y no se pasó `--yes`,
  imprimir un mensaje explícito en stderr ("stdin is not a TTY; pass
  --yes to non-interactive runs") y exit 3 (configuración inválida)
  en lugar de exit 1 (abort de usuario).

#### UX-02 — "Next steps" hace `git init` sin sugerir `cd <output>` explícito en negrita

- **Fichero:** `crates/archdots/src/cmd/export.rs:412-420`.
- **Descripción:** ya se imprime `cd <output_dir>` como primera línea
  del bloque, pero las líneas siguientes se ven como un bloque a
  copiar-pegar; el usuario distraído podría empezar en `git init` en
  el cwd actual.
- **Severidad:** BAJO.
- **Fix sugerido:** insertar `# run from inside the export dir:` antes
  de las cuatro líneas, o usar `cd <dir> && git init && …` como un
  one-liner.

---

## Cobertura de tests

**Estado actual:** 605 tests pasan, 5 ignorados, 0 fallidos.

| Módulo / camino | Cobertura | Comentario |
|---|---|---|
| `journal` (append, iter, orphan) | Buena | Cubre límite de 200 links, corrupción de líneas, esquema futuro. |
| `lock` | Buena (asume Linux/flock) | El test cross-process está `#[ignore]` y se ejecuta manualmente. Documentado. |
| `snapshot` (create/restore/list/prune) | Buena | Cubre symlinks, mode bits, payload sha mismatch. Hueco: no se verifica `host_info` vs un home distinto al del proceso (relacionado con SEC-02). |
| `linker` (plan, apply, rollback) | Buena | Incluye los 7 conflictos del planificador, idempotencia, fail-rollback. |
| `profile` (validación + resolución) | Buena | path_escapes_root, expand_vars, list_names, save/load roundtrip. |
| `validator` + `packages` | Buena con `MockRunner` | Hueco real: ningún test ejecuta el validator con `profile_dir == $HOME` (que es como `cmd/apply` lo invoca para sources). |
| `detector` | Buena | Catalog parse, scan_with_different_home_dirs. |
| `parsers/*` | Buena para `infer_kind` y `parse` | No hay tests de robustez para ficheros binarios accidentales (parsers asumen UTF-8). |
| `exporter::plan` + sensitive-path | Buena | Cubre los tres tipos (prefix, exact, suffix), el override allow-path, el dual-check target/source. |
| `exporter::scanner` (regex) | Aceptable | Positivos y negativos por regla; ver TST-01 sobre el test del github-token. |
| `exporter::write` (atómico) | Parcial | El happy-path tiene assert "no leftover .tmp". **No hay test del fallo a mitad del merge** (`finalize_staging` con `--force` y un error inyectado), que es exactamente el escenario de SEC-01. |
| `cmd::export` (parseo + JSON) | Buena | Forma de JSON, allow_secret parsing, gate include-secrets. |
| `cmd::apply` / `cmd::diff` / `cmd::rollback` (CLI) | Aceptable | Cubre los flujos básicos. No prueba `apply` justo después de `init` (COR-01 invisible). |
| `tui` views (rendering, dispatch) | Aceptable | Hay tests por vista. |
| `tui::tasks` (spawn, panic recovery) | Buena | `task_panic_caught_and_sent_as_error`, `spawn_returns_immediately`. |
| Recovery loop (orphan → recover) | Hueco | `cmd::recover` no tiene tests CLI. La lógica de `recover_one` solo se ejerce indirectamente vía `linker::tests`. |
| Diff util con binary/Unicode | Hueco | `cmd::diff` con un fichero binario muestra "identical" (COR-02), pero no hay test que lo afirme. |
| End-to-end `init → check → export` | **Ausente** | Es la combinación que destapa COR-01. |

Tests `ignored` (5):
1. `database_locked_returns_database_locked_error` (packages) — requiere
   `/var/lib/pacman/db.lck` o inyectar la ruta. Legítimo no-CI.
2. `cross_process_lock_pid` (lock) — fragilidad en sandbox CI. Legítimo.
3. `system_runner_real_command_returns_output` (packages::runner) —
   requiere `echo`. Razonable.
4. `app_copy_to_clipboard_ok_sets_status` (tui::app) — requiere display.
   Razonable.
5. `event_loop_next_returns_tick_when_no_input` (tui::app) — requiere TTY.
   Razonable.

Ninguno oculta cobertura crítica ausente; los marqué uno a uno y todos
son razones de entorno, no atajos.

---

## Estado de build

```
$ cargo --version
cargo 1.85.1 (d73d2caf9 2024-12-31)

$ cargo fmt --all -- --check
EXIT: 0  (limpio)

$ cargo clippy --workspace --all-targets -- -D warnings
EXIT: 0  (limpio)

$ cargo test --workspace
test result: ok. 142 passed; 0 failed; 2 ignored
test result: ok.  32 passed; 0 failed; 0 ignored   (cli)
test result: ok.   7 passed; 0 failed; 0 ignored   (cli_check)
test result: ok.  19 passed; 0 failed; 0 ignored   (cli_phase2)
test result: ok.  10 passed; 0 failed; 0 ignored   (cli_phase5)
test result: ok. 307 passed; 0 failed; 2 ignored   (lib)
test result: ok.  17 passed; 0 failed; 0 ignored   (detector)
test result: ok.  20 passed; 0 failed; 1 ignored   (packages)
test result: ok.  33 passed; 0 failed; 0 ignored   (profile)
test result: ok.  18 passed; 0 failed; 0 ignored   (validator)
test result: ok.   2 passed; 0 failed; 0 ignored   (doc-tests core)
                  ─────────
TOTAL             605 passed; 0 failed; 5 ignored
```

`cargo audit` no estaba instalado en el entorno de auditoría. No se
pudo comprobar CVEs en dependencias. **Recomendación:**
`cargo install cargo-audit` y añadir `cargo audit` al pipeline de CI
(.github/workflows/ci.yml), gated a falla solo si la severidad supera
un umbral.

---

## Lo que NO pude verificar

Esta sección lista lo que el método de auditoría no puede aseverar.
Una auditoría que oculta sus puntos ciegos no vale nada.

1. **Comportamiento sobre un sistema Arch real.** No se invocó `pacman`
   en ningún momento; `archdots check` solo se ejerció con
   `MockRunner`. La rama `--deep` (pacman -F) no se probó contra una DB
   real. Los códigos de salida 0/1/2/3 podrían divergir en un entorno
   con `pacman` faltante o DB locked, aunque la lógica del validator
   los cubre con `PackageError::PacmanMissing` / `DatabaseLocked`.
2. **Interacción con TTY real.** Los flujos
   `--include-secrets → "I UNDERSTAND"` y el `confirm_write` del CLI se
   probaron en modo non-TTY (que es lo que `assert_cmd` impone). El
   prompt visible y el comportamiento `^C` en un terminal real no se
   verificaron. La TUI tampoco se renderizó.
3. **Atomicidad bajo crash real.** No se simuló `SIGKILL` ni corte de
   energía durante un `apply` o un `export --force`. El protocolo
   journal-first / tmp+rename es correcto teóricamente; en la práctica
   depende del sistema de ficheros (ext4 vs btrfs vs zfs) y de
   `data=journal` vs `data=ordered`, fuera del alcance del proyecto.
4. **Permisos extremos.** No se ejecutó como root, con `$HOME` montado
   solo lectura, ni con cuotas llenas. El código maneja
   `PermissionDenied` en el planificador, pero el comportamiento bajo
   "disco lleno mientras se escribe el snapshot" depende del SO.
5. **CVEs en dependencias.** `cargo audit` no estaba instalado.
6. **Race conditions reales en el TUI.** El análisis es estático;
   reproducir condiciones de carrera entre el hilo de tarea y el de
   UI requeriría un fuzzer / loom. La lógica revisada (mpsc,
   `is_animating` antes de spawn) es razonable pero no probada
   exhaustivamente.
7. **Auditoría de Fase 5 a fondo.** Por instrucción explícita del
   brief, esta auditoría no re-litiga la Fase 5 en profundidad
   (eso lo hizo la auditoría dedicada). Las observaciones sobre
   `export` aquí se ciñen a su integración con el resto: COR-01
   (incompatibilidad con init/apply), SEC-01 (merge en --force) y
   SEC-03 (symlink fuera de $HOME).

---

## Recomendaciones priorizadas

Orden por *impacto / esfuerzo*, ascendente. Lo de arriba primero.

0. **Marcar v0.5.0 como yanked en crates.io (si llegó a publicarse)** y
   añadir un aviso CRÍTICO en el README hasta que COR-01 esté
   arreglado. Cualquier usuario que siga el flujo del README destruye
   sus dotfiles en disco (recuperable vía rollback, pero igualmente
   destructivo). Esto es lo más urgente.

1. **Resolver COR-01: alinear la semántica de `source`.**
   Elige (a) o (b) según el espíritu del producto. Yo recomendaría
   (b) — sources dentro del profile_dir — porque es lo que el
   diseño documenta, es lo que hace que `export` tenga sentido (sin
   esta convención el dotfile manager es un *symlink manager*) y es
   lo que la mayoría de gestores comparables hacen (stow, chezmoi).
   Eso significa:
   - Cambiar `cmd::init` para que copie/mueva los ficheros a
     `profile_dir`.
   - Cambiar `cmd::apply`, `cmd::diff`, `tui::tasks`,
     `tui::views::diff` para que pasen `profile_dir` (no `&home`)
     a `Profile::resolved_entries`.
   - Añadir un test e2e `init → check → export`.
   - Documentar la migración para perfiles previos (script
     opcional `archdots migrate-profile`).
2. **Actualizar el README a v0.5.0 y documentar `archdots export`**
   (DOC-01, DOC-02, DOC-03, DOC-04, DOC-07). Es un trabajo de pocas
   horas y multiplica el valor de la release.
3. **Eliminar `src/init.rs` y `src/profile_cmds.rs`** (COD-01). 5
   minutos. Limpia el grep.
4. **Documentar (o corregir) el comportamiento de `--force` en
   `Exporter::write` cuando el destino existe** (SEC-01). Decisión
   producto vs. diseño. Si se queda como `merge`, ADR-005 y CHANGELOG
   deben describirlo; idealmente se cambia a `rm -rf` + `rename` con
   backup, manteniendo la promesa de atomicidad.
5. **Quitar el `unwrap_or_default()` en `cmd::diff`** (COR-02). 10
   líneas, ya con un test directo.
6. **Convertir `Snapshot{Manager,Trigger}` a aceptar `home` por
   parámetro** (SEC-02). Llevará algo más; requiere tocar callers.
7. **Reemplazar los `expect()` sobre datos embebidos por errores
   tipados** (COR-04). El patrón ya está en `SecretScanner::new()`.
8. **Añadir `cargo audit` al CI** y correr periódicamente.
9. **Limpiezas COD-02 / COD-03 / COD-04 / COD-05** según oportunidad.
10. **Reforzar tests:** falta una prueba que rompa SEC-01 a propósito
    (`finalize_staging` con un fallo inyectado a mitad del merge); y
    falta el e2e mencionado en TST-02.

---
