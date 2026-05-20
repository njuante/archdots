# Phase 3 — Dependency Validation (Design)

Status: **approved** — implementation target for Phase 3.

This document specifies the design of the three core components that make
Phase 3 deliver `archdots check <profile>`:

1. `archdots-core::packages::PackageDB`
2. `archdots-core::parsers`
3. `archdots-core::validator::Validator`

Plus the binary-crate subcommand `archdots check`. All decisions below are
final unless an ADR supersedes them. The seven load-bearing decisions are
also recorded as ADR-003 in [`ARCHITECTURE.md`](./ARCHITECTURE.md).

---

## 1. Pipeline overview

```
            ┌──────────────────────────────────────────────┐
            │           archdots check <profile>           │
            └──────────────────────────────────────────────┘
                                   │
                                   ▼
                         ┌──────────────────┐
                         │  LOAD PROFILE    │  Profile::load_from_file
                         │                  │  → exit 3 on ProfileError
                         └──────────────────┘
                                   │
                                   ▼
                         ┌──────────────────┐
                         │  OPEN PACKAGEDB  │  pacman -Q + pacman -Qm
                         │                  │  (cached for process lifetime)
                         │                  │  → exit 3 if pacman missing
                         └──────────────────┘
                                   │
                                   ▼
                         ┌──────────────────┐
                         │  SCAN CONFIGS    │  for each FileEntry:
                         │                  │    infer_kind(source/target)?
                         │                  │    parse(kind, contents)
                         │                  │  → Vec<Mention> (no dedup)
                         └──────────────────┘
                                   │
                                   ▼
                         ┌──────────────────┐
                         │  CROSS-REFERENCE │  declared deps:
                         │                  │    db.is_installed(name)
                         │                  │  unique binaries from mentions:
                         │                  │    db.provider_of(binary, deep)
                         │                  │  db.detect_aur_helper()
                         └──────────────────┘
                                   │
                                   ▼
                         ┌──────────────────┐
                         │  BUILD REPORT    │  group mentions by binary
                         │                  │  compute warnings
                         │                  │  compute exit_code
                         └──────────────────┘
                                   │
                            ┌──────┴──────┐
                            ▼             ▼
                       text table    JSON (--json)
                            │             │
                            └─────┬───────┘
                                  ▼
                              exit code
```

### Read-only contract

`archdots check` mutates nothing on disk:
- It does **not** acquire `ApplyLock` (the read does not race with apply).
- It does **not** invoke any AUR helper.
- It does **not** invoke `pacman -Fy`. The user runs that manually as root.
- It does **not** modify the profile, journal, or snapshot store.

This is enforced by construction: the `Validator` only reads, and the CLI
command only emits to stdout.

---

## 2. Distro scope — Arch only

**Decision:** v0.3 targets Arch Linux exclusively. `PackageDB` is a single
concrete struct that hard-codes `pacman`. There is no `trait PackageDB`
abstraction for multi-distro support.

**Rationale:**

- The project is brand-coded as Arch-only across `README.md`, `Claude.md`,
  and killer feature #4. Diluting that focus is out of scope.
- Cross-distro is not "swap the package manager" — Debian's `libfoo-dev`
  has no Arch analogue, AUR has no apt equivalent, and metadata schemas
  diverge. A neutral trait would either leak distro concerns or paper
  over them with a useless lowest-common-denominator API.
- There is no second user today. Adding a trait for a hypothetical
  Debian user is the textbook premature abstraction `Claude.md` forbids.
- Refactoring `struct PackageDB → trait PackageDB` later is bounded:
  one concrete type, a handful of callers. Future-us can do it in an
  afternoon if Debian support ever becomes a real ask.

**Consequences:**

- `core` adds no new traits for distro abstraction.
- `archdots check` on non-Arch exits **3** with message `"archdots check
  requires an Arch-based system (pacman not found on PATH)"`.
- ADR-003 records the rejection so future contributors don't reopen it
  without a real second-distro use case.

A `CommandRunner` trait *does* exist (§7), but its purpose is testability
— it abstracts subprocess execution, not package managers.

---

## 3. PackageDB

### 3.1 Responsibilities

- Single source of truth for "is this package installed?" within one
  `archdots check` invocation.
- Resolves binary names to package names via a curated table and an
  optional `pacman -F` fallback.
- Detects which AUR helper (if any) is installed, for informational
  output. Never invokes it.

### 3.2 Caching model

`PackageDB` invokes `pacman` lazily and at most once per query family
per instance. Caches use interior mutability (`OnceCell` /
`RefCell<Option<…>>`) so the public API is immutably borrowable.

| Cache | Populated by | Trigger |
|---|---|---|
| `installed: HashMap<String, Pkg>` | `pacman -Q` | first call to `is_installed`, `lookup`, or `installed` |
| `aur: HashSet<String>` | `pacman -Qm` | first call that needs to classify `PkgSource` |
| `aur_helper: Option<AurHelper>` | `pacman -Q paru` / `pacman -Q yay` | first call to `detect_aur_helper` |
| `provider_cache: HashMap<String, ProviderHit>` | `provider_of` calls (incl. `pacman -F`) | per binary, memoised on first miss |

The cache lives for the process — there is no on-disk cache. A fresh
invocation re-queries pacman; this matches user expectations after they
install something.

### 3.3 AUR helper detection

`detect_aur_helper` returns `Some(Paru)` if both are present (paru wins
on tie). The CLI uses this only to render install hints (`paru -S foo`
vs `yay -S foo`); it never spawns the helper. Absence of a helper
combined with declared AUR deps surfaces
`ValidationWarning::AurDepsButNoHelper`.

### 3.4 Binary → package resolution

`provider_of(binary, deep)` proceeds in three steps:

1. **Curated lookup** — search the embedded `binary_providers.toml`
   table (§6.1). Hit → `ProviderHit::Curated`.
2. **Deep lookup** (only if `deep == true`) — invoke
   `pacman -F <binary>`. Hit → `ProviderHit::PacmanFiles`.
3. **Unknown** — return `ProviderHit::Unknown`.

If `deep == true` and `pacman -F` reports the file database is not
synced (stderr contains `"sync the database first"` or similar), return
`ProviderHit::FilesDbNotSynced` once and cache that signal so we don't
spam the warning. archdots **never** invokes `pacman -Fy`: it requires
root and is the user's call.

---

## 4. Parsers

### 4.1 Per-format parsers, shared helpers

Each config family has its own parser function. A generic table-driven
engine would either be too rigid (Hyprland's `bind = ..., exec, X`
needs structured field selection) or too configurable (a mini-DSL no
one maintains). The five parsers share two helpers:

- `strip_comments(line, comment_chars)` — removes `#` (every format)
  while respecting quoted strings.
- `join_continuations(content)` — folds lines ending with `\` into one
  logical line, keeping the originating line number of the *first*
  physical line.

### 4.2 Comment and continuation handling

| Format | Comment | Continuation |
|---|---|---|
| bspwm | `#` anywhere | `\` |
| sxhkd | `#` only when first non-whitespace | `\` (binding RHS) |
| hyprland | `#` anywhere | `\` |
| i3 / sway | `#` only when first non-whitespace | `\` |
| shell (.zshrc/.bashrc/.profile/.xprofile/.xinitrc) | `#` anywhere, respecting quotes | `\` |

### 4.3 Variables

Variable references (`$VAR`, `${VAR}`) are **not** resolved by parsers.
When a parser would emit a `Mention` whose binary token is purely a
variable reference, it instead emits no mention and records
`ValidationWarning::UnresolvedVariable { var, line, file }`. The user
can then declare the relevant package manually in `[dependencies]`.

Silently skipping was rejected: pretending the validator covered a
construct it ignored is dishonest.

### 4.4 Mentions are preserved; the validator groups

**Parsers never deduplicate.** A `.zshrc` with 30 `alias git-foo=git ...`
lines emits 30 `Mention` records, all sharing `binary = "git"` with
distinct line numbers. The validator (§5.2) is the layer that groups
mentions by binary and constructs one `DepEntry` per package.

Rationale: parser-level dedup would lose line/file provenance that the
validator's `--verbose` report wants to surface. The cost of carrying
duplicate `Mention`s into the validator is trivial — config files are
small and validators run interactively.

### 4.5 Per-format heuristics

| Parser | Extracts | Skips |
|---|---|---|
| `Bspwm` | `picom &`, `pgrep -x X \|\| X &`, `exec X` | `bspc rule -a Class ...` (X11 class, not a binary), `bspc config ...` |
| `Sxhkd` | first non-whitespace token of every binding's RHS | left-hand side (key chords), continuation operators |
| `Hyprland` | `exec = X`, `exec-once = X`, third arg of `bind*, KEY, exec, X` | `windowrule`, `monitor`, `decoration`, `general`, `input`, etc. |
| `I3Sway` | `exec X`, `exec_always X`, `bindsym KEY exec [--flags] X` | other directives (`set`, `font`, `bar`, `output`, `input`) |
| `Shell` | `alias name=X` (first token of X), `command -v X`, top-level `X arg…` invocations | shell builtins (filtered by validator); heredoc bodies (best effort) |

I3 and Sway use separate `ParserKind` variants (`I3` and `Sway`) but
share the same parser implementation — the syntax (`exec`,
`exec_always`, `bindsym ... exec`) is identical across the two.
Likewise, `Zsh`, `Bash`, and `Shell` each have their own `ParserKind`
variant but share implementation. The granularity is deliberate: it
allows the validator report to distinguish the source WM or shell even
when the extraction logic is identical.

### 4.6 Noise filter

The validator drops any mention whose binary appears in an embedded
allowlist of shell builtins and always-installed base packages,
shipped as `data/builtin_filter.toml` (§6.2). Without this, a typical
shell config produces hundreds of useless mentions (`ls`, `cd`,
`echo`, `grep`, …) that crowd the real signal.

False positives vs false negatives: the parser pipeline **favours
false negatives**. Heuristic under-reporting is better than confident
over-reporting. The CLI prints `Heuristic — review manually.` at the
top of any non-trivial report so the user knows the report is
advisory.

---

## 5. Validator

### 5.1 Pipeline

`Validator::validate(profile, profile_dir, opts)` runs:

1. For each `FileEntry`:
   - Resolve `entry.source` against `profile_dir`.
   - Resolve `entry.target` against the user's `ResolveCtx` (already
     used by Phase 2). An unresolved `$VAR` in the target is a
     `ProfileError::UnknownEnvVar` and propagates as
     `ValidatorError::Profile` → exit **3**.
   - Call `infer_kind(target_path)` (falls back to source path).
   - If parseable: read the source file, run the parser, accumulate
     mentions and warnings.
   - If unreadable: emit `ValidationWarning::ConfigUnreadable`.
2. For each declared dependency (`pacman`, `aur`, `optional_pacman`):
   build a `DepEntry` whose status comes from `PackageDB::is_installed`.
   Match the binaries from collected mentions against the dep name to
   tag `DepEntry::source` with the mentions that motivate it.
3. For each unique binary mentioned but **not** declared:
   - Filter against `builtin_filter.toml`.
   - `PackageDB::provider_of(binary, opts.deep)`.
   - Build a `DepEntry { kind: ImplicitFromConfig, ... }`.
4. Emit informational warnings: `DeclaredButUnused`,
   `AurDepsButNoHelper`, `PacmanDatabaseLocked`,
   `PacmanFilesDbNotSynced`, etc.

`Profile::hooks.pre_apply` / `post_apply` scripts are **not** parsed in
v0.3 — they are arbitrary shell, the false-positive rate is too high to
be worth the noise. Revisit in a later phase if a real need surfaces.

### 5.2 Grouping mentions into DepEntry

Mentions arrive raw and duplicated. The validator constructs one
`DepEntry` per unique binary:

- Filter out builtins (`builtin_filter.toml`).
- Filter out variables (their warnings are already in the warnings vec).
- Group remaining mentions by `binary`.
- Resolve each group's binary via `PackageDB::provider_of`.
- Attach the full mention list to `DepSource::InferredFromConfig.mentions`.

If a binary from the mention map resolves (via `provider_of`) to a
package whose name matches a declared dep, the implicit entry is not
emitted — the declared entry already covers it. Mentions are only
attached to `DepSource::InferredFromConfig` entries (see §10.1).

### 5.3 `--strict` mode

`ValidatorOptions::strict = true` promotes any `ImplicitFromConfig`
entry with status `Missing` to count as **required-missing** when the
exit code is computed (§11). It does *not* change the entry's `kind`;
the JSON output stays parseable. Strict mode is opt-in because most
ricing setups have a long tail of implicit binaries the user doesn't
care about.

---

## 6. Curated tables

### 6.1 `binary_providers.toml`

Embedded via `include_str!` at compile time, same pattern as
`known_dotfiles.toml`. 40 entries covering the most common ricing
binaries. Best effort, not exhaustive — anything missing requires
`archdots check --deep`.

```toml
# Curated binary → Arch package mapping for archdots Phase 3.
#
# This table is BEST EFFORT, not exhaustive. Target: the ~40 most common
# binaries seen in tiling-WM ricing setups. For anything not listed,
# users can pass `archdots check --deep` to fall back to `pacman -F`.
#
# Schema:
#   name    — binary (executable filename, exactly as it appears in $PATH)
#   package — canonical Arch package providing it
#   source  — "repo" (default) | "aur"

[[binary]]
name = "rofi"
package = "rofi"

[[binary]]
name = "dmenu"
package = "dmenu"

[[binary]]
name = "wofi"
package = "wofi"

[[binary]]
name = "fuzzel"
package = "fuzzel"

[[binary]]
name = "polybar"
package = "polybar"

[[binary]]
name = "waybar"
package = "waybar"

[[binary]]
name = "eww"
package = "eww"
source = "aur"

[[binary]]
name = "picom"
package = "picom"

[[binary]]
name = "dunst"
package = "dunst"

[[binary]]
name = "mako"
package = "mako"

[[binary]]
name = "swaync"
package = "swaync"
source = "aur"

[[binary]]
name = "kitty"
package = "kitty"

[[binary]]
name = "alacritty"
package = "alacritty"

[[binary]]
name = "foot"
package = "foot"

[[binary]]
name = "wezterm"
package = "wezterm"

[[binary]]
name = "st"
package = "st"
source = "aur"

[[binary]]
name = "nvim"
package = "neovim"

[[binary]]
name = "helix"
package = "helix"

[[binary]]
name = "Hyprland"
package = "hyprland"

[[binary]]
name = "hyprpaper"
package = "hyprpaper"

[[binary]]
name = "hypridle"
package = "hypridle"

[[binary]]
name = "hyprlock"
package = "hyprlock"

[[binary]]
name = "swww"
package = "swww"
source = "aur"

[[binary]]
name = "swaybg"
package = "swaybg"

[[binary]]
name = "swayidle"
package = "swayidle"

[[binary]]
name = "swaylock"
package = "swaylock"

[[binary]]
name = "i3lock"
package = "i3lock"

[[binary]]
name = "feh"
package = "feh"

[[binary]]
name = "nitrogen"
package = "nitrogen"

[[binary]]
name = "redshift"
package = "redshift"

[[binary]]
name = "gammastep"
package = "gammastep"

[[binary]]
name = "brightnessctl"
package = "brightnessctl"

# pactl / paplay / parec are provided by `libpulse` on stock pulseaudio
# installs. Pure PipeWire setups (without `pulseaudio` installed) get the
# same binary through `pipewire-pulse`, which declares `provides=libpulse`.
# `pacman -Q libpulse` reports installed in both cases, so this mapping
# holds — but profiles authored on a PipeWire box may declare
# `pipewire-pulse` instead of `libpulse`, in which case the validator will
# flag the curated answer as a false positive. `--deep` is the escape
# hatch.
[[binary]]
name = "pactl"
package = "libpulse"

[[binary]]
name = "pavucontrol"
package = "pavucontrol"

[[binary]]
name = "nm-applet"
package = "network-manager-applet"

[[binary]]
name = "sxhkd"
package = "sxhkd"

[[binary]]
name = "fzf"
package = "fzf"

[[binary]]
name = "starship"
package = "starship"

[[binary]]
name = "fastfetch"
package = "fastfetch"

[[binary]]
name = "bat"
package = "bat"

[[binary]]
name = "eza"
package = "eza"
```

### 6.2 `builtin_filter.toml`

Embedded via `include_str!`. ~60 names. Filtered out at the validator
level **before** binary→package resolution, so the curated table and
`--deep` never waste a lookup on them.

```toml
# Names the validator will not treat as dependency mentions.
#
# Two categories:
#   - shell builtins (cd, export, alias, ...) — never an external binary
#   - "assumed base" — coreutils/util-linux/grep/sed/awk/git/etc., shipped
#     by Arch's base packages, almost never a user-visible dep
#
# This is intentionally small. Anything that is plausibly missing on a
# stripped-down install (e.g. `tmux`, `python`) does NOT belong here.

builtins = [
  "cd", "export", "source", ".", "eval", "unset", "set", "alias",
  "unalias", "local", "readonly", "declare", "typeset", "return",
  "exit", "shift", "trap", "umask", "wait", ":", "[", "[[", "pwd",
  "echo", "printf", "test", "true", "false", "kill", "read", "type",
  "command", "hash", "history", "exec",
]

base = [
  "ls", "cp", "mv", "rm", "cat", "grep", "egrep", "fgrep", "sed", "awk",
  "find", "head", "tail", "wc", "sort", "uniq", "tr", "cut", "xargs",
  "chmod", "chown", "ln", "mkdir", "rmdir", "tar", "gzip", "gunzip",
  "which", "env", "date", "df", "du", "mount", "umount", "ps", "top",
  "pgrep", "pkill", "id", "whoami", "uname", "hostname", "ssh", "scp",
  "rsync", "git", "bash", "sh", "vim", "nano", "less", "more", "tee",
  "basename", "dirname", "realpath", "readlink", "sleep", "yes",
]
```

---

## 7. Public APIs

### `archdots-core::packages`

```rust
/// Abstraction over running a subprocess and capturing its output.
///
/// `PackageDB` depends on this trait so tests can inject canned `pacman`
/// output without spawning a real subprocess. Production code uses
/// `SystemRunner`; tests live in `crates/archdots-core/tests/` and provide
/// their own `MockRunner`.
pub trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, PackageError>;
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Real `std::process::Command`-based runner.
pub struct SystemRunner;
impl CommandRunner for SystemRunner { /* spawns std::process::Command */ }

/// Wrapper over `pacman -Q` / `pacman -Qm` / `pacman -F`.
///
/// All queries are memoised for the lifetime of the instance.
pub struct PackageDB { /* private; interior mutability for caches */ }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkg {
    pub name: String,
    pub version: String,
    pub source: PkgSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PkgSource {
    /// Found in `pacman -Q` but not in `pacman -Qm`.
    Repo,
    /// Found in `pacman -Qm` (foreign / AUR).
    Aur,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AurHelper { Paru, Yay }

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderHit {
    /// Resolved via the embedded curated table.
    Curated { package: String, source: PkgSource },
    /// Resolved via `pacman -F` (only when `deep == true`).
    PacmanFiles { package: String, source: PkgSource },
    /// No mapping known.
    Unknown,
    /// `--deep` was requested but the pacman file db is not synced.
    FilesDbNotSynced,
}

impl PackageDB {
    /// Construct using the real system command runner.
    pub fn new() -> Result<Self, PackageError>;

    /// Construct with an injected runner (used by tests via MockRunner).
    pub fn with_runner(runner: Box<dyn CommandRunner>) -> Self;

    pub fn is_installed(&self, name: &str) -> Result<bool, PackageError>;
    pub fn lookup(&self, name: &str) -> Result<Option<Pkg>, PackageError>;
    pub fn installed(&self) -> Result<&HashMap<String, Pkg>, PackageError>;
    pub fn aur_packages(&self) -> Result<&HashSet<String>, PackageError>;
    pub fn detect_aur_helper(&self) -> Result<Option<AurHelper>, PackageError>;
    pub fn provider_of(&self, binary: &str, deep: bool) -> Result<ProviderHit, PackageError>;
}
```

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PackageError {
    #[error("pacman not found on PATH; archdots check requires an Arch-based system")]
    PacmanMissing,
    #[error("pacman exited with status {code}: {stderr}")]
    PacmanFailed { code: i32, stderr: String },
    #[error("could not parse pacman output: {0}")]
    UnparseableOutput(String),
    #[error("pacman database is locked (/var/lib/pacman/db.lck exists)")]
    DatabaseLocked,
    #[error("failed to spawn subprocess: {0}")]
    Spawn(#[source] std::io::Error),
}
```

### `archdots-core::parsers`

```rust
/// One mention of a binary in a config file. Parsers emit one per
/// occurrence — the validator deduplicates downstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    pub binary: String,
    pub line: u32,
    pub source: MentionSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MentionSource {
    BspwmExec,
    HyprlandExec,        // `exec = X`
    HyprlandExecOnce,    // `exec-once = X`
    HyprlandBind,        // `bind* = MOD, KEY, exec, X`
    I3SwayExec,          // `exec` / `exec_always`
    I3SwayBindsym,
    SxhkdCommand,
    ShellAlias,
    ShellProbe,          // `command -v X`
    ShellInvoke,         // top-level `X arg…`
}

// Note: `MentionSource` and `ParserKind` variants are stable public API
// from v0.3.0. Downstream consumers may match on them; adding variants
// is a breaking change.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParserKind {
    Bspwm,
    Sxhkd,
    Hyprland,
    I3,
    Sway,
    Zsh,
    Bash,
    Shell,
}
// Note: I3 and Sway share the parser implementation internally but are
// distinguished for report granularity. Zsh, Bash, and Shell similarly
// share implementation but have separate variants.

/// Best-effort parser. Never errors; malformed input degrades to a
/// partial (or empty) result.
pub fn parse(kind: ParserKind, contents: &str) -> Vec<Mention>;
```

### `archdots-core::validator`

```rust
pub struct ValidationReport {
    pub profile_name: String,
    pub aur_helper: Option<AurHelper>,
    pub entries: Vec<DepEntry>,
    pub warnings: Vec<ValidationWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepEntry {
    /// Package name (best effort for `ImplicitFromConfig` — may equal the
    /// binary name if no provider is known).
    pub name: String,
    pub kind: DepKind,
    pub status: DepStatus,
    pub source: DepSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DepKind {
    Pacman,
    Aur,
    OptionalPacman,
    /// Inferred from a config file; not listed in profile.dependencies.
    ImplicitFromConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DepStatus {
    Installed,
    Missing,
    /// Binary was mentioned but no provider could be determined.
    UnknownBinary { binary: String },
    /// Could not determine: pacman query failed or db was locked.
    Indeterminate { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepSource {
    DeclaredInProfile,
    InferredFromConfig { mentions: Vec<Mention> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValidationWarning {
    DeclaredButUnused { name: String, kind: DepKind },
    AurDepsButNoHelper { aur_deps: Vec<String> },
    PacmanFilesDbNotSynced,
    UnresolvedVariable { var: String, line: u32, file: PathBuf },
    ConfigUnreadable { path: PathBuf, reason: String },
    PacmanDatabaseLocked,
}

#[derive(Debug, Clone, Copy)]
pub struct ValidatorOptions {
    /// Treat implicit-missing as required-missing for exit-code purposes.
    pub strict: bool,
    /// Fall back to `pacman -F` for binaries not in the curated table.
    pub deep: bool,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ValidatorError {
    #[error(transparent)]
    Package(#[from] PackageError),
    #[error(transparent)]
    Profile(#[from] crate::error::ProfileError),
    #[error("I/O error at {path}: {source}")]
    Io { path: PathBuf, #[source] source: std::io::Error },
}

pub struct Validator<'a> {
    db: &'a PackageDB,
    home: &'a Path,
}

impl<'a> Validator<'a> {
    pub fn new(db: &'a PackageDB, home: &'a Path) -> Self;
    // Note: `infer_kind` is a pure function of the `parsers` module —
    // it does not require an injected `Detector`.

    pub fn validate(
        &self,
        profile: &Profile,
        profile_dir: &Path,
        opts: ValidatorOptions,
    ) -> Result<ValidationReport, ValidatorError>;
}

impl ValidationReport {
    /// See §11 for the canonical mapping.
    pub fn exit_code(&self, strict: bool) -> i32;
    pub fn has_critical_missing(&self, strict: bool) -> bool;
}
```

---

## 8. Edge cases

| # | Case | Handling |
|---|---|---|
| 1 | `pacman` not on PATH (non-Arch host) | `PackageError::PacmanMissing` → exit **3** with explicit "Arch-only" message. |
| 2 | `pacman -Q` exits non-zero | `PackageError::PacmanFailed`. Every dep becomes `DepStatus::Indeterminate`. Exit **3**. |
| 3 | `/var/lib/pacman/db.lck` exists | `PackageError::DatabaseLocked` → `ValidationWarning::PacmanDatabaseLocked`, best-effort report from any cached output. |
| 4 | Config file unreadable (I/O error) | `ValidationWarning::ConfigUnreadable { path, reason }`. Validator continues with remaining files. |
| 5 | Config has syntactic garbage | Parser is lenient — emits whatever it could extract, never errors. |
| 6 | Profile lacks `[dependencies]` | All deps are empty vecs; all discovered binaries become `ImplicitFromConfig`. |
| 7 | Profile declares `kitty`, config mentions `alacritty` | `kitty` → declared+Installed/Missing + warning `DeclaredButUnused`. `alacritty` → `ImplicitFromConfig`. |
| 8 | Optional dep absent from every config | `OptionalPacman+DeclaredInProfile` + low-severity `DeclaredButUnused`. |
| 9 | Binary referenced via `$VAR` | No mention; warning `UnresolvedVariable { var, line, file }`. |
| 10 | AUR helper missing but profile declares AUR deps | Warning `AurDepsButNoHelper { aur_deps }`. Classification still uses `pacman -Qm`. |
| 11 | Curated table miss, `--deep` off | `DepStatus::UnknownBinary`. Hint: "rerun with `--deep`". |
| 12 | `--deep` on, file db not synced | `ProviderHit::FilesDbNotSynced` + `ValidationWarning::PacmanFilesDbNotSynced` (once). Tells user to run `sudo pacman -Fy`. |
| 13 | Same binary mentioned by 50 sxhkd bindings | Parser emits 50 raw mentions; validator groups into one `DepEntry` whose `mentions` holds all 50. CLI elides after 3 by default; `--verbose` shows all. |
| 14 | `$VAR` in profile **path** that doesn't expand | `ProfileError::UnknownEnvVar` propagates as `ValidatorError::Profile` → exit **3**. |
| 15 | Profile entry of `LinkMode::Template` | Not parsed (template not yet rendered). Skipped silently in v0.3. |
| 16 | Same package listed twice in `[dependencies.pacman]` | Deduplicated at validator level; one entry. No warning. |
| 17 | Binary mentioned with absolute path (`/usr/local/bin/foo`) | Parser strips path, uses `Path::file_name` (`foo`). |
| 18 | Mention with shell args (`rofi -show drun`) | Parser takes only the first token after the keyword (`rofi`). |
| 19 | `pacman -F` requires `pacman -Fy` first | Detected via stderr pattern; surfaced as `FilesDbNotSynced`. archdots never invokes `pacman -Fy`. |

---

## 9. Testing policy

### 9.1 No test scaffolding in production code

There is no `ARCHDOTS_RUNNER=mock` env var, no `#[cfg(feature =
"test-mock")]` gate that ships in release builds, and no environment
sniffing inside `PackageDB::new()`. Testability is provided exclusively
through `PackageDB::with_runner(Box<dyn CommandRunner>)`.

### 9.2 MockRunner location

`MockRunner` lives in `crates/archdots-core/tests/` (integration test
tree), **not** in `src/`. That keeps production code free of test
machinery and side-steps any pub-visibility shenanigans.

Sketch:

```rust
// crates/archdots-core/tests/support/mock_runner.rs
pub struct MockRunner {
    pub responses: HashMap<(String, Vec<String>), CommandOutput>,
}
impl CommandRunner for MockRunner { /* match args, return canned output */ }
```

### 9.3 Test layout

| Test file | What it covers | Runner |
|---|---|---|
| `crates/archdots-core/src/parsers.rs#[cfg(test)]` | per-parser unit tests | none (string in, vec out) |
| `crates/archdots-core/tests/packages.rs` | PackageDB caching, classification, AUR detection | MockRunner |
| `crates/archdots-core/tests/validator.rs` | end-to-end validator pipeline | MockRunner + tempdir |
| `crates/archdots/tests/cli_phase3.rs` | minimal CLI integration: `--help`, missing profile, JSON shape | no pacman; profile fixtures + small fakes |
| `crates/archdots/tests/cli_phase3_real_pacman.rs` (`#[ignore]`) | one smoke test that drives real `pacman -Q` end-to-end | real pacman, only on Arch hosts |

The `#[ignore]`d test is the same pattern Phase 2 uses for the
cross-process lock test: CI never sees it, but `cargo test --
--include-ignored` on a developer's Arch box exercises it.

### 9.4 Coverage target

≥ 70% on `core` (the project-wide bar). Parsers and validator are the
heaviest contributors. PackageDB's caching paths are individually
covered by `MockRunner`-driven assertions.

---

## 10. JSON output — stability policy

`archdots check --json` emits a **stable JSON shape** from v0.3.0
onward. The contract:

1. **Versioning is per-output, not per-crate.** The top-level field
   `schema_version: 1` versions the JSON shape independently of the
   crate's semver. archdots can ship 0.4.0, 0.5.0, 1.0.0 without
   touching `schema_version`. A breaking JSON change requires bumping
   `schema_version` (to `2`) and is its own dedicated PR + CHANGELOG
   entry.
2. **Additive changes are non-breaking.** Adding a new optional field
   to an object, adding a new enum variant tagged as `unknown_to_v1`,
   or extending a vec, all stay on `schema_version: 1`.
3. **Breaking changes require a bump.** Renaming a field, removing a
   field, changing a type, or changing the semantics of an existing
   variant all bump `schema_version`.
4. **CI scripts can rely on the shape.** Downstream tooling may pin
   to `schema_version == 1` and assume the documented field set is
   present.

### 10.1 Shape

```jsonc
{
  "schema_version": 1,
  "profile": "hyprland-rice",
  "aur_helper": "paru",                          // or null
  "entries": [
    {
      "name": "hyprland",
      "kind": "pacman",                           // pacman | aur | optional_pacman | implicit_from_config
      "status": "installed",                      // installed | missing | unknown_binary | indeterminate
      "source": "declared"                        // declared | inferred
    },
    {
      "name": "brightnessctl",
      "kind": "implicit_from_config",
      "status": "missing",
      "source": "inferred",
      "mentions": [
        {
          "binary": "brightnessctl",
          "line": 42,
          "kind": "hyprland_exec",
          "file": ".config/hypr/hyprland.conf"
        }
      ]
    }
  ],
  "warnings": [
    { "type": "declared_but_unused", "name": "alacritty", "kind": "pacman" }
  ],
  "exit_code": 1
}
```

Field encoding details:

- `status` of `unknown_binary` carries `binary: "<name>"`.
- `status` of `indeterminate` carries `reason: "<string>"`.
- `source` of `declared` has no extra fields; `inferred` carries
  `mentions: [...]`.
- `warnings[].type` is the snake_case variant name. Each warning
  variant has its own field set, documented per variant.

---

## 11. Exit codes — canonical table

| Code | Meaning |
|---|---|
| **0** | All declared required deps installed; no implicit-missing matters for current mode. |
| **1** | At least one declared required (`pacman` or `aur`) dep is missing; **OR** `--strict` is on and at least one implicit dep is missing. |
| **2** | At least one optional dep is missing **OR** at least one implicit dep is missing (and `--strict` is off); no required missing. |
| **3** | Indeterminate: pacman not installed, pacman failed, db locked, profile broken, or any `DepStatus::Indeterminate` in the report. |

### Precedence

`3 > 1 > 2 > 0`. If multiple categories apply, the higher code wins.

### Examples

| Scenario | Exit |
|---|---|
| Everything installed, some `DeclaredButUnused` warnings | 0 |
| Required pacman dep missing | 1 |
| Required AUR dep missing | 1 |
| Implicit `brightnessctl` missing, `--strict` off | 2 |
| Implicit `brightnessctl` missing, `--strict` on | 1 |
| Only an optional dep missing | 2 |
| Required dep AND optional dep missing | 1 (1 wins over 2) |
| pacman db locked, mixed installed/missing | 3 (3 wins over everything) |
| Running on Ubuntu (`pacman` not found) | 3 |

---

## 12. Out of scope for Phase 3

- Running `paru -S` / `yay -S` automatically. archdots only **reports**.
- Detecting which `bspc rule` class names map to which packages.
- Resolving `$VAR` from the user's shell environment.
- Parsing tmux / screen / awesome configs.
- Multi-distro support (see §2 and ADR-003).
- On-disk caching of `pacman -Q` across invocations.
- Auto-running `pacman -Fy` (requires root; user's responsibility).
- Parsing `[hooks].pre_apply` / `post_apply` scripts.
- TUI rendering of the report (Phase 4).
