use byte_unit::{Byte, UnitType};
use crossterm::event::{KeyCode, KeyEvent};
use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use archdots_core::snapshot::{Snapshot, SnapshotId, SnapshotManager, SnapshotSummary};

use crate::tui::{
    action::{Action, ConfirmKind},
    app::AppPaths,
    theme::Theme,
    ui::{Modal, StatusMessage},
    views::{View, ViewCtx},
};

// ─── SearchState ──────────────────────────────────────────────────────────────

struct SearchState {
    query: String,
    cursor: usize,
    filtered: Vec<usize>,
}

// ─── SnapshotsView ────────────────────────────────────────────────────────────

pub struct SnapshotsView {
    summaries: Vec<SnapshotSummary>,
    cursor: usize,
    /// Lazy-loaded full snapshot for the currently selected entry.
    detail: Option<Snapshot>,
    search: Option<SearchState>,
    list_error: Option<String>,
}

impl SnapshotsView {
    pub fn new(paths: &AppPaths) -> Self {
        let mut view = Self {
            summaries: Vec::new(),
            cursor: 0,
            detail: None,
            search: None,
            list_error: None,
        };
        view.refresh(paths);
        view
    }

    pub fn refresh(&mut self, paths: &AppPaths) {
        match SnapshotManager::open(&paths.data_home) {
            Ok(mgr) => match mgr.list() {
                Ok(summaries) => {
                    self.summaries = summaries;
                    self.detail = None;
                    if !self.summaries.is_empty() && self.cursor >= self.summaries.len() {
                        self.cursor = self.summaries.len() - 1;
                    }
                    if self.search.is_some() {
                        let query = self.search.as_ref().unwrap().query.clone();
                        let filtered = fuzzy_filter_snapshots(&self.summaries, &query);
                        self.search.as_mut().unwrap().filtered = filtered;
                        self.cursor = 0;
                    }
                }
                Err(e) => {
                    self.list_error = Some(e.to_string());
                }
            },
            Err(e) => {
                self.list_error = Some(e.to_string());
            }
        }
    }

    pub fn selected_id(&self) -> Option<&SnapshotId> {
        let visible = self.visible_indices();
        let idx = *visible.get(self.cursor)?;
        self.summaries.get(idx).map(|s| &s.id)
    }

    fn load_detail(&mut self, paths: &AppPaths) {
        let visible = self.visible_indices();
        let Some(&idx) = visible.get(self.cursor) else {
            return;
        };
        let Some(summary) = self.summaries.get(idx) else {
            return;
        };
        let id = summary.id.clone();
        match SnapshotManager::open(&paths.data_home) {
            Ok(mgr) => match mgr.get(&id) {
                Ok(snap) => self.detail = Some(snap),
                Err(e) => tracing::warn!("failed to load snapshot detail: {e}"),
            },
            Err(e) => tracing::warn!("failed to open snapshot manager for detail: {e}"),
        }
    }

    fn visible_indices(&self) -> Vec<usize> {
        if let Some(ref search) = self.search {
            search.filtered.clone()
        } else {
            (0..self.summaries.len()).collect()
        }
    }

    fn update_search_filter(&mut self) {
        let query = match &self.search {
            Some(s) => s.query.clone(),
            None => return,
        };
        let filtered = fuzzy_filter_snapshots(&self.summaries, &query);
        if let Some(ref mut s) = self.search {
            s.filtered = filtered;
        }
        self.cursor = 0;
        self.detail = None;
    }

    fn handle_search_key(&mut self, key: KeyEvent, _ctx: &ViewCtx<'_>) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.search = None;
                Action::None
            }
            KeyCode::Backspace => {
                if let Some(ref mut s) = self.search {
                    s.query.pop();
                    s.cursor = s.query.len();
                }
                self.update_search_filter();
                Action::None
            }
            KeyCode::Char(c) => {
                if let Some(ref mut s) = self.search {
                    s.query.push(c);
                    s.cursor = s.query.len();
                }
                self.update_search_filter();
                Action::None
            }
            _ => Action::None,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_normal_key(&mut self, key: KeyEvent, ctx: &ViewCtx<'_>) -> Action {
        let visible = self.visible_indices();
        let len = visible.len();

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if len > 0 {
                    self.cursor = (self.cursor + 1).min(len - 1);
                    self.detail = None;
                    self.load_detail(ctx.paths);
                }
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if len > 0 {
                    let prev = self.cursor;
                    self.cursor = self.cursor.saturating_sub(1);
                    if self.cursor != prev {
                        self.detail = None;
                        self.load_detail(ctx.paths);
                    }
                }
                Action::None
            }
            KeyCode::Char('g') => {
                if len > 0 {
                    self.cursor = 0;
                    self.detail = None;
                    self.load_detail(ctx.paths);
                }
                Action::None
            }
            KeyCode::Char('G') => {
                if len > 0 {
                    self.cursor = len - 1;
                    self.detail = None;
                    self.load_detail(ctx.paths);
                }
                Action::None
            }
            KeyCode::Char('r') => {
                if ctx.busy {
                    return Action::SetStatus(StatusMessage::Info("operation in progress".into()));
                }
                let Some(&idx) = visible.get(self.cursor) else {
                    return Action::None;
                };
                let Some(summary) = self.summaries.get(idx) else {
                    return Action::None;
                };
                let short = short_id(&summary.id);
                let profile = summary.profile.clone();
                let ts = summary.created_at.clone();
                let id = summary.id.clone();
                Action::OpenModal(Modal::Confirm {
                    prompt: format!("Restore snapshot {short} ({profile})?"),
                    details: Some(format!(
                        "This restores the filesystem state captured at {ts}. \
                         A new snapshot of the current state will be taken first."
                    )),
                    kind: ConfirmKind::RollbackToSnapshot(id),
                })
            }
            KeyCode::Char('x') => {
                if ctx.busy {
                    return Action::SetStatus(StatusMessage::Info("operation in progress".into()));
                }
                let Some(&idx) = visible.get(self.cursor) else {
                    return Action::None;
                };
                let Some(summary) = self.summaries.get(idx) else {
                    return Action::None;
                };
                let short = short_id(&summary.id);
                let id = summary.id.clone();
                let size_str = self.detail.as_ref().map_or_else(
                    || "unknown size".to_string(),
                    |d| human_size_path(&d.archive_path),
                );
                Action::OpenModal(Modal::Confirm {
                    prompt: format!("Prune snapshot {short}?"),
                    details: Some(format!(
                        "This will permanently delete the snapshot. \
                         {size_str} of disk space will be freed."
                    )),
                    kind: ConfirmKind::PruneSnapshot(id),
                })
            }
            KeyCode::Char('i') => {
                // Load detail synchronously if not available (small I/O).
                if self.detail.is_none() {
                    self.load_detail(ctx.paths);
                }
                if let Some(ref detail) = self.detail {
                    let title = format!("Snapshot {}", short_id(&detail.id));
                    let body = manifest_to_string(detail);
                    Action::OpenModal(Modal::ScrollableInfo {
                        title,
                        body,
                        scroll: 0,
                    })
                } else {
                    Action::SetStatus(StatusMessage::Info("Loading snapshot detail…".into()))
                }
            }
            KeyCode::Char('/') => {
                self.search = Some(SearchState {
                    query: String::new(),
                    cursor: 0,
                    filtered: (0..self.summaries.len()).collect(),
                });
                Action::None
            }
            _ => Action::None,
        }
    }

    // ─── Test helpers ─────────────────────────────────────────────────────────

    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    #[cfg(test)]
    pub fn summary_count(&self) -> usize {
        self.summaries.len()
    }

    #[cfg(test)]
    pub fn has_detail(&self) -> bool {
        self.detail.is_some()
    }

    #[cfg(test)]
    pub fn is_searching(&self) -> bool {
        self.search.is_some()
    }

    #[cfg(test)]
    pub fn search_filtered(&self) -> Option<&[usize]> {
        self.search.as_ref().map(|s| s.filtered.as_slice())
    }
}

// ─── View impl ────────────────────────────────────────────────────────────────

impl View for SnapshotsView {
    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let (left_area, right_area) = if area.width < 60 {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);
            (chunks[0], chunks[1])
        } else {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(area);
            (chunks[0], chunks[1])
        };

        render_list_pane(self, frame, left_area, theme);
        render_detail_pane(self, frame, right_area, theme);
    }

    fn handle_event(&mut self, ev: &crossterm::event::Event, ctx: &ViewCtx<'_>) -> Action {
        let crossterm::event::Event::Key(key) = ev else {
            return Action::None;
        };
        let key = *key;
        if self.search.is_some() {
            return self.handle_search_key(key, ctx);
        }
        self.handle_normal_key(key, ctx)
    }

    fn on_focus(&mut self, ctx: &ViewCtx<'_>) {
        if self.detail.is_none() && !self.summaries.is_empty() {
            self.load_detail(ctx.paths);
        }
    }

    fn footer_hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("r", "restore"),
            ("x", "prune"),
            ("i", "info"),
            ("/", "search"),
        ]
    }
}

// ─── Rendering helpers ────────────────────────────────────────────────────────

fn render_list_pane(view: &SnapshotsView, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(" Snapshots ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible = view.visible_indices();
    let mut lines: Vec<Line<'_>> = Vec::new();

    // Header row
    lines.push(Line::from(Span::styled(
        "id         created    profile",
        Style::default().add_modifier(Modifier::BOLD),
    )));

    for (vpos, &sidx) in visible.iter().enumerate() {
        if let Some(s) = view.summaries.get(sidx) {
            let marker = if vpos == view.cursor {
                if theme.use_unicode {
                    "▶ "
                } else {
                    "> "
                }
            } else {
                "  "
            };
            let created_short = s.created_at.get(5..16).unwrap_or(&s.created_at);
            let row = format!(
                "{marker}{short}  {created}  {profile}",
                short = short_id(&s.id),
                created = created_short,
                profile = &s.profile,
            );
            let style = if vpos == view.cursor {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(row, style)));
        }
    }

    if visible.is_empty() {
        let msg = view.list_error.as_deref().unwrap_or("No snapshots");
        lines.push(Line::from(Span::styled(
            msg.to_string(),
            Style::default().fg(theme.muted),
        )));
    }

    if let Some(ref search) = view.search {
        lines.push(Line::from(Span::styled(
            format!("/{}_", search.query),
            Style::default().fg(theme.accent),
        )));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_detail_pane(view: &SnapshotsView, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'_>> = Vec::new();
    let visible = view.visible_indices();

    if let Some(&sidx) = visible.get(view.cursor) {
        if let Some(s) = view.summaries.get(sidx) {
            lines.push(Line::from(Span::styled(
                short_id(&s.id),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from("─────"));
            lines.push(Line::from(format!("Created  : {}", s.created_at)));
            lines.push(Line::from(format!("Profile  : {}", s.profile)));
            lines.push(Line::from(format!("Trigger  : {:?}", s.trigger)));
            lines.push(Line::from(format!("Targets  : {}", s.target_count)));

            if let Some(ref detail) = view.detail {
                lines.push(Line::from(format!(
                    "Size     : {}",
                    human_size_path(&detail.archive_path)
                )));
                lines.push(Line::from(format!(
                    "Host     : {} ({} @ {})",
                    detail.manifest.host.hostname,
                    detail.manifest.host.user,
                    detail.manifest.host.home,
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "(press i for full info)",
                    Style::default().fg(theme.muted),
                )));
            }

            lines.push(Line::from(""));
            lines.push(Line::from("[r] restore  [x] prune  [i] info"));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "No snapshot selected",
            Style::default().fg(theme.muted),
        )));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn short_id(id: &SnapshotId) -> String {
    id.to_string().chars().take(8).collect()
}

fn human_size_path(path: &std::path::Path) -> String {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    format!(
        "{}",
        Byte::from_u64(bytes).get_appropriate_unit(UnitType::Binary)
    )
}

fn manifest_to_string(snap: &Snapshot) -> String {
    let m = &snap.manifest;
    let targets: String = m
        .targets
        .iter()
        .enumerate()
        .map(|(i, t)| format!("  {}: {} ({:?})", i + 1, t.path, t.kind))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Snapshot : {id}\n\
         Created  : {created}\n\
         Profile  : {profile}\n\
         Trigger  : {trigger:?}\n\
         Host     : {hostname} ({user} @ {home})\n\
         Targets  : {count}\n\
         \n\
         Files:\n{targets}",
        id = m.id,
        created = m.created_at,
        profile = m.profile,
        trigger = m.trigger,
        hostname = m.host.hostname,
        user = m.host.user,
        home = m.host.home,
        count = m.targets.len(),
    )
}

fn fuzzy_filter_snapshots(summaries: &[SnapshotSummary], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..summaries.len()).collect();
    }
    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, usize)> = summaries
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            matcher
                .fuzzy_match(&s.profile, query)
                .map(|score| (score, i))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, i)| i).collect()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use archdots_core::snapshot::{CreateRequest, SnapshotTrigger};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_paths(tmp: &TempDir) -> AppPaths {
        AppPaths {
            home: tmp.path().to_path_buf(),
            profiles_dir: tmp.path().join("profiles"),
            data_home: tmp.path().to_path_buf(),
            state_home: tmp.path().to_path_buf(),
        }
    }

    static THEME: std::sync::OnceLock<Theme> = std::sync::OnceLock::new();

    fn test_theme() -> &'static Theme {
        THEME.get_or_init(Theme::default)
    }

    fn make_ctx(paths: &AppPaths) -> ViewCtx<'_> {
        ViewCtx {
            paths,
            theme: test_theme(),
            busy: false,
        }
    }

    fn key(code: KeyCode) -> crossterm::event::Event {
        use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        crossterm::event::Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn add_snapshot(mgr: &SnapshotManager, profile: &str) -> archdots_core::snapshot::Snapshot {
        mgr.create(CreateRequest {
            profile,
            trigger: SnapshotTrigger::Manual,
            targets: &[],
        })
        .unwrap()
    }

    fn make_mgr(data_home: &std::path::Path) -> SnapshotManager {
        SnapshotManager::open(data_home).unwrap()
    }

    // ── 1. new() loads summaries ──────────────────────────────────────────────

    #[test]
    fn snapshots_new_loads_summaries() {
        let tmp = TempDir::new().unwrap();
        let paths = make_paths(&tmp);
        let mgr = make_mgr(&paths.data_home);
        add_snapshot(&mgr, "laptop");
        add_snapshot(&mgr, "desktop");

        let view = SnapshotsView::new(&paths);
        assert_eq!(view.summary_count(), 2);
    }

    // ── 2. cursor initial = 0 ────────────────────────────────────────────────

    #[test]
    fn snapshots_cursor_initial_zero() {
        let tmp = TempDir::new().unwrap();
        let paths = make_paths(&tmp);
        let view = SnapshotsView::new(&paths);
        assert_eq!(view.cursor(), 0);
    }

    // ── 3. detail is not loaded at construction ───────────────────────────────

    #[test]
    fn snapshots_detail_not_loaded_at_new() {
        let tmp = TempDir::new().unwrap();
        let paths = make_paths(&tmp);
        let mgr = make_mgr(&paths.data_home);
        add_snapshot(&mgr, "test");

        let view = SnapshotsView::new(&paths);
        assert!(!view.has_detail(), "detail must be None at construction");
    }

    // ── 4. j/k moves cursor and loads detail ─────────────────────────────────

    #[test]
    fn snapshots_jk_moves_cursor_and_loads_detail() {
        let tmp = TempDir::new().unwrap();
        let paths = make_paths(&tmp);
        let mgr = make_mgr(&paths.data_home);
        add_snapshot(&mgr, "first");
        std::thread::sleep(std::time::Duration::from_millis(2));
        add_snapshot(&mgr, "second");

        let mut view = SnapshotsView::new(&paths);
        let ctx = make_ctx(&paths);
        assert_eq!(view.cursor(), 0);
        assert!(!view.has_detail());

        view.handle_event(&key(KeyCode::Char('j')), &ctx);
        assert_eq!(view.cursor(), 1);
        assert!(view.has_detail(), "j must trigger detail load");

        view.handle_event(&key(KeyCode::Char('k')), &ctx);
        assert_eq!(view.cursor(), 0);
        assert!(view.has_detail(), "k must trigger detail load");
    }

    // ── 5. r → ConfirmKind::RollbackToSnapshot ───────────────────────────────

    #[test]
    fn snapshots_r_opens_restore_confirm() {
        let tmp = TempDir::new().unwrap();
        let paths = make_paths(&tmp);
        let mgr = make_mgr(&paths.data_home);
        add_snapshot(&mgr, "laptop");

        let mut view = SnapshotsView::new(&paths);
        let ctx = make_ctx(&paths);
        let action = view.handle_event(&key(KeyCode::Char('r')), &ctx);
        assert!(
            matches!(
                action,
                Action::OpenModal(Modal::Confirm {
                    kind: ConfirmKind::RollbackToSnapshot(_),
                    ..
                })
            ),
            "r must open RollbackToSnapshot confirm"
        );
    }

    // ── 6. x → ConfirmKind::PruneSnapshot ────────────────────────────────────

    #[test]
    fn snapshots_x_opens_prune_confirm() {
        let tmp = TempDir::new().unwrap();
        let paths = make_paths(&tmp);
        let mgr = make_mgr(&paths.data_home);
        add_snapshot(&mgr, "desktop");

        let mut view = SnapshotsView::new(&paths);
        let ctx = make_ctx(&paths);
        let action = view.handle_event(&key(KeyCode::Char('x')), &ctx);
        assert!(
            matches!(
                action,
                Action::OpenModal(Modal::Confirm {
                    kind: ConfirmKind::PruneSnapshot(_),
                    ..
                })
            ),
            "x must open PruneSnapshot confirm"
        );
    }

    // ── 7. i with detail loaded → Modal::ScrollableInfo ──────────────────────

    #[test]
    fn snapshots_i_with_detail_opens_info() {
        let tmp = TempDir::new().unwrap();
        let paths = make_paths(&tmp);
        let mgr = make_mgr(&paths.data_home);
        add_snapshot(&mgr, "laptop");

        let mut view = SnapshotsView::new(&paths);
        let ctx = make_ctx(&paths);
        // Force load detail via j (detail becomes some after cursor move)
        // Actually for a single snapshot j won't move, so use load_detail directly via 'i'.
        let action = view.handle_event(&key(KeyCode::Char('i')), &ctx);
        assert!(
            matches!(action, Action::OpenModal(Modal::ScrollableInfo { .. })),
            "i must open ScrollableInfo modal, got {action:?}"
        );
    }

    // ── 8. i without prior detail load → loads then opens modal ──────────────

    #[test]
    fn snapshots_i_loads_detail_if_missing() {
        let tmp = TempDir::new().unwrap();
        let paths = make_paths(&tmp);
        let mgr = make_mgr(&paths.data_home);
        add_snapshot(&mgr, "test");

        let mut view = SnapshotsView::new(&paths);
        assert!(!view.has_detail());

        let ctx = make_ctx(&paths);
        let action = view.handle_event(&key(KeyCode::Char('i')), &ctx);
        // After 'i', detail should be loaded and modal opened
        assert!(view.has_detail(), "i must load detail");
        assert!(
            matches!(action, Action::OpenModal(Modal::ScrollableInfo { .. })),
            "i must open ScrollableInfo"
        );
    }

    // ── 9. / activates search, filters by profile name ───────────────────────

    #[test]
    fn snapshots_search_filters_by_profile() {
        let tmp = TempDir::new().unwrap();
        let paths = make_paths(&tmp);
        let mgr = make_mgr(&paths.data_home);
        add_snapshot(&mgr, "laptop");
        std::thread::sleep(std::time::Duration::from_millis(2));
        add_snapshot(&mgr, "desktop");

        let mut view = SnapshotsView::new(&paths);
        let ctx = make_ctx(&paths);
        assert!(!view.is_searching());

        view.handle_event(&key(KeyCode::Char('/')), &ctx);
        assert!(view.is_searching());

        // Type "lap" — should match "laptop" but not "desktop"
        view.handle_event(&key(KeyCode::Char('l')), &ctx);
        view.handle_event(&key(KeyCode::Char('a')), &ctx);
        view.handle_event(&key(KeyCode::Char('p')), &ctx);

        let filtered = view.search_filtered().unwrap();
        assert!(
            !filtered.is_empty(),
            "searching 'lap' must match at least one snapshot"
        );
        // Check "desktop" snapshot not in filtered
        let all_summaries = &view.summaries;
        for &idx in filtered {
            assert_eq!(
                &all_summaries[idx].profile, "laptop",
                "filtered must contain only laptop"
            );
        }
    }

    // ── 10. empty summaries → cursor stays 0, no actions ─────────────────────

    #[test]
    fn snapshots_empty_no_actions() {
        let tmp = TempDir::new().unwrap();
        let paths = make_paths(&tmp);
        let mut view = SnapshotsView::new(&paths);
        let ctx = make_ctx(&paths);

        assert_eq!(view.cursor(), 0);
        let a1 = view.handle_event(&key(KeyCode::Char('r')), &ctx);
        let a2 = view.handle_event(&key(KeyCode::Char('x')), &ctx);
        let a3 = view.handle_event(&key(KeyCode::Char('j')), &ctx);
        assert!(matches!(a1, Action::None));
        assert!(matches!(a2, Action::None));
        assert!(matches!(a3, Action::None));
        assert_eq!(view.cursor(), 0);
    }

    // ── 11. short_id returns first 8 chars of ULID ───────────────────────────

    #[test]
    fn snapshots_short_id_is_8_chars() {
        let id = SnapshotId::new();
        let s = short_id(&id);
        assert_eq!(s.len(), 8, "short_id must be exactly 8 chars");
        // Verify it matches the start of the full id
        assert!(id.to_string().starts_with(&s));
    }

    // ── 12. action_blocked_during_busy ───────────────────────────────────────

    #[test]
    fn snapshots_busy_blocks_mutating_actions() {
        let tmp = TempDir::new().unwrap();
        let paths = make_paths(&tmp);
        let mgr = make_mgr(&paths.data_home);
        add_snapshot(&mgr, "laptop");

        let mut view = SnapshotsView::new(&paths);
        let busy_ctx = ViewCtx {
            paths: &paths,
            theme: test_theme(),
            busy: true,
        };
        let action = view.handle_event(&key(KeyCode::Char('r')), &busy_ctx);
        assert!(
            matches!(action, Action::SetStatus(_)),
            "r while busy must emit SetStatus"
        );
        let action = view.handle_event(&key(KeyCode::Char('x')), &busy_ctx);
        assert!(
            matches!(action, Action::SetStatus(_)),
            "x while busy must emit SetStatus"
        );
    }
}
