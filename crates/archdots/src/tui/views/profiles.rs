use crossterm::event::{KeyCode, KeyEvent};
use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use archdots_core::profile::Profile;

use crate::tui::{
    action::{Action, ConfirmKind},
    app::AppPaths,
    theme::Theme,
    ui::{Modal, StatusMessage},
    views::{View, ViewCtx},
};

// ─── Data types ───────────────────────────────────────────────────────────────

pub struct ProfileSummary {
    pub file_count: usize,
    pub pacman_deps: usize,
    pub aur_deps: usize,
    pub optional_deps: usize,
    pub description: Option<String>,
    pub author: Option<String>,
    pub tags: Vec<String>,
    pub wm: Option<String>,
}

struct ProfileItem {
    name: String,
    summary: Option<ProfileSummary>,
}

struct SearchState {
    query: String,
    /// Text-input cursor position (index into `query` chars).
    cursor: usize,
    /// Indices into `ProfilesView::profiles` that match the current query.
    filtered: Vec<usize>,
}

// ─── ProfilesView ─────────────────────────────────────────────────────────────

pub struct ProfilesView {
    profiles: Vec<ProfileItem>,
    /// Visual cursor: index into `visible_indices()`.
    cursor: usize,
    search: Option<SearchState>,
    list_error: Option<String>,
}

impl ProfilesView {
    pub fn new(paths: &AppPaths) -> Self {
        let mut view = Self {
            profiles: Vec::new(),
            cursor: 0,
            search: None,
            list_error: None,
        };
        view.refresh(paths);
        view
    }

    pub fn refresh(&mut self, paths: &AppPaths) {
        match Profile::list_names(&paths.profiles_dir) {
            Ok(names) => {
                self.profiles = names
                    .into_iter()
                    .map(|name| ProfileItem {
                        name,
                        summary: None,
                    })
                    .collect();
                if !self.profiles.is_empty() && self.cursor >= self.profiles.len() {
                    self.cursor = self.profiles.len() - 1;
                }
                // Recalculate filtered if search is active.
                if self.search.is_some() {
                    let query = self.search.as_ref().unwrap().query.clone();
                    let filtered = fuzzy_filter(&self.profiles, &query);
                    self.search.as_mut().unwrap().filtered = filtered;
                    self.cursor = 0;
                }
            }
            Err(e) => {
                self.list_error = Some(e.to_string());
                self.profiles.clear();
            }
        }
    }

    pub fn selected_name(&self) -> Option<&str> {
        let visible = self.visible_indices();
        let idx = *visible.get(self.cursor)?;
        self.profiles.get(idx).map(|p| p.name.as_str())
    }

    fn load_summary(&mut self, paths: &AppPaths, idx: usize) {
        if self.profiles.get(idx).is_none() {
            return;
        }
        if self.profiles[idx].summary.is_some() {
            return;
        }
        let name = self.profiles[idx].name.clone();
        let path = paths.profiles_dir.join(format!("{name}.toml"));
        match Profile::load_from_file(&path) {
            Ok(p) => {
                self.profiles[idx].summary = Some(ProfileSummary {
                    file_count: p.files.len(),
                    pacman_deps: p.dependencies.pacman.len(),
                    aur_deps: p.dependencies.aur.len(),
                    optional_deps: p.dependencies.optional_pacman.len(),
                    description: p.profile.description.clone(),
                    author: p.profile.author.clone(),
                    tags: p.profile.tags.clone(),
                    wm: p.wm.as_ref().map(|w| format!("{}", w.kind)),
                });
            }
            Err(e) => {
                tracing::warn!(profile = %name, "failed to load profile summary: {e}");
            }
        }
    }

    fn visible_indices(&self) -> Vec<usize> {
        if let Some(ref search) = self.search {
            search.filtered.clone()
        } else {
            (0..self.profiles.len()).collect()
        }
    }

    fn update_search_filter(&mut self) {
        let query = match &self.search {
            Some(s) => s.query.clone(),
            None => return,
        };
        let filtered = fuzzy_filter(&self.profiles, &query);
        if let Some(ref mut s) = self.search {
            s.filtered = filtered;
        }
        self.cursor = 0;
    }

    fn handle_search_key(&mut self, key: KeyEvent, _ctx: &ViewCtx<'_>) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.search = None;
                Action::None
            }
            KeyCode::Enter => {
                // Commit: exit search input but keep filtered results.
                self.search = None;
                Action::None
            }
            KeyCode::Backspace => {
                if let Some(ref mut search) = self.search {
                    search.query.pop();
                    search.cursor = search.query.len();
                }
                self.update_search_filter();
                Action::None
            }
            KeyCode::Char(c) => {
                if let Some(ref mut search) = self.search {
                    search.query.push(c);
                    search.cursor = search.query.len();
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
                    let new_cursor = (self.cursor + 1).min(len - 1);
                    self.cursor = new_cursor;
                    if let Some(&idx) = visible.get(self.cursor) {
                        self.load_summary(ctx.paths, idx);
                    }
                }
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if len > 0 {
                    self.cursor = self.cursor.saturating_sub(1);
                    if let Some(&idx) = visible.get(self.cursor) {
                        self.load_summary(ctx.paths, idx);
                    }
                }
                Action::None
            }
            KeyCode::Char('g') => {
                if len > 0 {
                    self.cursor = 0;
                    if let Some(&idx) = visible.first() {
                        self.load_summary(ctx.paths, idx);
                    }
                }
                Action::None
            }
            KeyCode::Char('G') => {
                if len > 0 {
                    self.cursor = len - 1;
                    if let Some(&idx) = visible.get(len - 1) {
                        self.load_summary(ctx.paths, idx);
                    }
                }
                Action::None
            }
            KeyCode::Enter => {
                if ctx.busy {
                    return Action::SetStatus(StatusMessage::Info("operation in progress".into()));
                }
                let Some(name) = self.selected_name() else {
                    return Action::None;
                };
                let name = name.to_string();
                let file_count = visible
                    .get(self.cursor)
                    .and_then(|&idx| self.profiles.get(idx))
                    .and_then(|item| item.summary.as_ref())
                    .map_or_else(|| "?".to_string(), |s| s.file_count.to_string());
                Action::OpenModal(Modal::Confirm {
                    prompt: format!("Apply '{name}'?"),
                    details: Some(format!(
                        "This will create or replace {file_count} symlink(s). \
                         Snapshot will be taken first."
                    )),
                    kind: ConfirmKind::ApplyProfile(name),
                })
            }
            KeyCode::Char('d') => {
                let Some(name) = self.selected_name() else {
                    return Action::None;
                };
                Action::SelectProfileForDiff(name.to_string())
            }
            KeyCode::Char('r') => {
                if ctx.busy {
                    return Action::SetStatus(StatusMessage::Info("operation in progress".into()));
                }
                let Some(name) = self.selected_name() else {
                    return Action::None;
                };
                let name = name.to_string();
                Action::OpenModal(Modal::Confirm {
                    prompt: format!("Rollback last apply of '{name}'?"),
                    details: None,
                    kind: ConfirmKind::RollbackProfile(name),
                })
            }
            KeyCode::Char('c') => {
                let Some(name) = self.selected_name() else {
                    return Action::None;
                };
                Action::SelectProfileForDeps(name.to_string())
            }
            KeyCode::Char('x') => {
                if ctx.busy {
                    return Action::SetStatus(StatusMessage::Info("operation in progress".into()));
                }
                let Some(name) = self.selected_name() else {
                    return Action::None;
                };
                let name = name.to_string();
                Action::OpenModal(Modal::Confirm {
                    prompt: format!("Delete profile '{name}'?"),
                    details: Some("Profile file will be deleted. Snapshots are kept.".into()),
                    kind: ConfirmKind::DeleteProfile(name),
                })
            }
            KeyCode::Char('/') => {
                self.search = Some(SearchState {
                    query: String::new(),
                    cursor: 0,
                    filtered: (0..self.profiles.len()).collect(),
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
    pub fn is_searching(&self) -> bool {
        self.search.is_some()
    }

    #[cfg(test)]
    pub fn search_query(&self) -> Option<&str> {
        self.search.as_ref().map(|s| s.query.as_str())
    }

    #[cfg(test)]
    pub fn search_filtered(&self) -> Option<&[usize]> {
        self.search.as_ref().map(|s| s.filtered.as_slice())
    }

    #[cfg(test)]
    pub fn profile_count(&self) -> usize {
        self.profiles.len()
    }

    #[cfg(test)]
    pub fn list_error(&self) -> Option<&str> {
        self.list_error.as_deref()
    }

    #[cfg(test)]
    pub fn has_summary(&self, idx: usize) -> bool {
        self.profiles.get(idx).is_some_and(|p| p.summary.is_some())
    }
}

// ─── View impl ────────────────────────────────────────────────────────────────

impl View for ProfilesView {
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
                .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
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
        let visible = self.visible_indices();
        if let Some(&idx) = visible.get(self.cursor) {
            self.load_summary(ctx.paths, idx);
        }
    }

    fn footer_hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("Enter", "apply"),
            ("d", "diff"),
            ("r", "rollback"),
            ("c", "check"),
            ("x", "delete"),
            ("/", "search"),
        ]
    }
}

// ─── Rendering helpers ────────────────────────────────────────────────────────

fn render_list_pane(view: &ProfilesView, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(" Profiles ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible = view.visible_indices();
    let mut lines: Vec<Line<'_>> = Vec::new();

    for (vpos, &pidx) in visible.iter().enumerate() {
        if let Some(item) = view.profiles.get(pidx) {
            let marker = if vpos == view.cursor {
                if theme.use_unicode {
                    "▶ "
                } else {
                    "> "
                }
            } else {
                "  "
            };
            let style = if vpos == view.cursor {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(
                format!("{marker}{}", item.name),
                style,
            )));
        }
    }

    if lines.is_empty() {
        let msg = view.list_error.as_deref().unwrap_or("No profiles found");
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

fn render_detail_pane(view: &ProfilesView, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'_>> = Vec::new();
    let visible = view.visible_indices();

    if let Some(&pidx) = visible.get(view.cursor) {
        if let Some(item) = view.profiles.get(pidx) {
            lines.push(Line::from(Span::styled(
                item.name.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from("─────"));

            if let Some(ref s) = item.summary {
                if let Some(ref d) = s.description {
                    lines.push(Line::from(format!("Description  : {d}")));
                }
                if let Some(ref a) = s.author {
                    lines.push(Line::from(format!("Author       : {a}")));
                }
                if !s.tags.is_empty() {
                    lines.push(Line::from(format!("Tags         : {}", s.tags.join(", "))));
                }
                if let Some(ref wm) = s.wm {
                    lines.push(Line::from(format!("WM           : {wm}")));
                }
                lines.push(Line::from(format!("Files        : {}", s.file_count)));
                lines.push(Line::from(format!(
                    "Pacman       : {} deps declared",
                    s.pacman_deps
                )));
                if s.aur_deps > 0 {
                    lines.push(Line::from(format!(
                        "AUR          : {} deps declared",
                        s.aur_deps
                    )));
                }
                if s.optional_deps > 0 {
                    lines.push(Line::from(format!(
                        "Optional     : {} deps declared",
                        s.optional_deps
                    )));
                }
                lines.push(Line::from("Last applied : (run check to see)"));
            } else {
                lines.push(Line::from(Span::styled(
                    "(loading...)",
                    Style::default().fg(theme.muted),
                )));
            }

            lines.push(Line::from(""));
            lines.push(Line::from("[Enter] apply  [d] diff  [r] rollback"));
            lines.push(Line::from("[c] check      [x] delete"));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "No profile selected",
            Style::default().fg(theme.muted),
        )));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

// ─── Fuzzy filter ─────────────────────────────────────────────────────────────

fn fuzzy_filter(profiles: &[ProfileItem], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..profiles.len()).collect();
    }
    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, usize)> = profiles
        .iter()
        .enumerate()
        .filter_map(|(i, p)| matcher.fuzzy_match(&p.name, query).map(|s| (s, i)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, i)| i).collect()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_paths(profiles_dir: PathBuf) -> AppPaths {
        AppPaths {
            home: PathBuf::from("/tmp"),
            profiles_dir,
            data_home: PathBuf::from("/tmp"),
            state_home: PathBuf::from("/tmp"),
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

    fn write_minimal_toml(dir: &std::path::Path, name: &str) {
        let content = format!("schema_version = 1\n[profile]\nname = \"{name}\"\n");
        std::fs::write(dir.join(format!("{name}.toml")), content).unwrap();
    }

    // ── 1. new() loads list from Profile::list_names ──────────────────────────

    #[test]
    fn profiles_new_loads_profile_list() {
        let tmp = TempDir::new().unwrap();
        write_minimal_toml(tmp.path(), "alpha");
        write_minimal_toml(tmp.path(), "beta");
        let paths = make_paths(tmp.path().to_path_buf());
        let view = ProfilesView::new(&paths);
        assert_eq!(view.profile_count(), 2);
    }

    // ── 2. new() with non-existent dir → empty, no error ─────────────────────

    #[test]
    fn profiles_new_nonexistent_dir_empty_no_error() {
        let paths = make_paths(PathBuf::from("/nonexistent/path/xyz"));
        let view = ProfilesView::new(&paths);
        assert_eq!(view.profile_count(), 0);
        assert!(view.list_error().is_none());
    }

    // ── 3. new() with read error → list_error set ─────────────────────────────

    #[test]
    fn profiles_new_unreadable_dir_sets_error() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let restricted = tmp.path().join("restricted");
        std::fs::create_dir(&restricted).unwrap();
        std::fs::write(restricted.join("test.toml"), "x").unwrap();
        std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o000)).unwrap();

        let paths = make_paths(restricted.clone());
        let view = ProfilesView::new(&paths);

        // Restore permissions before asserting so TempDir can clean up.
        std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            view.list_error().is_some(),
            "unreadable dir must set list_error"
        );
    }

    // ── 4. cursor initial = 0 ────────────────────────────────────────────────

    #[test]
    fn profiles_cursor_initial_zero() {
        let tmp = TempDir::new().unwrap();
        write_minimal_toml(tmp.path(), "alpha");
        let paths = make_paths(tmp.path().to_path_buf());
        let view = ProfilesView::new(&paths);
        assert_eq!(view.cursor(), 0);
    }

    // ── 5. j key advances cursor ─────────────────────────────────────────────

    #[test]
    fn profiles_j_advances_cursor() {
        let tmp = TempDir::new().unwrap();
        write_minimal_toml(tmp.path(), "alpha");
        write_minimal_toml(tmp.path(), "beta");
        let paths = make_paths(tmp.path().to_path_buf());
        let mut view = ProfilesView::new(&paths);
        let ctx = make_ctx(&paths);
        assert_eq!(view.cursor(), 0);
        view.handle_event(&key(KeyCode::Char('j')), &ctx);
        assert_eq!(view.cursor(), 1);
    }

    // ── 6. k key at cursor=0 does not wrap ───────────────────────────────────

    #[test]
    fn profiles_k_at_zero_stays() {
        let tmp = TempDir::new().unwrap();
        write_minimal_toml(tmp.path(), "alpha");
        write_minimal_toml(tmp.path(), "beta");
        let paths = make_paths(tmp.path().to_path_buf());
        let mut view = ProfilesView::new(&paths);
        let ctx = make_ctx(&paths);
        view.handle_event(&key(KeyCode::Char('k')), &ctx);
        assert_eq!(view.cursor(), 0);
    }

    // ── 7. Enter with no profiles → Action::None ─────────────────────────────

    #[test]
    fn profiles_enter_no_profiles_is_none() {
        let paths = make_paths(PathBuf::from("/nonexistent/xyz"));
        let mut view = ProfilesView::new(&paths);
        let ctx = make_ctx(&paths);
        let action = view.handle_event(&key(KeyCode::Enter), &ctx);
        assert!(matches!(action, Action::None));
    }

    // ── 8. Enter with profile → ApplyProfile confirm modal ───────────────────

    #[test]
    fn profiles_enter_opens_apply_confirm() {
        let tmp = TempDir::new().unwrap();
        write_minimal_toml(tmp.path(), "laptop");
        let paths = make_paths(tmp.path().to_path_buf());
        let mut view = ProfilesView::new(&paths);
        let ctx = make_ctx(&paths);
        let action = view.handle_event(&key(KeyCode::Enter), &ctx);
        assert!(
            matches!(
                action,
                Action::OpenModal(Modal::Confirm {
                    kind: ConfirmKind::ApplyProfile(_),
                    ..
                })
            ),
            "Enter must open ApplyProfile confirm, got {action:?}"
        );
    }

    // ── 9. d with profile → SelectProfileForDiff ─────────────────────────────

    #[test]
    fn profiles_d_opens_diff() {
        let tmp = TempDir::new().unwrap();
        write_minimal_toml(tmp.path(), "laptop");
        let paths = make_paths(tmp.path().to_path_buf());
        let mut view = ProfilesView::new(&paths);
        let ctx = make_ctx(&paths);
        let action = view.handle_event(&key(KeyCode::Char('d')), &ctx);
        assert!(
            matches!(action, Action::SelectProfileForDiff(n) if n == "laptop"),
            "d must emit SelectProfileForDiff(laptop)"
        );
    }

    // ── 10. / activates search mode ──────────────────────────────────────────

    #[test]
    fn profiles_slash_activates_search() {
        let tmp = TempDir::new().unwrap();
        let paths = make_paths(tmp.path().to_path_buf());
        let mut view = ProfilesView::new(&paths);
        let ctx = make_ctx(&paths);
        assert!(!view.is_searching());
        view.handle_event(&key(KeyCode::Char('/')), &ctx);
        assert!(view.is_searching());
    }

    // ── 11. Type chars in search → filtered indices correct ──────────────────

    #[test]
    fn profiles_search_filters_correctly() {
        let tmp = TempDir::new().unwrap();
        write_minimal_toml(tmp.path(), "alpha");
        write_minimal_toml(tmp.path(), "beta");
        write_minimal_toml(tmp.path(), "gamma");
        let paths = make_paths(tmp.path().to_path_buf());
        let mut view = ProfilesView::new(&paths);
        let ctx = make_ctx(&paths);

        // Activate search
        view.handle_event(&key(KeyCode::Char('/')), &ctx);
        // Type "alp" — should match only "alpha"
        view.handle_event(&key(KeyCode::Char('a')), &ctx);
        view.handle_event(&key(KeyCode::Char('l')), &ctx);
        view.handle_event(&key(KeyCode::Char('p')), &ctx);

        let filtered = view.search_filtered().unwrap();
        // Profile "alpha" is at sorted index 0 (alpha < beta < gamma)
        assert!(
            filtered.contains(&0),
            "filtered must include alpha (idx 0), got {filtered:?}"
        );
    }

    // ── 12. Esc in search clears search ──────────────────────────────────────

    #[test]
    fn profiles_esc_clears_search() {
        let tmp = TempDir::new().unwrap();
        let paths = make_paths(tmp.path().to_path_buf());
        let mut view = ProfilesView::new(&paths);
        let ctx = make_ctx(&paths);

        view.handle_event(&key(KeyCode::Char('/')), &ctx);
        assert!(view.is_searching());
        view.handle_event(&key(KeyCode::Esc), &ctx);
        assert!(!view.is_searching(), "Esc must clear search");
    }

    // ── 13. x → DeleteProfile confirm modal ──────────────────────────────────

    #[test]
    fn profiles_x_opens_delete_confirm() {
        let tmp = TempDir::new().unwrap();
        write_minimal_toml(tmp.path(), "laptop");
        let paths = make_paths(tmp.path().to_path_buf());
        let mut view = ProfilesView::new(&paths);
        let ctx = make_ctx(&paths);
        let action = view.handle_event(&key(KeyCode::Char('x')), &ctx);
        assert!(
            matches!(
                action,
                Action::OpenModal(Modal::Confirm {
                    kind: ConfirmKind::DeleteProfile(_),
                    ..
                })
            ),
            "x must open DeleteProfile confirm"
        );
    }

    // ── 14. ctx.busy → action keys emit SetStatus, not Modal ─────────────────

    #[test]
    fn profiles_busy_blocks_actions() {
        let tmp = TempDir::new().unwrap();
        write_minimal_toml(tmp.path(), "laptop");
        let paths = make_paths(tmp.path().to_path_buf());
        let mut view = ProfilesView::new(&paths);
        let ctx = ViewCtx {
            paths: &paths,
            theme: test_theme(),
            busy: true,
        };
        // Enter while busy must return SetStatus, not OpenModal
        let action = view.handle_event(&key(KeyCode::Enter), &ctx);
        assert!(
            matches!(action, Action::SetStatus(_)),
            "Enter while busy must emit SetStatus, got {action:?}"
        );
    }
}
