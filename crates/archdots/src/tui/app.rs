//! `App` — top-level TUI state machine.

#![allow(clippy::module_name_repetitions)]

use std::{path::PathBuf, sync::mpsc, time::Instant};

use time::OffsetDateTime;

use ratatui::{
    layout::Alignment,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{block::Title, Block, Borders, Paragraph, Tabs},
    Frame,
};

use crate::tui::{
    action::{Action, ConfirmKind},
    events::Event,
    tasks::{BackgroundKind, TaskId, TaskMessage, TaskResult},
    theme::Theme,
    ui::{self, layout, Modal, ModalOutcome, StatusMessage},
    views::{DepsView, DiffView, ProfilesView, SnapshotsView, View, ViewCtx, ViewKind},
};

// ─── AppPaths ─────────────────────────────────────────────────────────────────

/// XDG paths resolved at startup and cloned into every background worker.
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub home: PathBuf,
    pub profiles_dir: PathBuf,
    pub data_home: PathBuf,
    pub state_home: PathBuf,
}

// ─── BackgroundState ──────────────────────────────────────────────────────────

/// Whether a background task is currently running.
#[derive(Debug)]
pub enum BackgroundState {
    Idle,
    Running {
        // Reserved for stale-completion detection: compare with Completed.id before
        // transitioning to Idle to guard against a force-quit/restart race.
        #[allow(dead_code)]
        id: TaskId,
        kind: BackgroundKind,
        started: Instant,
    },
}

// ─── Journal helpers ──────────────────────────────────────────────────────────

/// Read the most recent successful apply entry from the journal.
///
/// Silently returns `None` on any error so a missing or corrupt journal never
/// prevents the TUI from starting.
fn read_last_apply(paths: &AppPaths) -> Option<(String, OffsetDateTime)> {
    use archdots_core::journal::{Journal, JournalAction, JournalStatus};
    let journal = Journal::open(&paths.state_home).ok()?;
    let mut latest: Option<(String, OffsetDateTime)> = None;
    for result in journal.iter().ok()? {
        let Ok(entry) = result else { continue };
        if entry.action == JournalAction::Apply
            && entry.status == JournalStatus::Success
            && latest.as_ref().is_none_or(|(_, ts)| entry.ts > *ts)
        {
            latest = Some((entry.profile.clone(), entry.ts));
        }
    }
    latest
}

/// Format an `OffsetDateTime` as a human-readable relative time string.
fn format_relative_time(ts: OffsetDateTime) -> String {
    let now = OffsetDateTime::now_utc();
    let secs = (now - ts).whole_seconds().max(0).unsigned_abs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

// ─── App ──────────────────────────────────────────────────────────────────────

/// Top-level TUI state: views, modal, background task, channel.
pub struct App {
    // ── Per-view state ──
    profiles: ProfilesView,
    snapshots: SnapshotsView,
    deps: DepsView,
    diff: DiffView,

    // ── Navigation ──
    active: ViewKind,
    modal: Option<Modal>,
    status_msg: StatusMessage,
    background: BackgroundState,

    // ── Infrastructure ──
    paths: AppPaths,
    theme: Theme,
    next_task_id: TaskId,
    tx: mpsc::Sender<TaskMessage>,
    rx: mpsc::Receiver<TaskMessage>,
    should_quit: bool,
    dirty: bool,
    last_apply: Option<(String, OffsetDateTime)>,
}

impl App {
    /// Construct a new `App`.
    ///
    /// Creates the internal `mpsc` channel; background tasks will be sent
    /// clones of `tx`.
    ///
    /// # Errors
    ///
    /// Currently infallible; the `Result` is reserved for future initialisation
    /// that may fail (e.g. reading the profiles directory on startup).
    #[allow(clippy::unnecessary_wraps)]
    pub fn new(paths: AppPaths, theme: Theme) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<TaskMessage>();
        let last_apply = read_last_apply(&paths);
        Ok(Self {
            profiles: ProfilesView::new(&paths),
            snapshots: SnapshotsView::new(&paths),
            deps: DepsView::empty(),
            diff: DiffView::empty(),
            active: ViewKind::Profiles,
            modal: None,
            status_msg: StatusMessage::None,
            background: BackgroundState::Idle,
            paths,
            theme,
            next_task_id: TaskId(0),
            tx,
            rx,
            should_quit: false,
            dirty: true,
            last_apply,
        })
    }

    /// Drain all pending `TaskMessage`s from the channel.
    ///
    /// Called once per event-loop iteration before `step`.
    ///
    /// # Errors
    ///
    /// Currently infallible; the `Result` is reserved for future error
    /// propagation.
    #[allow(clippy::unnecessary_wraps)]
    pub fn drain_task_messages(&mut self) -> anyhow::Result<()> {
        while let Ok(TaskMessage::Completed { result, kind, .. }) = self.rx.try_recv() {
            self.background = BackgroundState::Idle;
            self.handle_task_result(&result, &kind);
        }
        Ok(())
    }

    /// Process one input event and return `true` if a redraw is needed.
    ///
    /// # Errors
    ///
    /// Propagates errors from `dispatch`.
    #[allow(clippy::needless_pass_by_value, clippy::unnecessary_wraps)]
    pub fn step(&mut self, ev: Event) -> anyhow::Result<bool> {
        self.dirty = false;
        match ev {
            Event::Tick => {
                if self.is_animating() {
                    self.dirty = true;
                }
            }
            Event::Input(crossterm::event::Event::Resize(_, _)) => {
                self.dirty = true;
            }
            Event::Input(crossterm::event::Event::Key(key)) => {
                // Take the modal out temporarily to avoid borrow conflicts.
                if let Some(mut modal) = self.modal.take() {
                    match modal.handle_key(key) {
                        ModalOutcome::KeepOpen => {
                            self.modal = Some(modal);
                        }
                        ModalOutcome::Close => {
                            self.dirty = true;
                        }
                        ModalOutcome::Confirm(kind) => {
                            let action = self.execute_confirm(kind);
                            self.dispatch(action);
                            self.dirty = true;
                        }
                        ModalOutcome::Copy(_text) => {
                            // Clipboard handling — Session 3+.
                            self.modal = Some(modal);
                        }
                    }
                } else {
                    let action = self.handle_global_key(key);
                    self.dispatch(action);
                }
            }
            Event::Input(_) => {}
        }
        Ok(self.dirty)
    }

    /// Draw the full TUI frame.
    pub fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();

        if area.width < 40 || area.height < 10 {
            let msg = format!(
                "Terminal too small ({}×{}). Minimum: 40×10.",
                area.width, area.height
            );
            frame.render_widget(
                Paragraph::new(msg).style(Style::default().fg(self.theme.err)),
                area,
            );
            return;
        }

        let [top_bar, body, status] = layout::main_chunks(area);

        self.render_top_bar(frame, top_bar);

        let theme = &self.theme;
        match self.active {
            ViewKind::Profiles => self.profiles.render(frame, body, theme),
            ViewKind::Snapshots => self.snapshots.render(frame, body, theme),
            ViewKind::Deps => self.deps.render(frame, body, theme),
            ViewKind::Diff => self.diff.render(frame, body, theme),
        }

        if let Some(modal) = &self.modal {
            modal.render(frame, theme);
        }

        ui::status_bar::render_status_bar(frame, status, &self.status_msg, &self.background, theme);
    }

    /// `true` while a background task is running (drives spinner redraws).
    #[must_use]
    pub fn is_animating(&self) -> bool {
        matches!(self.background, BackgroundState::Running { .. })
    }

    /// `true` when the event loop should exit.
    #[must_use]
    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    // ─── Private ──────────────────────────────────────────────────────────────

    fn handle_global_key(&mut self, key: crossterm::event::KeyEvent) -> Action {
        use crossterm::event::{KeyCode, KeyModifiers};

        match (key.code, key.modifiers) {
            // Quit / Ctrl+C
            (KeyCode::Char('q'), KeyModifiers::NONE)
            | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                if self.is_animating() {
                    Action::OpenModal(Modal::Confirm {
                        prompt:
                            "Operation in progress; quitting will leave an orphan. Quit anyway?"
                                .into(),
                        details: None,
                        kind: ConfirmKind::QuitWithRunningTask,
                    })
                } else {
                    Action::Quit
                }
            }
            // Help overlay
            (KeyCode::Char('?'), _) => {
                let hints: Vec<_> = match self.active {
                    ViewKind::Profiles => self.profiles.footer_hints(),
                    ViewKind::Snapshots => self.snapshots.footer_hints(),
                    ViewKind::Deps => self.deps.footer_hints(),
                    ViewKind::Diff => self.diff.footer_hints(),
                };
                let body = crate::tui::views::help::build_help_body(
                    self.active,
                    &hints,
                    &self.paths.profiles_dir,
                );
                Action::OpenModal(Modal::ScrollableInfo {
                    title: "Help".into(),
                    body,
                    scroll: 0,
                })
            }
            // Tab switching by number
            (KeyCode::Char('1'), _) => Action::SwitchView(ViewKind::Profiles),
            (KeyCode::Char('2'), _) => Action::SwitchView(ViewKind::Snapshots),
            (KeyCode::Char('3'), _) => Action::SwitchView(ViewKind::Deps),
            (KeyCode::Char('4'), _) => Action::SwitchView(ViewKind::Diff),
            // Tab key — cycle forward
            (KeyCode::Tab, _) => {
                let all = ViewKind::all();
                let idx = all.iter().position(|v| *v == self.active).unwrap_or(0);
                Action::SwitchView(all[(idx + 1) % all.len()])
            }
            // Delegate to active view
            _ => {
                let busy = self.is_animating();
                let ctx = ViewCtx {
                    paths: &self.paths,
                    busy,
                };
                let ev = crossterm::event::Event::Key(key);
                match self.active {
                    ViewKind::Profiles => self.profiles.handle_event(&ev, &ctx),
                    ViewKind::Snapshots => self.snapshots.handle_event(&ev, &ctx),
                    ViewKind::Deps => self.deps.handle_event(&ev, &ctx),
                    ViewKind::Diff => self.diff.handle_event(&ev, &ctx),
                }
            }
        }
    }

    fn dispatch(&mut self, action: Action) {
        match action {
            Action::None => {}
            Action::SwitchView(kind) => {
                self.active = kind;
                let busy = self.is_animating();
                let ctx = ViewCtx {
                    paths: &self.paths,
                    busy,
                };
                match kind {
                    ViewKind::Profiles => self.profiles.on_focus(&ctx),
                    ViewKind::Snapshots => self.snapshots.on_focus(&ctx),
                    ViewKind::Deps => self.deps.on_focus(&ctx),
                    ViewKind::Diff => self.diff.on_focus(&ctx),
                }
                self.dirty = true;
            }
            Action::SelectProfileForDeps(name) => {
                self.deps.set_profile(name.clone());
                self.active = ViewKind::Deps;
                self.dirty = true;
                if !self.is_animating() {
                    self.spawn_task(BackgroundKind::Check {
                        profile: name,
                        deep: false,
                    });
                }
            }
            Action::SelectProfileForDiff(name) => {
                let paths = self.paths.clone();
                self.diff.set_profile(&name, &paths);
                self.active = ViewKind::Diff;
                self.dirty = true;
            }
            Action::SpawnTask(kind) => {
                if self.is_animating() {
                    let label = format!("{kind:?}");
                    let label = label
                        .split_once('{')
                        .map_or(label.as_str(), |(l, _)| l)
                        .trim();
                    let elapsed = match &self.background {
                        BackgroundState::Running { started, .. } => started.elapsed().as_secs(),
                        BackgroundState::Idle => 0,
                    };
                    self.status_msg =
                        StatusMessage::Info(format!("operation in progress: {label} ({elapsed}s)"));
                    self.dirty = true;
                } else {
                    self.spawn_task(kind);
                }
            }
            Action::SetStatus(msg) => {
                self.status_msg = msg;
                self.dirty = true;
            }
            Action::OpenModal(modal) => {
                self.modal = Some(modal);
                self.dirty = true;
            }
            Action::CloseModal => {
                self.modal = None;
                self.dirty = true;
            }
            Action::Quit => {
                self.should_quit = true;
            }
            Action::CopyToClipboard(text) => {
                match arboard::Clipboard::new().and_then(|mut c| c.set_text(text.clone())) {
                    Ok(()) => {
                        self.status_msg = StatusMessage::Success(format!("Copied: {text}"));
                    }
                    Err(e) => {
                        tracing::warn!("clipboard error: {e}");
                        self.modal = Some(Modal::Error {
                            title: "Clipboard unavailable".into(),
                            body: format!(
                                "Could not access clipboard. Command:\n\n{text}\n\n\
                                 Select the text above manually."
                            ),
                        });
                    }
                }
                self.dirty = true;
            }
        }
    }

    fn spawn_task(&mut self, kind: BackgroundKind) {
        let id = self.next_task_id;
        self.next_task_id = TaskId(self.next_task_id.0 + 1);
        self.background = BackgroundState::Running {
            id,
            kind: kind.clone(),
            started: Instant::now(),
        };
        crate::tui::tasks::spawn(id, kind, self.paths.clone(), self.tx.clone());
    }

    fn execute_confirm(&mut self, kind: ConfirmKind) -> Action {
        match kind {
            ConfirmKind::ApplyProfile(name) => {
                Action::SpawnTask(BackgroundKind::Apply { profile: name })
            }
            ConfirmKind::RollbackProfile(name) => {
                Action::SpawnTask(BackgroundKind::Rollback { profile: name })
            }
            ConfirmKind::RollbackToSnapshot(id) => {
                Action::SpawnTask(BackgroundKind::RollbackToSnapshot { id })
            }
            ConfirmKind::DeleteProfile(name) => {
                let path = self.paths.profiles_dir.join(format!("{name}.toml"));
                if let Err(e) = std::fs::remove_file(&path) {
                    return Action::SetStatus(StatusMessage::Error(format!(
                        "Failed to delete profile '{name}': {e}"
                    )));
                }
                let paths = self.paths.clone();
                self.profiles.refresh(&paths);
                Action::SetStatus(StatusMessage::Success(format!("Profile deleted: {name}")))
            }
            ConfirmKind::PruneSnapshot(id) => {
                Action::SpawnTask(BackgroundKind::PruneSnapshot { id })
            }
            ConfirmKind::QuitWithRunningTask => Action::Quit,
        }
    }

    fn handle_task_result(&mut self, result: &TaskResult, kind: &BackgroundKind) {
        match result {
            TaskResult::Apply(Ok(ref rep)) => {
                if rep.rolled_back {
                    self.modal = Some(Modal::Error {
                        title: "Apply failed — rolled back".into(),
                        body: format!(
                            "{} link(s) applied before failure. Snapshot was restored.",
                            rep.applied.len()
                        ),
                    });
                } else {
                    self.status_msg =
                        StatusMessage::Success(format!("Applied {} link(s)", rep.applied.len()));
                    self.last_apply = read_last_apply(&self.paths);
                }
                let paths = self.paths.clone();
                self.snapshots.refresh(&paths);
                self.profiles.refresh(&paths);
                // Refresh DiffView if it's active and shows the same profile.
                if let BackgroundKind::Apply {
                    profile: ref applied_profile,
                } = *kind
                {
                    if matches!(self.active, ViewKind::Diff)
                        && self.diff.profile.as_deref() == Some(applied_profile.as_str())
                    {
                        self.diff.set_profile(applied_profile, &paths);
                    }
                }
            }
            TaskResult::Apply(Err(ref e)) => {
                self.modal = Some(Modal::Error {
                    title: "Apply failed".into(),
                    body: e.to_string(),
                });
            }
            TaskResult::Rollback(Ok(ref rep)) => {
                self.status_msg =
                    StatusMessage::Success(format!("Rolled back {} entry(s)", rep.applied.len()));
                let paths = self.paths.clone();
                self.snapshots.refresh(&paths);
            }
            TaskResult::Rollback(Err(ref e)) => {
                self.modal = Some(Modal::Error {
                    title: "Rollback failed".into(),
                    body: e.to_string(),
                });
            }
            TaskResult::SnapshotList(Ok(_)) => {
                self.status_msg = StatusMessage::Info("Snapshots refreshed".into());
                let paths = self.paths.clone();
                self.snapshots.refresh(&paths);
            }
            TaskResult::SnapshotList(Err(ref e)) => {
                self.modal = Some(Modal::Error {
                    title: "Error".into(),
                    body: e.to_string(),
                });
            }
            TaskResult::Check(Ok(ref report)) => {
                let missing = report
                    .entries
                    .iter()
                    .filter(|e| matches!(e.status, archdots_core::validator::DepStatus::Missing))
                    .count();
                self.status_msg =
                    StatusMessage::Success(format!("Validation complete: {missing} missing"));
                self.deps.apply_report(report.clone());
            }
            TaskResult::Check(Err(ref e)) => {
                // Show error in DepsView, not as a modal (expected failure path).
                self.deps.set_error(e.to_string());
                self.status_msg = StatusMessage::Error(format!("Check failed: {e}"));
            }
            TaskResult::Prune(Ok(ref rep)) => {
                self.status_msg =
                    StatusMessage::Success(format!("Pruned {} snapshot(s)", rep.removed.len()));
                let paths = self.paths.clone();
                self.snapshots.refresh(&paths);
            }
            TaskResult::Prune(Err(ref e)) => {
                self.modal = Some(Modal::Error {
                    title: "Prune failed".into(),
                    body: e.to_string(),
                });
            }
        }
        self.dirty = true;
    }

    fn render_top_bar(&self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        let all = ViewKind::all();
        let selected = all.iter().position(|v| *v == self.active).unwrap_or(0);

        let titles: Vec<Line<'_>> = all
            .iter()
            .map(|v| Line::from(Span::raw(format!("[{}]{}", v.key(), v.name()))))
            .collect();

        let last_apply_label = match &self.last_apply {
            Some((name, ts)) => format!(" last applied: {name} ({}) ", format_relative_time(*ts)),
            None => " no apply history ".to_string(),
        };

        let tabs = Tabs::new(titles)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(Span::styled(
                        concat!(" archdots ", env!("CARGO_PKG_VERSION"), " "),
                        Style::default().add_modifier(Modifier::BOLD),
                    ))
                    .title(
                        Title::from(Span::styled(
                            last_apply_label,
                            Style::default().fg(self.theme.muted),
                        ))
                        .alignment(Alignment::Right),
                    )
                    .title_alignment(Alignment::Left),
            )
            .select(selected)
            .style(Style::default().fg(self.theme.muted))
            .highlight_style(
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            );

        frame.render_widget(tabs, area);
    }

    // ─── Test helpers ─────────────────────────────────────────────────────────

    /// Return a clone of the internal task sender so tests can inject messages.
    #[cfg(test)]
    pub fn test_sender(&self) -> mpsc::Sender<TaskMessage> {
        self.tx.clone()
    }

    /// Force `background` to `Running` so tests can verify busy-state guards.
    #[cfg(test)]
    pub fn force_running(&mut self) {
        self.background = BackgroundState::Running {
            id: TaskId(0),
            kind: BackgroundKind::RefreshSnapshots,
            started: Instant::now(),
        };
    }

    /// Expose `background` for assertions.
    #[cfg(test)]
    pub fn is_idle(&self) -> bool {
        matches!(self.background, BackgroundState::Idle)
    }

    /// Expose `modal` for assertions.
    #[cfg(test)]
    pub fn modal(&self) -> Option<&Modal> {
        self.modal.as_ref()
    }

    /// Expose `active` for assertions.
    #[cfg(test)]
    pub fn active(&self) -> ViewKind {
        self.active
    }

    /// Expose `status_msg` for assertions.
    #[cfg(test)]
    pub fn status_msg(&self) -> &StatusMessage {
        &self.status_msg
    }

    /// Expose deps profile name for assertions.
    #[cfg(test)]
    pub fn deps_profile(&self) -> Option<&str> {
        self.deps.profile.as_deref()
    }

    /// Expose whether deps has a loaded report.
    #[cfg(test)]
    pub fn deps_report_is_some(&self) -> bool {
        self.deps.report.is_some()
    }

    /// Expose deps error for assertions.
    #[cfg(test)]
    pub fn deps_error(&self) -> Option<&str> {
        self.deps.error.as_deref()
    }

    /// Expose diff profile name for assertions.
    #[cfg(test)]
    pub fn diff_profile(&self) -> Option<&str> {
        self.diff.profile.as_deref()
    }

    /// Expose diff item kind at a given index for assertions.
    #[cfg(test)]
    pub fn diff_item_kind(&self, idx: usize) -> Option<crate::tui::views::diff::DiffItemKind> {
        self.diff.item_kind(idx)
    }

    /// Expose last-apply profile name for assertions.
    #[cfg(test)]
    pub fn last_apply_profile(&self) -> Option<&str> {
        self.last_apply.as_ref().map(|(name, _)| name.as_str())
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn fake_paths() -> AppPaths {
        AppPaths {
            home: PathBuf::from("/tmp/test-home"),
            profiles_dir: PathBuf::from("/tmp/test-home/.config/archdots/profiles"),
            data_home: PathBuf::from("/tmp/test-home/.local/share"),
            state_home: PathBuf::from("/tmp/test-home/.local/state"),
        }
    }

    fn make_app() -> App {
        App::new(fake_paths(), Theme::detect()).unwrap()
    }

    fn key_event(code: crossterm::event::KeyCode) -> Event {
        use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        Event::Input(crossterm::event::Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }))
    }

    #[test]
    fn app_new_constructs_correctly() {
        let app = make_app();
        assert!(app.is_idle());
        assert!(!app.should_quit());
        assert_eq!(app.active(), ViewKind::Profiles);
        assert!(app.modal().is_none());
    }

    #[test]
    fn step_tick_idle_not_dirty() {
        let mut app = make_app();
        app.dirty = false;
        let dirty = app.step(Event::Tick).unwrap();
        assert!(!dirty, "tick in idle state must not mark dirty");
    }

    #[test]
    fn step_tick_busy_is_dirty() {
        let mut app = make_app();
        app.force_running();
        let dirty = app.step(Event::Tick).unwrap();
        assert!(dirty, "tick while running must mark dirty for spinner");
    }

    #[test]
    fn step_q_sets_should_quit() {
        let mut app = make_app();
        app.step(key_event(crossterm::event::KeyCode::Char('q')))
            .unwrap();
        assert!(app.should_quit());
    }

    #[test]
    fn step_question_mark_opens_modal() {
        let mut app = make_app();
        app.step(key_event(crossterm::event::KeyCode::Char('?')))
            .unwrap();
        assert!(app.modal().is_some(), "? must open a modal");
        assert!(matches!(app.modal(), Some(Modal::ScrollableInfo { .. })));
    }

    #[test]
    fn step_number_keys_switch_views() {
        let mut app = make_app();

        app.step(key_event(crossterm::event::KeyCode::Char('2')))
            .unwrap();
        assert_eq!(app.active(), ViewKind::Snapshots);

        app.step(key_event(crossterm::event::KeyCode::Char('3')))
            .unwrap();
        assert_eq!(app.active(), ViewKind::Deps);

        app.step(key_event(crossterm::event::KeyCode::Char('4')))
            .unwrap();
        assert_eq!(app.active(), ViewKind::Diff);

        app.step(key_event(crossterm::event::KeyCode::Char('1')))
            .unwrap();
        assert_eq!(app.active(), ViewKind::Profiles);
    }

    #[test]
    fn step_tab_rotates_view() {
        let mut app = make_app();
        // Start at Profiles → Snapshots → Deps → Diff → Profiles
        for expected in &[
            ViewKind::Snapshots,
            ViewKind::Deps,
            ViewKind::Diff,
            ViewKind::Profiles,
        ] {
            app.step(key_event(crossterm::event::KeyCode::Tab)).unwrap();
            assert_eq!(app.active(), *expected);
        }
    }

    #[test]
    fn action_blocked_during_busy_state() {
        let mut app = make_app();
        app.force_running();
        assert!(app.is_animating());

        // Dispatch a SpawnTask while busy — must NOT spawn a new task.
        app.dispatch(Action::SpawnTask(BackgroundKind::RefreshSnapshots));

        // Still running (the original forced-running task), not a new one.
        assert!(app.is_animating(), "must still be running");
        // Status message updated to warn.
        assert!(matches!(app.status_msg(), StatusMessage::Info(_)));
    }

    #[test]
    fn drain_task_messages_completed_ok_transitions_to_idle() {
        let mut app = make_app();
        app.force_running();
        assert!(app.is_animating());

        // Inject a success result directly on the channel.
        let tx = app.test_sender();
        tx.send(TaskMessage::Completed {
            id: TaskId(0),
            kind: BackgroundKind::RefreshSnapshots,
            result: TaskResult::SnapshotList(Ok(vec![])),
        })
        .unwrap();

        app.drain_task_messages().unwrap();
        assert!(app.is_idle(), "must transition to Idle after completion");
    }

    #[test]
    fn drain_task_messages_completed_err_opens_error_modal() {
        let mut app = make_app();
        app.force_running();

        let tx = app.test_sender();
        tx.send(TaskMessage::Completed {
            id: TaskId(0),
            kind: BackgroundKind::RefreshSnapshots,
            result: TaskResult::SnapshotList(Err(anyhow::anyhow!("simulated failure"))),
        })
        .unwrap();

        app.drain_task_messages().unwrap();
        assert!(app.is_idle());
        assert!(
            matches!(app.modal(), Some(Modal::Error { .. })),
            "error result must open an error modal"
        );
    }

    #[test]
    fn quit_during_busy_opens_confirm_modal() {
        let mut app = make_app();
        app.force_running();

        // Pressing 'q' while running → confirm modal, not immediate quit.
        app.step(key_event(crossterm::event::KeyCode::Char('q')))
            .unwrap();
        assert!(!app.should_quit(), "must not quit immediately");
        assert!(
            matches!(
                app.modal(),
                Some(Modal::Confirm {
                    kind: ConfirmKind::QuitWithRunningTask,
                    ..
                })
            ),
            "must open QuitWithRunningTask confirm modal"
        );
    }

    // ── Integration: real views constructed ───────────────────────────────────

    #[test]
    fn app_new_with_real_views_constructs_ok() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let paths = AppPaths {
            home: tmp.path().join("home"),
            profiles_dir: tmp.path().join("profiles"),
            data_home: tmp.path().join("data"),
            state_home: tmp.path().join("state"),
        };
        let app = App::new(paths, Theme::detect());
        assert!(app.is_ok(), "App::new must succeed with real views");
    }

    #[test]
    fn app_select_profile_for_deps_switches_view() {
        let mut app = make_app();
        app.dispatch(Action::SelectProfileForDeps("laptop".into()));
        assert_eq!(app.active(), ViewKind::Deps);
    }

    #[test]
    fn app_select_profile_for_diff_switches_view() {
        let mut app = make_app();
        app.dispatch(Action::SelectProfileForDiff("laptop".into()));
        assert_eq!(app.active(), ViewKind::Diff);
    }

    #[test]
    fn app_delete_profile_removes_file() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        std::fs::create_dir_all(&profiles_dir).unwrap();
        let profile_path = profiles_dir.join("test.toml");
        std::fs::write(
            &profile_path,
            "schema_version = 1\n[profile]\nname = \"test\"\n",
        )
        .unwrap();
        assert!(profile_path.exists());

        let paths = AppPaths {
            home: tmp.path().join("home"),
            profiles_dir,
            data_home: tmp.path().join("data"),
            state_home: tmp.path().join("state"),
        };
        let mut app = App::new(paths, Theme::detect()).unwrap();
        // execute_confirm is called internally; call dispatch directly
        app.dispatch(Action::OpenModal(Modal::Confirm {
            prompt: "Delete?".into(),
            details: None,
            kind: ConfirmKind::DeleteProfile("test".into()),
        }));
        // Now simulate pressing Enter on the confirm modal
        app.step(key_event(crossterm::event::KeyCode::Enter))
            .unwrap();

        assert!(
            !profile_path.exists(),
            "profile file must be removed after DeleteProfile confirm"
        );
        assert!(matches!(app.status_msg(), StatusMessage::Success(_)));
    }

    #[test]
    fn app_apply_ok_refreshes_profiles_and_snapshots() {
        let mut app = make_app();
        app.force_running();

        let tx = app.test_sender();
        let apply_report = archdots_core::linker::ApplyReport {
            journal_id: archdots_core::journal::JournalId::new(),
            snapshot_id: None,
            applied: vec![],
            rolled_back: false,
        };
        tx.send(TaskMessage::Completed {
            id: TaskId(0),
            kind: BackgroundKind::Apply {
                profile: "laptop".into(),
            },
            result: TaskResult::Apply(Ok(apply_report)),
        })
        .unwrap();

        app.drain_task_messages().unwrap();
        assert!(app.is_idle());
        assert!(matches!(app.status_msg(), StatusMessage::Success(_)));
    }

    #[test]
    fn app_prune_ok_updates_status() {
        let mut app = make_app();
        app.force_running();

        let tx = app.test_sender();
        let prune_report = archdots_core::snapshot::PruneReport {
            removed: vec![archdots_core::snapshot::SnapshotId::new()],
            freed_bytes: 1024,
        };
        tx.send(TaskMessage::Completed {
            id: TaskId(0),
            kind: BackgroundKind::PruneSnapshot {
                id: archdots_core::snapshot::SnapshotId::new(),
            },
            result: TaskResult::Prune(Ok(prune_report)),
        })
        .unwrap();

        app.drain_task_messages().unwrap();
        assert!(app.is_idle());
        assert!(matches!(app.status_msg(), StatusMessage::Success(_)));
    }

    // ── DepsView integration ──────────────────────────────────────────────────

    #[test]
    fn app_select_profile_for_deps_sets_profile_and_spawns_task() {
        let mut app = make_app();
        app.dispatch(Action::SelectProfileForDeps("laptop".into()));
        assert_eq!(app.active(), ViewKind::Deps);
        assert_eq!(
            app.deps_profile(),
            Some("laptop"),
            "deps.profile must be set to 'laptop'"
        );
        // Check task should be spawned (background running)
        assert!(
            app.is_animating(),
            "SelectProfileForDeps must spawn a Check task"
        );
    }

    #[test]
    fn app_check_ok_calls_apply_report() {
        let mut app = make_app();
        app.dispatch(Action::SelectProfileForDeps("laptop".into()));
        app.force_running(); // ensure running state for drain

        let tx = app.test_sender();
        let report = archdots_core::validator::ValidationReport {
            schema_version: 1,
            profile_name: "laptop".into(),
            aur_helper: None,
            entries: vec![],
            warnings: vec![],
        };
        tx.send(TaskMessage::Completed {
            id: TaskId(0),
            kind: BackgroundKind::Check {
                profile: "laptop".into(),
                deep: false,
            },
            result: TaskResult::Check(Ok(report)),
        })
        .unwrap();

        app.drain_task_messages().unwrap();
        assert!(app.is_idle());
        assert!(
            app.deps_report_is_some(),
            "deps.report must be set after Check Ok"
        );
        assert!(matches!(app.status_msg(), StatusMessage::Success(_)));
    }

    #[test]
    fn app_check_err_calls_set_error_not_modal() {
        let mut app = make_app();
        app.dispatch(Action::SelectProfileForDeps("laptop".into()));
        app.force_running();

        let tx = app.test_sender();
        tx.send(TaskMessage::Completed {
            id: TaskId(0),
            kind: BackgroundKind::Check {
                profile: "laptop".into(),
                deep: false,
            },
            result: TaskResult::Check(Err(anyhow::anyhow!("pacman not found"))),
        })
        .unwrap();

        app.drain_task_messages().unwrap();
        assert!(app.is_idle());
        assert_eq!(
            app.deps_error(),
            Some("pacman not found"),
            "deps.error must be set on Check Err"
        );
        assert!(app.modal().is_none(), "Check failure must NOT open a modal");
        assert!(!app.deps_report_is_some());
    }

    // ── DiffView integration ──────────────────────────────────────────────────

    #[test]
    fn app_select_profile_for_diff_sets_profile_and_switches() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let profiles_dir = tmp.path().join("profiles");
        std::fs::create_dir_all(&profiles_dir).unwrap();

        let profile_content = "schema_version = 1\n\n[profile]\nname = \"rice\"\n\n[dependencies]\n\n[hooks]\n\n[[files]]\nid = \"f0\"\nsource = \"src\"\ntarget = \"~/.src\"\n";
        std::fs::write(profiles_dir.join("rice.toml"), profile_content).unwrap();
        std::fs::write(home.join("src"), "content").unwrap();

        let paths = AppPaths {
            home: home.clone(),
            profiles_dir,
            data_home: tmp.path().join("data"),
            state_home: tmp.path().join("state"),
        };
        let mut app = App::new(paths, Theme::detect()).unwrap();
        app.dispatch(Action::SelectProfileForDiff("rice".into()));

        assert_eq!(app.active(), ViewKind::Diff, "must switch to Diff view");
        assert_eq!(app.diff_profile(), Some("rice"), "diff.profile must be set");
    }

    #[test]
    fn app_select_profile_for_diff_is_synchronous_no_task() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let profiles_dir = tmp.path().join("profiles");
        std::fs::create_dir_all(&profiles_dir).unwrap();
        let profile_content =
            "schema_version = 1\n\n[profile]\nname = \"p\"\n\n[dependencies]\n\n[hooks]\n";
        std::fs::write(profiles_dir.join("p.toml"), profile_content).unwrap();

        let paths = AppPaths {
            home,
            profiles_dir,
            data_home: tmp.path().join("data"),
            state_home: tmp.path().join("state"),
        };
        let mut app = App::new(paths, Theme::detect()).unwrap();
        app.dispatch(Action::SelectProfileForDiff("p".into()));

        // set_profile is sync: no background task spawned
        assert!(
            app.is_idle(),
            "SelectProfileForDiff must NOT spawn a background task"
        );
    }

    #[test]
    fn app_apply_ok_when_active_diff_same_profile_refreshes_diff() {
        use crate::tui::views::diff::DiffItemKind;
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let profiles_dir = tmp.path().join("profiles");
        std::fs::create_dir_all(&profiles_dir).unwrap();

        let src_file = home.join("dotfile");
        std::fs::write(&src_file, "content").unwrap();
        let profile_content = "schema_version = 1\n\n[profile]\nname = \"test\"\n\n[dependencies]\n\n[hooks]\n\n[[files]]\nid = \"f0\"\nsource = \"dotfile\"\ntarget = \"~/.dotfile\"\n".to_string();
        std::fs::write(profiles_dir.join("test.toml"), &profile_content).unwrap();

        let paths = AppPaths {
            home: home.clone(),
            profiles_dir,
            data_home: tmp.path().join("data"),
            state_home: tmp.path().join("state"),
        };
        let mut app = App::new(paths, Theme::detect()).unwrap();

        // Step 1: select profile for diff — target absent, item is Missing
        app.dispatch(Action::SelectProfileForDiff("test".into()));
        assert_eq!(app.active(), ViewKind::Diff);
        assert_eq!(
            app.diff_item_kind(0),
            Some(DiffItemKind::Missing),
            "before apply: must be Missing"
        );

        // Step 2: simulate apply by creating the symlink
        let target = home.join(".dotfile");
        symlink(&src_file, &target).unwrap();

        // Step 3: deliver Apply(Ok) on the channel
        app.force_running();
        let tx = app.test_sender();
        let report = archdots_core::linker::ApplyReport {
            journal_id: archdots_core::journal::JournalId::new(),
            snapshot_id: None,
            applied: vec![],
            rolled_back: false,
        };
        tx.send(TaskMessage::Completed {
            id: TaskId(0),
            kind: BackgroundKind::Apply {
                profile: "test".into(),
            },
            result: TaskResult::Apply(Ok(report)),
        })
        .unwrap();
        app.drain_task_messages().unwrap();
        assert!(app.is_idle());

        // Step 4: diff was refreshed; item should now be Owned
        assert_eq!(
            app.diff_item_kind(0),
            Some(DiffItemKind::Owned),
            "after apply: item must be Owned (symlink is ours)"
        );
    }

    #[test]
    fn app_apply_ok_different_profile_does_not_refresh_diff() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let profiles_dir = tmp.path().join("profiles");
        std::fs::create_dir_all(&profiles_dir).unwrap();

        std::fs::write(home.join("dotfile"), "content").unwrap();
        let profile_content = "schema_version = 1\n\n[profile]\nname = \"alpha\"\n\n[dependencies]\n\n[hooks]\n\n[[files]]\nid = \"f0\"\nsource = \"dotfile\"\ntarget = \"~/.dotfile\"\n";
        std::fs::write(profiles_dir.join("alpha.toml"), profile_content).unwrap();

        let paths = AppPaths {
            home: home.clone(),
            profiles_dir,
            data_home: tmp.path().join("data"),
            state_home: tmp.path().join("state"),
        };
        let mut app = App::new(paths, Theme::detect()).unwrap();
        app.dispatch(Action::SelectProfileForDiff("alpha".into()));

        // Apply completes for a DIFFERENT profile ("beta")
        app.force_running();
        let tx = app.test_sender();
        let report = archdots_core::linker::ApplyReport {
            journal_id: archdots_core::journal::JournalId::new(),
            snapshot_id: None,
            applied: vec![],
            rolled_back: false,
        };
        tx.send(TaskMessage::Completed {
            id: TaskId(0),
            kind: BackgroundKind::Apply {
                profile: "beta".into(), // different profile
            },
            result: TaskResult::Apply(Ok(report)),
        })
        .unwrap();
        app.drain_task_messages().unwrap();

        // diff.profile should still be "alpha", unchanged
        assert_eq!(app.diff_profile(), Some("alpha"));
    }

    #[test]
    #[ignore = "requires a display/clipboard — run manually or on a system with a display"]
    fn app_copy_to_clipboard_ok_sets_status() {
        let mut app = make_app();
        app.dispatch(Action::CopyToClipboard("sudo pacman -S test".into()));
        // If clipboard is available, status should say "Copied"
        // If not, a modal is opened — either way no panic
        let _ = app.status_msg();
    }

    #[test]
    fn app_copy_to_clipboard_clipboard_unavailable_opens_modal_or_succeeds() {
        // This test is environment-dependent. We just verify no panic occurs.
        let mut app = make_app();
        app.dispatch(Action::CopyToClipboard("sudo pacman -S hyprland".into()));
        // Regardless of clipboard availability: either status is set or modal is opened.
        let has_status = matches!(app.status_msg(), StatusMessage::Success(_));
        let has_modal = app.modal().is_some();
        assert!(
            has_status || has_modal,
            "CopyToClipboard must either set status or open modal"
        );
    }

    // ── Last-apply ────────────────────────────────────────────────────────────

    #[test]
    fn app_last_apply_none_without_journal() {
        // fake_paths points to /tmp/test-home which has no journal on disk.
        let app = make_app();
        assert!(app.last_apply_profile().is_none());
    }

    #[test]
    fn app_last_apply_reads_most_recent_from_journal() {
        use archdots_core::journal::{
            Journal, JournalAction, JournalEntry, JournalId, JournalStatus,
        };
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let paths = AppPaths {
            home: tmp.path().join("home"),
            profiles_dir: tmp.path().join("profiles"),
            data_home: tmp.path().join("data"),
            state_home: tmp.path().join("state"),
        };
        std::fs::create_dir_all(&paths.state_home).unwrap();

        let journal = Journal::open(&paths.state_home).unwrap();
        let entry = JournalEntry {
            schema_version: 1,
            id: JournalId::new(),
            ts: OffsetDateTime::now_utc(),
            profile: "my-rice".to_string(),
            action: JournalAction::Apply,
            snapshot_id: None,
            links: vec![],
            status: JournalStatus::Success,
            error: None,
            supersedes: None,
        };
        journal.append(&entry).unwrap();

        let app = App::new(paths, Theme::detect()).unwrap();
        assert_eq!(app.last_apply_profile(), Some("my-rice"));
    }

    #[test]
    fn app_last_apply_ignores_non_success_entries() {
        use archdots_core::journal::{
            Journal, JournalAction, JournalEntry, JournalId, JournalStatus,
        };
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let paths = AppPaths {
            home: tmp.path().join("home"),
            profiles_dir: tmp.path().join("profiles"),
            data_home: tmp.path().join("data"),
            state_home: tmp.path().join("state"),
        };
        std::fs::create_dir_all(&paths.state_home).unwrap();

        let journal = Journal::open(&paths.state_home).unwrap();
        let entry = JournalEntry {
            schema_version: 1,
            id: JournalId::new(),
            ts: OffsetDateTime::now_utc(),
            profile: "rice".to_string(),
            action: JournalAction::Apply,
            snapshot_id: None,
            links: vec![],
            status: JournalStatus::Failed,
            error: Some("disk full".to_string()),
            supersedes: None,
        };
        journal.append(&entry).unwrap();

        let app = App::new(paths, Theme::detect()).unwrap();
        assert!(
            app.last_apply_profile().is_none(),
            "failed entries must be ignored"
        );
    }

    #[test]
    fn app_apply_ok_refreshes_last_apply_badge() {
        use archdots_core::journal::{
            Journal, JournalAction, JournalEntry, JournalId, JournalStatus,
        };
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let paths = AppPaths {
            home: tmp.path().join("home"),
            profiles_dir: tmp.path().join("profiles"),
            data_home: tmp.path().join("data"),
            state_home: tmp.path().join("state"),
        };
        std::fs::create_dir_all(&paths.state_home).unwrap();

        let journal = Journal::open(&paths.state_home).unwrap();
        // Old entry — present at App::new time.
        let old_entry = JournalEntry {
            schema_version: 1,
            id: JournalId::new(),
            ts: OffsetDateTime::now_utc() - time::Duration::hours(1),
            profile: "old-rice".to_string(),
            action: JournalAction::Apply,
            snapshot_id: None,
            links: vec![],
            status: JournalStatus::Success,
            error: None,
            supersedes: None,
        };
        journal.append(&old_entry).unwrap();

        let mut app = App::new(paths.clone(), Theme::detect()).unwrap();
        assert_eq!(
            app.last_apply_profile(),
            Some("old-rice"),
            "must read initial journal entry"
        );

        // Write a newer entry (simulates what the worker would have written).
        let new_entry = JournalEntry {
            schema_version: 1,
            id: JournalId::new(),
            ts: OffsetDateTime::now_utc(),
            profile: "new-rice".to_string(),
            action: JournalAction::Apply,
            snapshot_id: None,
            links: vec![],
            status: JournalStatus::Success,
            error: None,
            supersedes: None,
        };
        journal.append(&new_entry).unwrap();

        // Deliver Apply(Ok) on the channel.
        app.force_running();
        let tx = app.test_sender();
        let report = archdots_core::linker::ApplyReport {
            journal_id: archdots_core::journal::JournalId::new(),
            snapshot_id: None,
            applied: vec![],
            rolled_back: false,
        };
        tx.send(TaskMessage::Completed {
            id: TaskId(0),
            kind: BackgroundKind::Apply {
                profile: "new-rice".into(),
            },
            result: TaskResult::Apply(Ok(report)),
        })
        .unwrap();

        app.drain_task_messages().unwrap();
        assert!(app.is_idle());

        // last_apply must now reflect the newer journal entry.
        assert_eq!(
            app.last_apply_profile(),
            Some("new-rice"),
            "last_apply must be refreshed after Apply(Ok)"
        );
    }

    // ── format_relative_time ─────────────────────────────────────────────────

    #[test]
    fn format_relative_time_just_now() {
        let ts = OffsetDateTime::now_utc();
        assert_eq!(format_relative_time(ts), "just now");
    }

    #[test]
    fn format_relative_time_minutes_ago() {
        let ts = OffsetDateTime::now_utc() - time::Duration::minutes(5);
        assert_eq!(format_relative_time(ts), "5m ago");
    }

    #[test]
    fn format_relative_time_hours_ago() {
        let ts = OffsetDateTime::now_utc() - time::Duration::hours(3);
        assert_eq!(format_relative_time(ts), "3h ago");
    }

    #[test]
    fn format_relative_time_days_ago() {
        let ts = OffsetDateTime::now_utc() - time::Duration::days(2);
        assert_eq!(format_relative_time(ts), "2d ago");
    }

    // Requires a real TTY; skip in CI / non-TTY test runners where
    // crossterm::event::poll errors immediately and Tick is never emitted.
    #[test]
    #[ignore = "requires a TTY — run manually with `cargo test -- --ignored`"]
    fn event_loop_next_returns_tick_when_no_input() {
        use crate::tui::events::EventLoop;
        let mut el = EventLoop::new();
        // If no input arrives within ~70 ms (> 50 ms tick_rate), we get Tick.
        // This is a best-effort test; it may rarely return Input on a busy TTY.
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        let mut got_tick = false;
        while std::time::Instant::now() < deadline {
            if let Ok(Event::Tick) = el.next() {
                got_tick = true;
                break;
            }
        }
        assert!(got_tick, "EventLoop must emit at least one Tick in 200 ms");
    }
}
