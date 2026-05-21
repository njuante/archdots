# Phase 4 — TUI con Ratatui (Design)

Status: **approved** — implementation target for Phase 4.

This document specifies the design of `archdots`'s interactive terminal UI.
The TUI **replaces day-to-day use** of `apply` / `rollback` / `check` / `diff`
/ `snapshots list`. The CLI subcommands remain canonical for scripts and CI.

The TUI does **not** duplicate business logic. Every user action funnels into
`archdots-core`. The CLI and TUI are two presenters over the same engine.

The five load-bearing decisions are also recorded as ADR-004 in
[`ARCHITECTURE.md`](./ARCHITECTURE.md).

---

## 1. Principles and invariants

P1. **Core stays sync (ADR-002).** The binary may spawn OS threads; the
library does not import tokio, async-std, or smol.

P2. **No duplicated logic.** Every action ends in a core call. Render code is
per-presenter (ratatui frames in the TUI, stdout `println!` in `cmd/*`). The
validator, linker, snapshot manager, journal, parsers, packages module are
the single source of truth.

P3. **Render is cheap; tasks are slow.** Render must never block on I/O.
Anything that can take > 50 ms goes into a background thread.

P4. **No `unwrap` / `expect` / `panic!` in TUI code.** Any panic in a worker
thread is caught and surfaced as an error modal — it never crashes the loop.

P5. **The TUI is recoverable on quit.** If the user force-quits during an
apply, the existing orphan-recovery flow in `Linker` (ADR-002) cleans up on
next launch. The TUI itself does no extra book-keeping.

P6. **`tracing` output never goes to the terminal in TUI mode.** It is
redirected to `$XDG_STATE_HOME/archdots/tui.log`. Stray `info!` lines must
not corrupt the frame.

P7. **No on-disk UI state.** Each `archdots tui` boots into ProfilesView.
Persisted preferences are explicitly out of scope.

P8. **Worker threads receive everything by value.** A spawned task owns its
clones of paths and arguments; it never holds references into `App`. User
navigation during a running task is invisible to the worker — by design.

---

## 2. Module layout

```
crates/archdots/src/
├── main.rs                              # adds `Tui` subcommand
├── cmd/
│   ├── ...
│   └── tui.rs                           # thin entrypoint: cmd::tui::run()
└── tui/                                 # NEW
    ├── mod.rs                           # pub fn run_loop(); raw-mode lifecycle
    ├── app.rs                           # struct App + handle_event + render
    ├── action.rs                        # enum Action (cross-view messages)
    ├── events.rs                        # enum Event + EventLoop
    ├── tasks.rs                         # BackgroundKind, TaskMessage, spawn
    ├── theme.rs                         # Theme + NO_COLOR / LANG detection
    ├── ui/
    │   ├── mod.rs
    │   ├── layout.rs                    # chunks(): top bar / body / status
    │   ├── status_bar.rs                # spinner + message
    │   ├── modal.rs                     # Confirm / Error / ScrollableInfo
    │   └── widgets.rs                   # ListWithDetail, SearchInput, Spinner
    └── views/
        ├── mod.rs                       # trait View + ViewKind
        ├── profiles.rs                  # ProfilesView
        ├── snapshots.rs                 # SnapshotsView
        ├── deps.rs                      # DepsView
        ├── diff.rs                      # DiffView
        └── help.rs                      # HelpView (overlay-only)
```

Rationale: `tui/` is a flat tree under the binary crate. No `archdots-tui`
sub-crate — the TUI has no value as a reusable library and splitting it out
would just add lifetime/visibility noise. `views/` mirrors how `cmd/` is
organised.

---

## 3. Concurrency model — sync core, threads in the TUI

### 3.1 Operations and their latency

Estimated upper bounds on a typical Arch laptop:

| Operation | Dominated by | Wall-clock (typical / worst) |
|---|---|---|
| `Profile::load_from_file` | TOML parse | < 5 ms / 20 ms |
| `Linker::plan` | per-target `lstat` | < 50 ms / 200 ms |
| `Linker::apply` (24 links) | tar + gzip + `fsync` × N | **2 – 8 s** / 15 s |
| `Linker::rollback` / `rollback_to_snapshot` | tar read + atomic restore | 2 – 8 s / 15 s |
| `SnapshotManager::list` | dir read + JSON parse | 50 – 200 ms / 1 s |
| `SnapshotManager::prune` (10 entries) | `unlink` | < 100 ms / 500 ms |
| `Validator::validate` (no `--deep`) | `pacman -Q` spawn | 200 – 500 ms / 1.5 s |
| `Validator::validate` (`--deep`) | `pacman -F` × unknown-bins | **5 – 30 s** / 60 s |
| `similar::TextDiff` on a 200-line file | CPU | < 20 ms / 100 ms |

The two operations that routinely exceed 50 ms are **apply / rollback** and
**deep check**. All long ops are routed through background tasks for
consistency.

### 3.2 Why threads, not tokio

| Concern | Threads + mpsc | Tokio |
|---|---|---|
| Function-color contagion | None — `core` stays sync | Async leaks into every caller |
| Stack memory | ~2 MB × max 1 active task | Tasks cheaper, but we only run 0–1 |
| Crash isolation | `catch_unwind` → channel msg | Same effort with `JoinHandle::is_panicked` |
| Stack traces on panic | Native Rust backtrace | Async backtrace, harder to read |
| Integration with crossterm | `event::poll(Duration::from_millis(50))` + `try_recv` | Need `crossterm-tokio` or manual bridging |
| New deps | 0 | `tokio`, `tokio-util` |
| ADR-002 compliance | Yes | Forces "tokio in binary" footnote |

We expect **at most one concurrent background task at any time**. Threads are
trivially affordable. Tokio's selling point (millions of cheap tasks, async
I/O composition) is irrelevant for an interactive single-user FS-bound
transactional workflow.

**Decision: threads + mpsc.** ADR-002 stands.

### 3.3 Multiple sequential operations

The user may "apply, then check, then rollback" in one session. Model:

```rust
enum BackgroundState {
    Idle,
    Running {
        id: TaskId,
        kind: BackgroundKind,
        started: std::time::Instant,
    },
}
```

Rules:

- Only `Idle` accepts a new task. Triggering an action while `Running` →
  `Action::SetStatus("operation in progress: <kind> (<elapsed>s)")`.
- The background thread sends exactly one `TaskMessage::Completed { id, kind,
  result }` on the global channel. When received, `App` transitions to
  `Idle` and the result is rendered (modal for errors, status bar for
  success). The handler is responsible for follow-up state refresh
  (re-reading the journal so SnapshotsView shows the new snapshot, etc.).
- We do **not** queue tasks. Queueing adds UX ambiguity ("what's running?",
  "can I cancel item 3?") for zero practical value in v0.4.

### 3.4 Cancellation policy

**No cancellation in v0.4.** Justification:

1. `Linker::apply` is already transactional. If it crashes or is killed
   mid-flight, the journal contains an `InProgress` orphan and the next
   launch refuses to run with "run `archdots recover`". OS-level cancellation
   (force kill) is the rollback story.
2. Soft cancellation (an `AtomicBool` checked between symlinks) would
   require core API changes for a few seconds of saved time. Bad ROI.
3. The user's mental model is "I started this, it's atomic, I wait" — same
   as `cmd::apply::run`.

What `Ctrl+C` does:

- During `Idle`: opens a `QuitWithRunningTask`-style confirm modal (or quits
  silently if no work is pending).
- During `Running`: opens an inline status hint "press `Ctrl+C` again within
  3 s to force-quit (will leave an orphan)". Two strikes → `std::process::exit`.
  The orphan is recovered on next launch via `Journal::orphaned_in_progress`.

### 3.5 The mpsc channel

One global `mpsc::Sender<TaskMessage>` cloned into each worker. `App` owns
the `Receiver` and drains it once per event-loop iteration with `try_recv()`
in a tight loop until `Err(Empty)`. Tasks are identified by an opaque
`TaskId(u64)` (a monotonic counter) so a stale completion from a
force-quit-and-restart scenario is recognisable.

Worker shape:

```rust
// pseudo
thread::Builder::new().name("apply-task").spawn(move || {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let snaps = SnapshotManager::open(&paths.data_home)?;
        let journal = Journal::open(&paths.state_home)?;
        let linker = Linker::new(&snaps, &journal);
        linker.apply(plan, opts)
    }));
    let _ = tx.send(TaskMessage::Completed { id, kind, result: normalize(result) });
});
```

Panics and `Result::Err` are normalised into the same `TaskResult` shape
before being sent.

### 3.6 Worker isolation (no references into `App`)

A spawned worker owns clones of:

- `AppPaths` (`home`, `state_home`, `data_home`, `profiles_dir` — all
  `PathBuf`s).
- Profile name(s) (`String`).
- Operation options (`ApplyOptions`, `ValidatorOptions`, `SnapshotId`).

The worker never accesses `App` directly. Consequences:

- User navigation during the task does not affect the task. If the user
  moves to SnapshotsView while an Apply runs, the worker keeps running with
  the original profile name; the result comes back regardless.
- `App` is never borrowed across thread boundaries, so we don't need
  `Arc<Mutex<App>>` or any locking.
- The worker is `Send + 'static` by construction.

---

## 4. State management — per-view structs + Action enum

### 4.1 Decision

**Per-view structs composed by `App`; cross-view coordination via an `Action`
enum returned from `handle_event`.** No global reducer.

Rationale (vs alternatives):

| Pattern | Pros | Cons |
|---|---|---|
| Elm-like reducer | Pure transitions, replayable | Whole-state clones, big match, awkward side-effects |
| Free-form mutable `App` | Idiomatic, cheap | Cross-view dependencies tangle past 3 views |
| **Per-view + Action** | Local mutation, testable per-view, explicit cross-view via Action | Slightly more boilerplate than free-form |

The reducer pattern is good for UIs that diff immutable values (web
frontends). A terminal UI built on ratatui isn't that — the renderer takes
`&App` and `&mut Frame`, and there's no benefit to immutable state because
we never replay.

The free-form approach scales badly past three views: shared state (e.g.
"selected profile in ProfilesView seeds DepsView") gets tangled. The
Per-view + Action pattern is strictly more testable: a unit test pushes a
sequence of `Event`s into a view and asserts on the `Action`s returned.

### 4.2 Action enum

```rust
pub enum Action {
    None,
    SwitchView(ViewKind),
    SelectProfileForDeps(String),
    SelectProfileForDiff(String),
    SpawnTask(BackgroundKind),
    SetStatus(StatusMessage),
    OpenModal(ui::Modal),
    CloseModal,
    Quit,
}
```

Each view's `handle_event(&mut self, ev, ctx) -> Action` reports outward.
`App::dispatch` is the only place that handles `Action` — it owns the view
enum, the background state, and the modal stack.

### 4.3 Testing surface

What we test, without rendering:

- **Per-view**: each keystroke produces the expected `Action` and the
  expected internal mutation (cursor position, search filter, etc.).
- **App-level**: dispatching `Action::SwitchView(ViewKind::Deps)` transitions
  correctly and forwards profile context.
- **Tasks**: `TaskMessage::Completed { result: Err(_) }` opens an error
  modal; `Ok(_)` updates state and clears the spinner.

What we **don't** test:

- Pixel-accurate rendering. Validated by manual dogfooding.
- Style / colour palette. Compiles, runs, looks right.

---

## 5. Public APIs (Rust signatures, no bodies)

### 5.1 `cmd/tui.rs`

```rust
pub fn run() -> anyhow::Result<()>;
// Entry point invoked from main::Commands::Tui. Sets up file-based logging,
// enters raw mode + alt screen, runs the event loop, restores the terminal
// on every exit path (panic-safe via Drop guard).
```

### 5.2 `tui::app`

```rust
pub struct App {
    profiles: views::ProfilesView,
    snapshots: views::SnapshotsView,
    deps: views::DepsView,
    diff: views::DiffView,

    active: ViewKind,
    modal: Option<ui::Modal>,
    status_msg: ui::StatusMessage,
    background: BackgroundState,

    paths: AppPaths,
    theme: theme::Theme,
    next_task_id: TaskId,
    tx: std::sync::mpsc::Sender<TaskMessage>,
    rx: std::sync::mpsc::Receiver<TaskMessage>,
    should_quit: bool,
    dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind { Profiles, Snapshots, Deps, Diff }

pub enum BackgroundState {
    Idle,
    Running { id: TaskId, kind: BackgroundKind, started: std::time::Instant },
}

impl App {
    pub fn new(paths: AppPaths, theme: theme::Theme) -> anyhow::Result<Self>;

    /// Drains pending task completions, dispatches one input event, possibly
    /// transitions views/state. Returns whether a redraw is needed.
    pub fn step(&mut self, ev: Event) -> anyhow::Result<bool>;

    pub fn render(&self, frame: &mut ratatui::Frame<'_>);

    pub fn should_quit(&self) -> bool;
    pub fn is_animating(&self) -> bool;     // true while a task is running

    fn dispatch(&mut self, action: Action) -> anyhow::Result<()>;
    fn drain_task_messages(&mut self) -> anyhow::Result<()>;
    fn spawn_task(&mut self, kind: BackgroundKind);
    fn execute_confirm(&mut self, kind: ConfirmKind) -> Action;
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub home: PathBuf,
    pub profiles_dir: PathBuf,
    pub data_home: PathBuf,
    pub state_home: PathBuf,
}
```

### 5.3 `tui::action`

```rust
pub enum Action {
    None,
    SwitchView(ViewKind),
    SelectProfileForDeps(String),
    SelectProfileForDiff(String),
    SpawnTask(BackgroundKind),
    SetStatus(StatusMessage),
    OpenModal(ui::Modal),
    CloseModal,
    Quit,
}
```

### 5.4 `tui::events`

```rust
#[derive(Debug)]
pub enum Event {
    Tick,                                     // 20 Hz heartbeat (50 ms)
    Input(crossterm::event::Event),           // key / mouse / resize
}

/// Polls crossterm + ticks at 20 Hz. Task completions are read by `App`
/// directly via the mpsc Receiver, so EventLoop only emits Tick and Input.
pub struct EventLoop { /* private */ }

impl EventLoop {
    pub fn new() -> Self;                     // tick_rate is hardcoded 50 ms
    pub fn next(&mut self) -> anyhow::Result<Event>;
}
```

### 5.5 `tui::tasks`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(u64);

#[derive(Debug, Clone)]
pub enum BackgroundKind {
    Apply { profile: String },
    Rollback { profile: String },
    RollbackToSnapshot { snapshot_id: archdots_core::snapshot::SnapshotId, profile: String },
    Check { profile: String, deep: bool },
    RefreshSnapshots,
    PruneSnapshot { id: archdots_core::snapshot::SnapshotId },
}

#[derive(Debug)]
pub enum TaskMessage {
    Completed { id: TaskId, kind: BackgroundKind, result: TaskResult },
}

#[derive(Debug)]
pub enum TaskResult {
    Apply(Result<archdots_core::linker::ApplyReport, anyhow::Error>),
    Rollback(Result<archdots_core::linker::ApplyReport, anyhow::Error>),
    Check(Result<archdots_core::validator::ValidationReport, anyhow::Error>),
    SnapshotList(Result<Vec<archdots_core::snapshot::SnapshotSummary>, anyhow::Error>),
    Prune(Result<archdots_core::snapshot::PruneReport, anyhow::Error>),
}

/// Spawn `kind` on a fresh OS thread. Catches panics; the result (or a
/// normalised panic message) is always sent on `tx`. All arguments needed
/// to execute are cloned into the closure — no references to `App`.
pub fn spawn(
    id: TaskId,
    kind: BackgroundKind,
    paths: AppPaths,
    tx: std::sync::mpsc::Sender<TaskMessage>,
);
```

### 5.6 `tui::ui::modal`

```rust
#[derive(Debug, Clone)]
pub enum ConfirmKind {
    ApplyProfile(String),
    RollbackProfile(String),
    RollbackToSnapshot(archdots_core::snapshot::SnapshotId),
    DeleteProfile(String),
    PruneSnapshot(archdots_core::snapshot::SnapshotId),
    QuitWithRunningTask,
}

#[derive(Debug, Clone)]
pub enum Modal {
    Confirm { prompt: String, kind: ConfirmKind },
    Error { title: String, body: String },
    ScrollableInfo { title: String, body: String, scroll: u16 },
}

impl Modal {
    pub fn render(&self, frame: &mut ratatui::Frame<'_>, theme: &theme::Theme);
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> ModalOutcome;
}

#[derive(Debug)]
pub enum ModalOutcome {
    KeepOpen,
    Close,
    Confirm,            // user accepted; App reads `kind` from the modal
    Copy(String),       // body text the user asked to copy
}
```

`Modal: Clone` is preserved. `Error` always offers `[c] copy` (the body is
the copy target); on clipboard failure the modal renders a one-line hint
"clipboard unavailable; use mouse selection to copy" and ignores the `c`
key. No `copyable: bool` flag is needed.

`App::execute_confirm` is a single `match` over `ConfirmKind` returning the
appropriate `Action`:

```rust
fn execute_confirm(&mut self, kind: ConfirmKind) -> Action {
    match kind {
        ConfirmKind::ApplyProfile(name) =>
            Action::SpawnTask(BackgroundKind::Apply { profile: name }),
        ConfirmKind::RollbackProfile(name) =>
            Action::SpawnTask(BackgroundKind::Rollback { profile: name }),
        ConfirmKind::RollbackToSnapshot(id) => {
            let profile = /* read from snapshot manifest */;
            Action::SpawnTask(BackgroundKind::RollbackToSnapshot { snapshot_id: id, profile })
        }
        ConfirmKind::DeleteProfile(name)   => /* fs::remove_file + refresh */,
        ConfirmKind::PruneSnapshot(id)     =>
            Action::SpawnTask(BackgroundKind::PruneSnapshot { id }),
        ConfirmKind::QuitWithRunningTask   => Action::Quit,
    }
}
```

### 5.7 `tui::theme`

```rust
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub accent: ratatui::style::Color,
    pub ok: ratatui::style::Color,
    pub warn: ratatui::style::Color,
    pub err: ratatui::style::Color,
    pub muted: ratatui::style::Color,
    pub border: ratatui::style::Color,
    pub use_unicode: bool,            // false on LANG=C / POSIX
    pub use_color: bool,              // false on NO_COLOR
}

impl Theme {
    pub fn detect() -> Self;
}
```

### 5.8 `tui::views`

```rust
pub trait View {
    fn render(&self, frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, theme: &Theme);
    fn handle_event(&mut self, ev: &crossterm::event::Event, ctx: &ViewCtx) -> Action;
    fn on_focus(&mut self, ctx: &ViewCtx);
    fn footer_hints(&self) -> Vec<(&'static str, &'static str)>;
}

pub struct ViewCtx<'a> {
    pub paths: &'a AppPaths,
    pub theme: &'a Theme,
    pub busy: bool,                   // true while a background task runs
}

// ProfilesView -------------------------------------------------------------

pub struct ProfilesView {
    profiles: Vec<ProfileItem>,
    cursor: usize,
    search: Option<SearchState>,      // Some when `/` active
    error: Option<String>,
}

struct ProfileItem {
    name: String,
    summary: Option<ProfileSummary>,  // lazy: loaded on focus / cursor move
}

struct ProfileSummary {
    file_count: usize,
    pacman_deps: usize,
    aur_deps: usize,
    last_applied: Option<time::OffsetDateTime>,
    description: Option<String>,
}

impl ProfilesView {
    pub fn new(paths: &AppPaths) -> anyhow::Result<Self>;
    pub fn refresh(&mut self, paths: &AppPaths) -> anyhow::Result<()>;
    pub fn selected_name(&self) -> Option<&str>;
}

// SnapshotsView ------------------------------------------------------------

pub struct SnapshotsView {
    summaries: Vec<archdots_core::snapshot::SnapshotSummary>,
    cursor: usize,
    search: Option<SearchState>,
    detail: Option<archdots_core::snapshot::Snapshot>,
    error: Option<String>,
}

impl SnapshotsView {
    pub fn new(paths: &AppPaths) -> anyhow::Result<Self>;
    pub fn refresh(&mut self, paths: &AppPaths) -> anyhow::Result<()>;
    pub fn selected_id(&self) -> Option<&archdots_core::snapshot::SnapshotId>;
}

// DepsView -----------------------------------------------------------------

pub struct DepsView {
    profile: Option<String>,
    report: Option<archdots_core::validator::ValidationReport>,
    cursor: usize,
    section: DepsSection,
    deep_used: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepsSection { Required, RequiredAur, Optional, Implicit }

impl DepsView {
    pub fn empty() -> Self;
    pub fn set_profile(&mut self, name: String);      // triggers a Check task
    pub fn apply_report(&mut self, report: archdots_core::validator::ValidationReport);
}

// DiffView -----------------------------------------------------------------

pub struct DiffView {
    profile: Option<String>,
    items: Vec<DiffItem>,
    cursor: usize,
    detail: DiffDetail,
    scroll: u16,
}

struct DiffItem {
    target: PathBuf,
    source: PathBuf,
    kind: DiffItemKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffItemKind { Owned, Missing, ReplaceFile, ReplaceSymlink, Conflict }

#[derive(Debug, Clone)]
pub enum DiffDetail {
    RenderedDiff(String),         // pre-rendered for display
    Identical,
    ExternalSymlink { points_to: PathBuf },
    Missing,
}

impl DiffView {
    pub fn empty() -> Self;
    pub fn set_profile(&mut self, name: String, paths: &AppPaths) -> anyhow::Result<()>;
}

// HelpView -----------------------------------------------------------------

pub struct HelpView; // stateless overlay

impl HelpView {
    pub fn render(frame: &mut ratatui::Frame<'_>, active: ViewKind, theme: &Theme);
}
```

---

## 6. Event flow diagram

```
                ┌──────────────────────────────────────────────┐
                │ run_loop                                     │
                │   Terminal::draw(|f| app.render(f))          │
                │                                              │
                │   loop {                                     │
                │     app.drain_task_messages()  ────────────┐ │
                │     event = event_loop.next()              │ │
                │     dirty  = app.step(event)               │ │
                │     if dirty || app.is_animating()         │ │
                │        terminal.draw(|f| app.render(f))    │ │
                │     if app.should_quit() { break }         │ │
                │   }                                        │ │
                └────────────────────────────────────────────┼─┘
                                                             │
                ┌──────────────────────┐                     │
                │ tasks::spawn(...)    │  worker thread      │
                │   linker.apply(...)  │  (panics caught)    │
                │   tx.send(Completed) │ ────────────────────┘
                └──────────────────────┘                     ▲
                                                             │
                  ┌──────────────────────────────────────────┴──┐
                  │ App::step(Event)                            │
                  │ ┌─────────────────────────────────────────┐ │
                  │ │ match event                             │ │
                  │ │   Tick               → maybe redraw     │ │
                  │ │   Input(Resize)      → force redraw     │ │
                  │ │   Input(KeyEvent)    → modal? view?     │ │
                  │ │                       → Action          │ │
                  │ └─────────────────┬───────────────────────┘ │
                  │                   ▼                         │
                  │ ┌─────────────────────────────────────────┐ │
                  │ │ App::dispatch(action)                   │ │
                  │ │   SwitchView      → self.active = ...   │ │
                  │ │   SpawnTask       → tasks::spawn(...)   │ │
                  │ │   OpenModal       → self.modal = ...    │ │
                  │ │   SelectFor{Deps,Diff} → forward + sw   │ │
                  │ └─────────────────────────────────────────┘ │
                  └─────────────────────────────────────────────┘
```

Tick rate: **20 Hz hardcoded** (`tick_rate = Duration::from_millis(50)`).
Sufficient for the spinner, cheaper than 60 Hz, inaudible on battery. Not
configurable.

Redraw policy: only redraw when `dirty` (set by any state change) or
`is_animating` (background task → spinner needs frames). Otherwise the
main loop sleeps inside `event_loop.next()` (which is
`crossterm::event::poll(50ms)`).

---

## 7. View layouts

### 7.1 Common chrome

```
┌────────────────────────────────────────────────────────────────────┐
│ archdots 0.4.0 │ [1]Profiles [2]Snapshots [3]Deps [4]Diff   [?]Help │  ← top bar
├────────────────────────────────────────────────────────────────────┤
│                                                                    │
│                              <body>                                │
│                                                                    │
├────────────────────────────────────────────────────────────────────┤
│ 3 profiles • laptop selected • press ? for help                    │  ← status bar
└────────────────────────────────────────────────────────────────────┘
```

The active tab is highlighted with `theme.accent`. When
`BackgroundState::Running`, the status bar shows
`⠋ Applying laptop... (4s) • press ? for help` (spinner left-aligned).

### 7.2 ProfilesView

```
┌────────────────────────────────────────────────────────────────────┐
│ archdots 0.4.0 │ [1]Profiles [2]Snapshots [3]Deps [4]Diff   [?]Help │
├──────────────────────┬─────────────────────────────────────────────┤
│ Profiles             │ laptop                                      │
│ ▶ laptop             │ ─────────                                   │
│   desktop            │ Description : tiling rice for ThinkPad      │
│   minimal            │ Author      : njuante                       │
│                      │ Tags        : hyprland, work                │
│                      │ Files       : 24                            │
│                      │ Pacman deps : 18 (15 installed, 3 missing)  │
│                      │ AUR deps    : 3 (paru detected)             │
│                      │ Last applied: 2026-05-18 14:32              │
│ /typeable_filter     │                                             │
│                      │ [Enter] apply  [d] diff  [r] rollback       │
│                      │ [c] check      [x] delete                   │
├──────────────────────┴─────────────────────────────────────────────┤
│ 3 profiles • laptop selected • press ? for help                    │
└────────────────────────────────────────────────────────────────────┘
```

Keys (in addition to globals): `j`/`k` or arrows move the cursor; `Enter`
opens `Modal::Confirm { kind: ConfirmKind::ApplyProfile(name) }`; `d`
switches to DiffView with this profile; `r` opens `ConfirmKind::RollbackProfile`;
`c` switches to DepsView (triggers a `Check` task); `x` opens
`ConfirmKind::DeleteProfile`; `/` enters search mode (`Esc` cancels, `Enter`
commits).

Search ranking uses `fuzzy_matcher::skim::SkimMatcherV2`.

### 7.3 SnapshotsView

```
┌────────────────────────────────────────────────────────────────────┐
│ archdots 0.4.0 │ [1]Profiles [2]Snapshots [3]Deps [4]Diff   [?]Help │
├──────────────────────────────────┬─────────────────────────────────┤
│ id        created      profile   │ 01HVAC9PQ2RT0002                │
│ ▶ 01HVA…  18 14:32     laptop    │ ──────────                      │
│   01HVA…  17 21:11     laptop    │ Created : 2026-05-18T14:32:11Z  │
│   01HV9…  15 09:50     desktop   │ Profile : laptop                │
│   01HV8…  10 18:02     minimal   │ Trigger : pre_apply             │
│                                  │ Targets : 24                    │
│                                  │ Size    : 184.2 KiB             │
│                                  │ Host    : archbox (u@/home/u)   │
│                                  │                                 │
│                                  │ [r] restore  [x] prune  [i] info│
├──────────────────────────────────┴─────────────────────────────────┤
│ 4 snapshots • press ? for help                                     │
└────────────────────────────────────────────────────────────────────┘
```

Keys: `r` opens `ConfirmKind::RollbackToSnapshot(id)` — uses the new
`Linker::rollback_to_snapshot` API (§13.1); `x` opens
`ConfirmKind::PruneSnapshot(id)`; `i` opens `Modal::ScrollableInfo` with the
full manifest; `/` search by id-prefix / profile.

### 7.4 DepsView

```
┌────────────────────────────────────────────────────────────────────┐
│ archdots 0.4.0 │ [1]Profiles [2]Snapshots [3]Deps [4]Diff   [?]Help │
├────────────────────────────────────────────────────────────────────┤
│ Profile: laptop                         AUR helper: paru            │
├────────────────────────────────────────────────────────────────────┤
│ Required (pacman)                                                  │
│   ✓ hyprland                                                       │
│   ✓ waybar                                                         │
│   ✗ brightnessctl       ← cursor                                    │
│ Required (AUR)                                                     │
│   ✓ hyprshot                                                       │
│ Optional (pacman)                                                  │
│   ? grim                                                           │
│ Implicit (mentioned in configs)                                    │
│   ✗ swww                .config/hypr/hyprland.conf:42               │
│   ! pavucontrol         .config/hypr/hyprland.conf:88 (+1 more)    │
├────────────────────────────────────────────────────────────────────┤
│ 4 sections • Heuristic, review manually • press ? for help          │
└────────────────────────────────────────────────────────────────────┘
```

Keys: `j`/`k` move within a section; `J`/`K` jump between sections; `c`
copy install command for the selected missing entry (uses detected AUR
helper for AUR entries, `sudo pacman -S` for repo); `R` re-run check; `D`
toggle `--deep`.

Status icons: ✓ green, ✗ red, ? yellow, ! cyan — same convention as
`cmd::check`. ASCII fallback under NO_COLOR / `Theme::use_unicode == false`.

### 7.5 DiffView

```
┌────────────────────────────────────────────────────────────────────┐
│ archdots 0.4.0 │ [1]Profiles [2]Snapshots [3]Deps [4]Diff   [?]Help │
├──────────────────────┬─────────────────────────────────────────────┤
│ laptop               │ .config/hypr/hyprland.conf                  │
│ ▶ hyprland.conf  ⟂   │ ───────                                     │
│   waybar/config  ✓   │ --- source                                  │
│   foot.ini       ⊕   │ +++ target                                  │
│   .zshrc         ⚠   │  monitor=,preferred,auto,1                  │
│                      │  exec-once = waybar                         │
│                      │ -exec-once = hyprshot                       │
│                      │ +exec-once = grimblast                      │
│                      │  bind = SUPER, T, exec, kitty               │
│                      │ -bind = SUPER, R, exec, wofi                │
│                      │ +bind = SUPER, R, exec, rofi                │
│                      │ ↓ scroll                                    │
├──────────────────────┴─────────────────────────────────────────────┤
│ ⊕ create  ✓ owned  ⟂ replace-file  ⚠ conflict                       │
│ [a] apply this profile  [q] back                                    │
└────────────────────────────────────────────────────────────────────┘
```

Keys: `j`/`k` move the file cursor; `J`/`K` scroll the diff body; `a`
opens `ConfirmKind::ApplyProfile`; `q` returns to ProfilesView.

The unified-diff rendering is produced by `similar::TextDiff::from_lines`,
the same crate that powers `cmd::diff::run`. The string is computed once
when the file cursor moves and cached in `DiffDetail::RenderedDiff`.

### 7.6 Confirm modal

```
              ┌────────────────────────────────────────────┐
              │ Apply 'laptop'?                            │
              │                                            │
              │ This will create or replace 22 symlink(s). │
              │ Snapshot will be taken first.              │
              │                                            │
              │   [Enter] Yes   [Esc] No                   │
              └────────────────────────────────────────────┘
```

Centered overlay (`Clear` widget + bordered Block). When a modal is open,
input goes to the modal handler exclusively; the underlying view receives
no events.

---

## 8. Long operation — worked example (Apply)

Step by step, what happens when the user presses `Enter` on `laptop` in
ProfilesView:

1. **t = 0 ms** — `ProfilesView::handle_event` returns
   `Action::OpenModal(Modal::Confirm { prompt, kind: ConfirmKind::ApplyProfile("laptop") })`.
   `App::dispatch` sets `self.modal = Some(modal)` and marks dirty.

2. **t = 20 ms** — Next frame paints the modal over the ProfilesView frame.

3. **t ≈ 1 s** (user presses Enter) — Modal handler returns
   `ModalOutcome::Confirm`. `App` reads the modal's `ConfirmKind`, calls
   `execute_confirm` → `Action::SpawnTask(BackgroundKind::Apply { profile: "laptop" })`.
   Modal is closed.

4. **`App::spawn_task`** transitions
   `background = Running { id: 7, kind: Apply { .. }, started: now }` and
   calls `tasks::spawn(7, kind, paths.clone(), tx.clone())`. The thread
   starts; the main loop is unblocked.

5. **t ≈ 1.05 s** — Worker thread runs:

   ```rust
   let profile = Profile::load_from_file(&path)?;
   let specs   = profile.resolved_entries(...)?;
   let snaps   = SnapshotManager::open(&data_home)?;
   let journal = Journal::open(&state_home)?;
   let linker  = Linker::new(&snaps, &journal);
   let plan    = linker.plan("laptop", &specs, &home)?;
   let report  = linker.apply(plan, ApplyOptions {
       dry_run: false, force: false, home, state_home,
   })?;
   tx.send(TaskMessage::Completed {
       id: 7, kind, result: TaskResult::Apply(Ok(report)),
   });
   ```

   The lock (`ApplyLock`) is taken **inside** `Linker::apply` by the worker,
   not by the TUI process itself (§9.2).

6. **t = 1.05 s … 6 s** (apply runs) — Every 50 ms the main loop ticks.
   `is_animating()` returns true → redraw. The status bar shows
   `⠋ Applying laptop... (3s)`. The body is still whichever view is active
   (Tab navigation works during a running task). All action-triggering keys
   are gated by `ctx.busy = true` and emit
   `Action::SetStatus("operation in progress: apply (3s)")` instead of
   spawning anything.

7. **t = 6 s** — Worker sends
   `TaskMessage::Completed { id: 7, kind, result: TaskResult::Apply(Ok(report)) }`.

8. **t = 6.02 s** — Main loop's next iteration calls
   `drain_task_messages`. It matches `Apply(Ok(report))`:

   - `background = Idle`.
   - If `report.rolled_back == true` →
     `Modal::Error { title: "Apply failed — rolled back", body: human(&report) }`.
   - Else → `status_msg = format!("✓ Applied {} link(s) — snapshot {}", linked, report.snapshot_id.unwrap())`.
   - `self.snapshots.refresh(&paths)?` so SnapshotsView shows the new snapshot.
   - `self.profiles.refresh(&paths)?` so the "Last applied" timestamp
     updates.

9. **t = 6.04 s** — Repaint. User sees the success message and can keep
   working.

### Variations

- **User navigates to SnapshotsView at t = 3 s**: Tab key is allowed.
  Rendering switches; the task continues unaffected (workers don't read
  `App`, see P8 / §3.6). The spinner appears on every view because the
  status bar is global.
- **Apply fails mid-flight** (`report.rolled_back == true`): the error
  modal has `[c]` to copy details. Status bar: `⚠ Apply rolled back — see details`.
- **Worker panic**: `catch_unwind` catches it; the worker sends
  `TaskResult::Apply(Err(anyhow!("panic in worker: ...")))`. Same handler.
- **Lock contention**: `LockError::Busy { pid }` arrives wrapped in
  `anyhow::Error`. Modal text: "Another archdots process is running
  (PID 12345). Try again when it finishes."

---

## 9. Logging and locking

### 9.1 Tracing during TUI

`tracing-subscriber` would corrupt the frame if it wrote to stderr while
ratatui is in raw mode. In TUI mode the subscriber is reconfigured before
entering the alternate screen:

- Target: `$XDG_STATE_HOME/archdots/tui.log`.
- Mode: **truncated** on each `archdots tui` launch (the log is for the
  current session; previous sessions are not preserved by default — they
  live in the journal anyway).
- Level: `INFO` by default. Overridable via `ARCHDOTS_LOG=debug` (uses
  `tracing_subscriber::EnvFilter`).
- The CLI's `--log-level` global flag does not apply in TUI mode — the
  TUI is launched without arguments. `ARCHDOTS_LOG` is the only knob.

The file remains after the TUI exits, for post-mortem inspection. The
`tui.log` path is also shown in the help overlay (`?`) for discoverability.

### 9.2 Locking model

The **TUI process does not acquire `ApplyLock`**. Only the per-task worker
thread acquires it when performing a mutating operation:

| Operation | Lock taken by | Where |
|---|---|---|
| Apply | worker | inside `Linker::apply` |
| Rollback | worker | inside `Linker::rollback` |
| RollbackToSnapshot | worker | inside `Linker::rollback_to_snapshot` |
| Prune | worker | inside `SnapshotManager::prune` (lock not required by core today, but the TUI worker is the only entry point for prune-from-TUI; this leaves room for future locking) |
| Check | none | read-only |
| Refresh* | none | read-only |

Consequences:

- **Two `archdots tui` processes can coexist** on the same system. Both can
  browse profiles, snapshots, and journal entries simultaneously. Only one
  may run a mutating worker at a time.
- If the second TUI's worker tries to grab the lock while the first's
  worker holds it, the second receives `LockError::Busy { pid }` and the
  modal reads "Another archdots process is running (PID …)".
- This is consistent with the CLI: `archdots apply` already runs without
  the CLI process pre-acquiring the lock; the lock is per-operation, not
  per-process.

---

## 10. New dependencies

| Crate | Action | Justification |
|---|---|---|
| `ratatui = "0.28"` | already in workspace | the renderer |
| `crossterm = "0.28"` | already in workspace | input + raw mode |
| `color-eyre = "0.6"` | already in workspace | panic reporter |
| `similar = "2"` | already in workspace | reused in DiffView (same crate as `cmd::diff`) |
| **`fuzzy-matcher = "0.3"`** | **ADD** | `SkimMatcherV2` ranks `/` search in Profiles/Snapshots. ~50 KB compiled, no notable transitive deps. Standard in helix / zellij / gitui. |
| **`arboard = "3"`** | **ADD** | Copy install commands and error details. Wayland + X11 supported. On clipboard-init failure, fall back to a hint message ("clipboard unavailable; use mouse selection to copy"). |
| `throbber-widgets-tui` | SKIP | Spinner is `["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"][tick % 10]`. 10 lines we own. |
| `tui-input` | SKIP | Single-line search input is a 40-line `SearchInput { buf: String, cursor: usize }`. |
| `tokio` / `async-std` / `smol` | SKIP | See §3.2 and ADR-002. |

Net impact: **+2 crates**, both small, both single-purpose. Compatible with
the project's "no unnecessary deps" rule.

---

## 11. Edge cases

| # | Case | Handling |
|---|---|---|
| 1 | Terminal narrower than 80 cols | `Layout` constraints use `Min(0)`. If width < 60, render a single-column fallback. If width < 40, replace body with "Terminal too small (min 40×10)". Status bar always shown. |
| 2 | Resize during long operation | crossterm emits `Event::Resize(w, h)`. Worker is untouched (it does not render). Main loop re-renders next frame. |
| 3 | Apply fails mid-flight | `ApplyReport { rolled_back: true, applied: [...] }`. Open `Modal::Error` with per-link summary (count of `Linked`/`Skipped`/`Failed`, first failure reason). Status bar: `⚠ Apply rolled back — see details`. |
| 4 | Ctrl+C during apply | Intercepted in `App::step`. If `background.is_running()` → inline hint "press Ctrl+C again within 3 s to force-quit (will leave an orphan)". Second Ctrl+C calls `std::process::exit(130)`; orphan is recovered on next launch. |
| 5 | NO_COLOR=1 | `Theme::detect` honours it. All `Color::*` are replaced with `Color::Reset`; status icons fall back to ASCII (`[ok]`, `[X]`, `[?]`, `[!]`). |
| 6 | `LANG=C` / no Unicode | `Theme::use_unicode = false`. Spinner becomes `[|/-\]`; icons ASCII; box drawing falls back to ratatui's ASCII set. |
| 7 | tmux / screen resize batching | Same `Event::Resize` handler. We coalesce by draining Tick + Resize events in one iteration before redrawing. |
| 8 | Operation > 30 s (e.g. `--deep` check) | After 10 s, status bar appends "still running…"; after 30 s, also "this can take > 60 s with --deep". No automatic cancellation. |
| 9 | Worker thread panics | `catch_unwind(AssertUnwindSafe(...))` wraps the body. The panic message becomes an `anyhow::Error` on the channel. The TUI never crashes. |
| 10 | Quit while op in progress | Opens `Modal::Confirm { kind: ConfirmKind::QuitWithRunningTask, prompt: "Operation in progress; quitting will leave an orphan. Quit anyway?" }`. |
| 11 | Profile dir doesn't exist | `ProfilesView::new` returns an empty view with `status_msg = "No profiles found. Run 'archdots init' first."` Other tabs still work. |
| 12 | Journal corrupt | `Journal::iter` skips bad lines (core behaviour). Views render whatever's parseable; no fatal error. |
| 13 | Profile load fails | Detail panel shows the error; cursor can move past it. Apply/diff on a broken profile triggers a `Modal::Error` instead of running. |
| 14 | Concurrent CLI invocation grabs the lock | Worker returns `LockError::Busy { pid }`. Modal "Another archdots is running (PID xxx)." |
| 15 | Invalid filenames in `profiles_dir` | `Profile::list_names` ignores non-TOML files; invalid names are skipped on load and surface as a per-item error in the detail panel. |
| 16 | Snapshot tarball corrupt | Detail panel renders "tarball corrupt — restore is disabled for this snapshot". Restore attempt opens a `Modal::Error`. |
| 17 | Clipboard unavailable | `arboard::Clipboard::new()` returns `Err`. Modal renders the hint "clipboard unavailable; use mouse selection to copy" and ignores the `c` key. Modal body remains visible. |
| 18 | `pacman` absent (TUI on Ubuntu, somehow) | DepsView shows "archdots check requires pacman" (same string as CLI). Other views work fine. |
| 19 | `TERM=dumb` (no alt-screen) | crossterm fails at `EnterAlternateScreen`. We refuse and print a single line to stderr. TUI assumes a modern terminal. |
| 20 | Two `archdots tui` instances simultaneously | Allowed by design (§9.2). Both can browse; only one's worker may hold the lock at a time. |

---

## 12. Invocation — `archdots tui`

### 12.1 Decision

**Explicit subcommand only.** `archdots tui` enters the TUI. `archdots` (no
args) keeps `arg_required_else_help = true` and prints the help text.

### 12.2 Rationale

- Clap can support a default subcommand by stripping
  `arg_required_else_help` and adding custom logic in `main`, at the cost of
  surprising CI/scripts (`archdots` accidentally entering raw mode), TTY
  detection ambiguity, and a `arg_required_else_help` flag that becomes a
  lie.
- A separate subcommand is one command longer to type and has zero
  ambiguity. Discoverable via `archdots --help`. Future autocompletion
  (clap_complete) gets it for free.

### 12.3 Wiring

```rust
// main.rs additions
enum Commands {
    // ... existing variants ...
    /// Launch the interactive TUI.
    Tui,
}

Commands::Tui => cmd::tui::run(),
```

`cmd::tui::run` opens the TUI log (§9.1), reconfigures
`tracing-subscriber`, enters raw mode + alt screen, then delegates to
`tui::run_loop`. A `Drop` guard ensures the terminal is restored on every
exit path (normal, panic, propagation of `?`).

---

## 13. Core API additions

Two additive functions in `archdots-core`. Both are required by the TUI and
have CLI value as well.

### 13.1 `Linker::rollback_to_snapshot`

```rust
impl<'a> Linker<'a> {
    /// Restore an arbitrary snapshot. Unlike [`Self::rollback`], this does
    /// not consult the journal to find a target — the caller supplies the
    /// snapshot id directly. The profile name is read from the snapshot
    /// manifest and used for journal accounting and orphan checks.
    ///
    /// Journal chain written (action = `Rollback`):
    /// 1. `InProgress`, `snapshot_id: None`, `supersedes: None`
    /// 2. `InProgress`, `snapshot_id: Some(id)`, `supersedes: Entry1`
    /// 3. `Success | Partial | Failed`, `supersedes: Entry2`
    ///
    /// Same orphan-recovery semantics as [`Self::apply`]: a crash between
    /// entries 1 and 3 leaves an `InProgress` orphan that `archdots recover`
    /// reconciles on next launch.
    ///
    /// # Errors
    ///
    /// - [`SnapshotError::NotFound`] if `snapshot_id` is unknown.
    /// - [`LinkerError::OrphanedTransaction`] if the profile has an open
    ///   in-progress entry.
    /// - [`LockError::Busy`] if another process holds the apply lock.
    /// - I/O / snapshot / journal errors as appropriate.
    pub fn rollback_to_snapshot(
        &self,
        snapshot_id: &SnapshotId,
        opts: ApplyOptions,
    ) -> Result<ApplyReport, CoreError>;
}
```

Implementation notes:

- Step order: load snapshot (`self.snapshots.get(id)?`) → derive profile
  from `manifest.profile` → acquire lock → orphan check for that profile →
  write Entry 1 (InProgress, no snapshot) → write Entry 2 (InProgress, with
  snapshot) → restore → write Entry 3.
- Reusing `Linker::rollback`'s closing-entry logic is encouraged but not
  mandated; the two functions may share a private helper.
- `ApplyReport.applied` is populated from `RestoreReport.restored` (same as
  `rollback`). `journal_id` is Entry 1's id (anchors the transaction).
- The function is part of the stable public API from v0.4.0.

### 13.2 `Profile::list_names`

```rust
impl Profile {
    /// List profile names in `profiles_dir`. Returns names without the
    /// `.toml` extension, sorted ascending. Non-TOML files and entries
    /// that fail to parse a valid name from their stem are skipped
    /// silently.
    ///
    /// Returns an empty `Vec` (not an error) if `profiles_dir` does not
    /// exist — the typical first-run case.
    ///
    /// # Errors
    ///
    /// - [`ProfileError::Io`] if `profiles_dir` exists but cannot be read.
    pub fn list_names(profiles_dir: &Path) -> Result<Vec<String>, ProfileError>;
}
```

Implementation notes:

- Replaces the open-coded loop in `cmd::profile::run_list`. The CLI is
  refactored to call `Profile::list_names`.
- Used by `ProfilesView::new` and `ProfilesView::refresh`.
- No validation of file content beyond the name — listing must remain
  cheap. Full parsing happens lazily when a profile is loaded.

These are the **only** core API additions for Phase 4. `ApplyReport` and
`ValidationReport` are unchanged.

---

## 14. Testing approach

### 14.1 Per-view unit tests (no rendering)

For each of `ProfilesView`, `SnapshotsView`, `DepsView`, `DiffView`:

- Construct the view + a fake `ViewCtx`.
- Push a `crossterm::event::Event` (key, resize, etc.).
- Assert on the returned `Action` and on internal mutation (cursor
  position, search filter, section).

Target: ~20 tests per view.

### 14.2 App-level integration

Construct an `App` with a controlled `mpsc::Receiver<TaskMessage>` (the
`Sender` half is held by the test). Drive `App::step` with an event
sequence, then inject `TaskMessage::Completed { .. }` directly into the
channel and assert on state transitions.

**Critical tests:**

- `app_test_action_blocked_during_busy_state` — verify that if
  `App::dispatch(Action::SpawnTask)` is invoked while
  `BackgroundState::Running`, the second task is **not** spawned and
  instead an `Action::SetStatus("operation in progress: <kind> (<elapsed>s)")`
  is emitted (or equivalent observable state mutation).
- `app_test_task_completion_transitions_to_idle` — `TaskMessage::Completed`
  on the channel transitions `Running → Idle` and applies the result.
- `app_test_panic_in_worker_becomes_error_modal` — feed a panicked
  `TaskResult` and confirm `self.modal = Some(Modal::Error { .. })`.
- `app_test_quit_during_busy_opens_confirm_modal` — pressing the quit key
  while `Running` opens `Modal::Confirm { kind: ConfirmKind::QuitWithRunningTask }`.

### 14.3 Confirm-kind transitions

For each `ConfirmKind` variant, a test that calls
`App::execute_confirm(kind)` and asserts on the returned `Action`. This
locks the dispatch table.

### 14.4 What we don't test

- Pixel-accurate rendering (use ratatui's `TestBackend` only for the
  smallest sanity check that a frame renders without panicking).
- Style / colour palette — compiles, runs, looks right; validated by
  manual dogfooding.

### 14.5 Coverage target

≥ 70 % on `tui/views/*.rs` and `tui/app.rs` (project-wide bar). Render
functions count against LOC but not against the bar; pragmatically, no
special-case gating.

---

## 15. Out of scope for Phase 4

- Saved UI preferences (`config.toml [ui]`).
- Mouse support beyond what crossterm provides for terminal-native selection.
- Per-view custom themes.
- Inline editing of `profile.toml` from the TUI.
- Embedded shell / terminal pane.
- Notifications via `notify-rust`.
- A debug overlay tailing `tui.log` live.
- Snapshot **comparison** (diff between two snapshots).
- Cancellation of running tasks (see §3.4).
- A "task queue" — only one operation at a time (see §3.3).
- `Linker::apply_with_progress` callback per item — deferred to Phase 5+.
- Picking AUR vs repo when both provide a binary in DepsView's
  copy-command action — uses curated table priority.
- Multi-distro support — out of scope project-wide (ADR-003).
