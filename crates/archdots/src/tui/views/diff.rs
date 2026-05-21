use std::{fs, path::PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use archdots_core::{
    journal::Journal,
    linker::{ConflictReason, LinkDisposition, LinkSpec, Linker},
    profile::{Profile, ResolveCtx},
    snapshot::SnapshotManager,
};

use crate::{
    diff_util,
    tui::{
        action::{Action, ConfirmKind},
        app::AppPaths,
        theme::Theme,
        ui::{Modal, StatusMessage},
        views::{View, ViewCtx, ViewKind},
    },
};

// ─── Types ───────────────────────────────────────────────────────────────────

/// Classification of one managed file entry in the diff plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffItemKind {
    /// Target is already a symlink to our source — no change needed.
    Owned,
    /// Target does not exist — apply will create the symlink.
    Missing,
    /// Target is a regular file that differs from source.
    ReplaceFile,
    /// Target is a symlink pointing elsewhere.
    ReplaceSymlink,
    /// Target cannot be replaced (directory, bad permissions, outside home …).
    Conflict,
}

struct DiffItem {
    target: PathBuf,
    source: PathBuf,
    display_name: String,
    kind: DiffItemKind,
    conflict_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffLineKind {
    Context,
    Added,
    Removed,
}

struct DiffLine {
    kind: DiffLineKind,
    text: String,
}

enum DiffDetail {
    NotComputed,
    Identical,
    Rendered { lines: Vec<DiffLine> },
    WouldCreate,
    ExternalSymlink { points_to: PathBuf },
    ConflictReason(String),
    Unreadable(String),
}

// ─── DiffView ────────────────────────────────────────────────────────────────

pub struct DiffView {
    pub profile: Option<String>,
    items: Vec<DiffItem>,
    cursor: usize,
    detail: DiffDetail,
    diff_scroll: u16,
    pub error: Option<String>,
}

impl DiffView {
    pub fn empty() -> Self {
        Self {
            profile: None,
            items: Vec::new(),
            cursor: 0,
            detail: DiffDetail::NotComputed,
            diff_scroll: 0,
            error: None,
        }
    }

    /// Load the profile, run `Linker::plan`, classify every entry.
    ///
    /// Synchronous — `plan()` is < 200 ms (only `lstat` per target).
    pub fn set_profile(&mut self, name: &str, paths: &AppPaths) {
        self.profile = Some(name.to_string());
        self.items.clear();
        self.cursor = 0;
        self.detail = DiffDetail::NotComputed;
        self.diff_scroll = 0;
        self.error = None;

        let profile_path = paths.profiles_dir.join(format!("{name}.toml"));
        let profile = match Profile::load_from_file(&profile_path) {
            Ok(p) => p,
            Err(e) => {
                self.error = Some(format!("failed to load profile '{name}': {e}"));
                return;
            }
        };

        let ctx = ResolveCtx::with_home(&paths.home);
        let specs: Vec<LinkSpec> = match profile
            .resolved_entries(&paths.home, &ctx)
            .map(|r| {
                r.map(|(_entry, src, tgt)| LinkSpec {
                    source: src,
                    target: tgt,
                })
            })
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(v) => v,
            Err(e) => {
                self.error = Some(format!("failed to resolve entries: {e}"));
                return;
            }
        };

        let snaps = match SnapshotManager::open(&paths.data_home) {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(format!("failed to open snapshots: {e}"));
                return;
            }
        };
        let journal = match Journal::open(&paths.state_home) {
            Ok(j) => j,
            Err(e) => {
                self.error = Some(format!("failed to open journal: {e}"));
                return;
            }
        };
        let linker = Linker::new(&snaps, &journal);
        let plan = match linker.plan(name, &specs, &paths.home) {
            Ok(p) => p,
            Err(e) => {
                self.error = Some(format!("plan failed: {e}"));
                return;
            }
        };

        self.items = plan
            .items
            .into_iter()
            .map(|planned| {
                let (kind, conflict_reason) = classify_disposition(&planned.disposition);
                let display_name = planned.spec.target.strip_prefix(&paths.home).map_or_else(
                    |_| {
                        planned.spec.target.file_name().map_or_else(
                            || planned.spec.target.to_string_lossy().to_string(),
                            |n| n.to_string_lossy().to_string(),
                        )
                    },
                    |p| p.to_string_lossy().to_string(),
                );
                DiffItem {
                    target: planned.spec.target,
                    source: planned.spec.source,
                    display_name,
                    kind,
                    conflict_reason,
                }
            })
            .collect();

        if !self.items.is_empty() {
            self.compute_detail(paths);
        }
    }

    fn compute_detail(&mut self, _paths: &AppPaths) {
        let (kind, conflict_reason, source, target) =
            if let Some(item) = self.items.get(self.cursor) {
                (
                    item.kind,
                    item.conflict_reason.clone(),
                    item.source.clone(),
                    item.target.clone(),
                )
            } else {
                self.detail = DiffDetail::NotComputed;
                return;
            };

        self.detail = match kind {
            DiffItemKind::Owned => DiffDetail::Identical,
            DiffItemKind::Missing => DiffDetail::WouldCreate,
            DiffItemKind::ReplaceSymlink => match fs::read_link(&target) {
                Ok(dest) => DiffDetail::ExternalSymlink { points_to: dest },
                Err(e) => DiffDetail::Unreadable(format!("cannot read symlink: {e}")),
            },
            DiffItemKind::Conflict => DiffDetail::ConflictReason(
                conflict_reason.unwrap_or_else(|| "conflict".to_string()),
            ),
            DiffItemKind::ReplaceFile => {
                match (fs::read_to_string(&source), fs::read_to_string(&target)) {
                    (Err(e), _) => DiffDetail::Unreadable(format!("cannot read source: {e}")),
                    (_, Err(e)) => DiffDetail::Unreadable(format!("cannot read target: {e}")),
                    (Ok(src_content), Ok(tgt_content)) => {
                        if src_content == tgt_content {
                            DiffDetail::Identical
                        } else {
                            let lines = diff_util::compute_diff_lines(&tgt_content, &src_content)
                                .into_iter()
                                .map(|(tag, text)| DiffLine {
                                    kind: match tag {
                                        diff_util::DiffTag::Context => DiffLineKind::Context,
                                        diff_util::DiffTag::Added => DiffLineKind::Added,
                                        diff_util::DiffTag::Removed => DiffLineKind::Removed,
                                    },
                                    text,
                                })
                                .collect();
                            DiffDetail::Rendered { lines }
                        }
                    }
                }
            }
        };
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &ViewCtx<'_>) -> Action {
        let len = self.items.len();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if len > 0 && self.cursor + 1 < len {
                    self.cursor += 1;
                    self.diff_scroll = 0;
                    self.compute_detail(ctx.paths);
                }
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.diff_scroll = 0;
                    self.compute_detail(ctx.paths);
                }
                Action::None
            }
            KeyCode::Char('g') => {
                if len > 0 && self.cursor != 0 {
                    self.cursor = 0;
                    self.diff_scroll = 0;
                    self.compute_detail(ctx.paths);
                }
                Action::None
            }
            KeyCode::Char('G') => {
                if len > 0 && self.cursor != len - 1 {
                    self.cursor = len - 1;
                    self.diff_scroll = 0;
                    self.compute_detail(ctx.paths);
                }
                Action::None
            }
            KeyCode::Char('J') => {
                let max = self.diff_max_scroll();
                if self.diff_scroll < max {
                    self.diff_scroll += 1;
                }
                Action::None
            }
            KeyCode::Char('K') => {
                self.diff_scroll = self.diff_scroll.saturating_sub(1);
                Action::None
            }
            KeyCode::Char('a') => {
                if ctx.busy {
                    return Action::SetStatus(StatusMessage::Info("operation in progress".into()));
                }
                let Some(ref profile_name) = self.profile else {
                    return Action::None;
                };
                let profile_name = profile_name.clone();
                let n_create = self
                    .items
                    .iter()
                    .filter(|i| i.kind == DiffItemKind::Missing)
                    .count();
                let n_replace = self
                    .items
                    .iter()
                    .filter(|i| {
                        matches!(
                            i.kind,
                            DiffItemKind::ReplaceFile | DiffItemKind::ReplaceSymlink
                        )
                    })
                    .count();
                let n_conflict = self
                    .items
                    .iter()
                    .filter(|i| i.kind == DiffItemKind::Conflict)
                    .count();
                let total = self.items.len();
                Action::OpenModal(Modal::Confirm {
                    prompt: format!("Apply '{profile_name}'?"),
                    details: Some(format!(
                        "{total} file(s): {n_create} to create, {n_replace} to replace, {n_conflict} conflict(s)."
                    )),
                    kind: ConfirmKind::ApplyProfile(profile_name),
                })
            }
            KeyCode::Char('q') => Action::SwitchView(ViewKind::Profiles),
            _ => Action::None,
        }
    }

    fn diff_max_scroll(&self) -> u16 {
        match &self.detail {
            DiffDetail::Rendered { lines } => u16::try_from(lines.len())
                .unwrap_or(u16::MAX)
                .saturating_sub(1),
            _ => 0,
        }
    }

    // ── Test helpers ──────────────────────────────────────────────────────────

    #[cfg(test)]
    pub fn item_count(&self) -> usize {
        self.items.len()
    }

    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    #[cfg(test)]
    pub fn diff_scroll(&self) -> u16 {
        self.diff_scroll
    }

    #[cfg(test)]
    pub fn item_kind(&self, idx: usize) -> Option<DiffItemKind> {
        self.items.get(idx).map(|i| i.kind)
    }

    #[cfg(test)]
    pub fn detail_is_identical(&self) -> bool {
        matches!(self.detail, DiffDetail::Identical)
    }

    #[cfg(test)]
    pub fn detail_is_would_create(&self) -> bool {
        matches!(self.detail, DiffDetail::WouldCreate)
    }

    #[cfg(test)]
    pub fn detail_is_rendered(&self) -> bool {
        matches!(self.detail, DiffDetail::Rendered { .. })
    }

    #[cfg(test)]
    pub fn detail_is_external_symlink(&self) -> bool {
        matches!(self.detail, DiffDetail::ExternalSymlink { .. })
    }

    #[cfg(test)]
    pub fn detail_is_conflict_reason(&self) -> bool {
        matches!(self.detail, DiffDetail::ConflictReason(_))
    }

    #[cfg(test)]
    pub fn detail_is_unreadable(&self) -> bool {
        matches!(self.detail, DiffDetail::Unreadable(_))
    }

    /// Returns true if the rendered diff has at least one Added or Removed line.
    #[cfg(test)]
    pub fn rendered_has_changes(&self) -> bool {
        match &self.detail {
            DiffDetail::Rendered { lines } => lines
                .iter()
                .any(|l| matches!(l.kind, DiffLineKind::Added | DiffLineKind::Removed)),
            _ => false,
        }
    }
}

// ─── Helper functions ────────────────────────────────────────────────────────

fn classify_disposition(disposition: &LinkDisposition) -> (DiffItemKind, Option<String>) {
    match disposition {
        LinkDisposition::Create => (DiffItemKind::Missing, None),
        LinkDisposition::AlreadyOwned => (DiffItemKind::Owned, None),
        LinkDisposition::ReplaceFile => (DiffItemKind::ReplaceFile, None),
        LinkDisposition::ReplaceSymlink { .. } => (DiffItemKind::ReplaceSymlink, None),
        LinkDisposition::Conflict(reason) => {
            let msg = conflict_reason_str(*reason).to_string();
            (DiffItemKind::Conflict, Some(msg))
        }
        LinkDisposition::SkipDir => (
            DiffItemKind::Conflict,
            Some("target is a directory".to_string()),
        ),
        _ => (DiffItemKind::Conflict, Some("unknown conflict".to_string())),
    }
}

fn conflict_reason_str(reason: ConflictReason) -> &'static str {
    match reason {
        ConflictReason::ParentMissing => "parent directory does not exist",
        ConflictReason::PermissionDenied => "parent directory is not writable",
        ConflictReason::OutsideHome => "target is outside $HOME",
        ConflictReason::SourceMissing => "source file does not exist",
        ConflictReason::TargetIsDirectory => "target is a directory",
        _ => "unknown conflict",
    }
}

fn item_glyph(kind: DiffItemKind, theme: &Theme) -> (&'static str, Color) {
    if theme.use_unicode {
        match kind {
            DiffItemKind::Owned => ("✓", theme.ok),
            DiffItemKind::Missing => ("⊕", theme.accent),
            DiffItemKind::ReplaceFile | DiffItemKind::ReplaceSymlink => ("⟂", theme.warn),
            DiffItemKind::Conflict => ("⚠", theme.err),
        }
    } else {
        match kind {
            DiffItemKind::Owned => ("[=]", theme.ok),
            DiffItemKind::Missing => ("[+]", theme.accent),
            DiffItemKind::ReplaceFile | DiffItemKind::ReplaceSymlink => ("[~]", theme.warn),
            DiffItemKind::Conflict => ("[!]", theme.err),
        }
    }
}

// ─── View impl ───────────────────────────────────────────────────────────────

impl View for DiffView {
    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let (left_area, right_area) = if area.width < 60 {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
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
        self.handle_key(*key, ctx)
    }

    fn on_focus(&mut self, _ctx: &ViewCtx<'_>) {}

    fn footer_hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("j/k", "files"),
            ("J/K", "scroll"),
            ("a", "apply"),
            ("q", "back"),
        ]
    }
}

// ─── Render helpers ──────────────────────────────────────────────────────────

fn render_list_pane(view: &DiffView, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let title = view.profile.as_deref().unwrap_or("Diff");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(format!(" {title} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'_>> = Vec::new();

    match (&view.profile, &view.error) {
        (None, _) => {
            lines.push(Line::from(Span::styled(
                "No profile selected. Press [d] on a profile.",
                Style::default().fg(theme.muted),
            )));
        }
        (Some(_), Some(err)) => {
            lines.push(Line::from(Span::styled(
                err.clone(),
                Style::default().fg(theme.err),
            )));
        }
        (Some(_), None) if view.items.is_empty() => {
            lines.push(Line::from(Span::styled(
                "Profile has no files to diff.",
                Style::default().fg(theme.muted),
            )));
        }
        (Some(_), None) => {
            for (idx, item) in view.items.iter().enumerate() {
                let is_cursor = idx == view.cursor;
                let marker = if is_cursor {
                    if theme.use_unicode {
                        "▶ "
                    } else {
                        "> "
                    }
                } else {
                    "  "
                };
                let (glyph, glyph_color) = item_glyph(item.kind, theme);
                let name_style = if is_cursor {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::raw(marker.to_owned()),
                    Span::styled(item.display_name.clone(), name_style),
                    Span::raw("  ".to_owned()),
                    Span::styled(glyph.to_owned(), Style::default().fg(glyph_color)),
                ]));
            }
            // Legend
            lines.push(Line::from(""));
            if theme.use_unicode {
                lines.push(Line::from(Span::styled(
                    "⊕ create  ✓ owned  ⟂ replace  ⚠ conflict".to_owned(),
                    Style::default().fg(theme.muted),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "[+]create [=]owned [~]replace [!]conflict".to_owned(),
                    Style::default().fg(theme.muted),
                )));
            }
            lines.push(Line::from(Span::styled(
                "[a]apply [j/k]files [J/K]scroll [q]back".to_owned(),
                Style::default().fg(theme.muted),
            )));
        }
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

#[allow(clippy::too_many_lines)]
fn render_detail_pane(view: &DiffView, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'_>> = Vec::new();

    if view.profile.is_none() {
        lines.push(Line::from(Span::styled(
            "No profile selected. Press [d] on a profile in the Profiles view.".to_owned(),
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    }

    if let Some(ref err) = view.error {
        lines.push(Line::from(Span::styled(
            err.clone(),
            Style::default().fg(theme.err),
        )));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    }

    if view.items.is_empty() {
        lines.push(Line::from(Span::styled(
            "Profile has no files to diff.".to_owned(),
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    }

    let Some(item) = view.items.get(view.cursor) else {
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    };

    // Header: file name + separator
    lines.push(Line::from(Span::styled(
        item.display_name.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    let sep = if theme.use_unicode {
        "─".repeat(inner.width as usize)
    } else {
        "-".repeat(inner.width as usize)
    };
    lines.push(Line::from(sep.clone()));

    match &view.detail {
        DiffDetail::NotComputed => {
            lines.push(Line::from(Span::styled(
                "Loading...".to_owned(),
                Style::default().fg(theme.muted),
            )));
        }
        DiffDetail::Identical => {
            lines.push(Line::from(
                "Files are identical. Target already matches source.".to_owned(),
            ));
        }
        DiffDetail::WouldCreate => {
            lines.push(Line::from(
                "Target does not exist. Apply will create this symlink.".to_owned(),
            ));
        }
        DiffDetail::ExternalSymlink { points_to } => {
            lines.push(Line::from(format!(
                "Target is a symlink to: {}",
                points_to.display()
            )));
            lines.push(Line::from("Apply will replace it.".to_owned()));
        }
        DiffDetail::ConflictReason(reason) => {
            lines.push(Line::from(format!("Conflict: {reason}")));
            lines.push(Line::from(
                "Apply will skip or fail on this entry.".to_owned(),
            ));
        }
        DiffDetail::Unreadable(reason) => {
            lines.push(Line::from(format!("Could not read file: {reason}")));
        }
        DiffDetail::Rendered { lines: diff_lines } => {
            lines.push(Line::from(Span::styled(
                "--- target (current)".to_owned(),
                Style::default().fg(theme.err),
            )));
            lines.push(Line::from(Span::styled(
                "+++ source (profile)".to_owned(),
                Style::default().fg(theme.ok),
            )));

            // Reserve 2 lines for the header already added, 2 for ±++ header
            let header_lines = u16::try_from(lines.len()).unwrap_or(u16::MAX);
            let visible_height = inner.height.saturating_sub(header_lines + 1);

            let start = view.diff_scroll as usize;
            for diff_line in diff_lines.iter().skip(start).take(visible_height as usize) {
                let (prefix, style) = match diff_line.kind {
                    DiffLineKind::Context => (" ", Style::default()),
                    DiffLineKind::Added => ("+", Style::default().fg(theme.ok)),
                    DiffLineKind::Removed => ("-", Style::default().fg(theme.err)),
                };
                let text = diff_line.text.trim_end_matches('\n');
                lines.push(Line::from(Span::styled(format!("{prefix}{text}"), style)));
            }

            if view.diff_scroll < view.diff_max_scroll() {
                lines.push(Line::from(Span::styled(
                    if theme.use_unicode {
                        "↓ scroll".to_owned()
                    } else {
                        "v scroll".to_owned()
                    },
                    Style::default().fg(theme.muted),
                )));
            }
        }
    }

    // Footer separator
    lines.push(Line::from(sep));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use tempfile::TempDir;

    use crate::tui::{app::AppPaths, theme::Theme};

    // ── Helpers ───────────────────────────────────────────────────────────────

    struct TestEnv {
        home: TempDir,
        data: TempDir,
        state: TempDir,
        profiles_dir: PathBuf,
    }

    fn make_env() -> TestEnv {
        let home = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let profiles_dir = home.path().join("profiles");
        std::fs::create_dir_all(&profiles_dir).unwrap();
        TestEnv {
            home,
            data,
            state,
            profiles_dir,
        }
    }

    fn make_paths(env: &TestEnv) -> AppPaths {
        AppPaths {
            home: env.home.path().to_path_buf(),
            profiles_dir: env.profiles_dir.clone(),
            data_home: env.data.path().to_path_buf(),
            state_home: env.state.path().to_path_buf(),
        }
    }

    fn write_profile(dir: &Path, name: &str, files: &[(&str, &str)]) {
        let mut content = format!(
            "schema_version = 1\n\n[profile]\nname = \"{name}\"\n\n[dependencies]\n\n[hooks]\n\n"
        );
        for (i, (source, target)) in files.iter().enumerate() {
            content.push_str(&format!(
                "[[files]]\nid = \"f{i}\"\nsource = \"{source}\"\ntarget = \"{target}\"\n\n"
            ));
        }
        std::fs::write(dir.join(format!("{name}.toml")), content).unwrap();
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

    static THEME: std::sync::OnceLock<Theme> = std::sync::OnceLock::new();
    fn test_theme() -> &'static Theme {
        THEME.get_or_init(Theme::default)
    }

    fn make_ctx(paths: &AppPaths, busy: bool) -> ViewCtx<'_> {
        ViewCtx {
            paths,
            theme: test_theme(),
            busy,
        }
    }

    // ── 1. empty() ────────────────────────────────────────────────────────────

    #[test]
    fn diff_empty_initial_state() {
        let view = DiffView::empty();
        assert!(view.profile.is_none());
        assert_eq!(view.item_count(), 0);
        assert_eq!(view.cursor(), 0);
        assert_eq!(view.diff_scroll(), 0);
        assert!(view.error.is_none());
    }

    // ── 2. set_profile loads N items ─────────────────────────────────────────

    #[test]
    fn diff_set_profile_loads_items() {
        let env = make_env();
        let paths = make_paths(&env);
        std::fs::write(env.home.path().join("dotfile1"), "c1").unwrap();
        std::fs::write(env.home.path().join("dotfile2"), "c2").unwrap();
        write_profile(
            &env.profiles_dir,
            "test",
            &[("dotfile1", "~/.dotfile1"), ("dotfile2", "~/.dotfile2")],
        );

        let mut view = DiffView::empty();
        view.set_profile("test", &paths);
        assert!(view.error.is_none(), "unexpected error: {:?}", view.error);
        assert_eq!(view.item_count(), 2);
    }

    // ── 3. set_profile nonexistent → error ───────────────────────────────────

    #[test]
    fn diff_set_profile_nonexistent_sets_error() {
        let env = make_env();
        let paths = make_paths(&env);
        let mut view = DiffView::empty();
        view.set_profile("nonexistent", &paths);
        assert!(view.error.is_some(), "must set error for missing profile");
        assert_eq!(view.item_count(), 0);
    }

    // ── 4. Missing classification ─────────────────────────────────────────────

    #[test]
    fn diff_classify_target_missing() {
        let env = make_env();
        let paths = make_paths(&env);
        std::fs::write(env.home.path().join("src"), "x").unwrap();
        // target ~/.tgt does NOT exist
        write_profile(&env.profiles_dir, "p", &[("src", "~/.tgt")]);

        let mut view = DiffView::empty();
        view.set_profile("p", &paths);
        assert_eq!(view.item_kind(0), Some(DiffItemKind::Missing));
    }

    // ── 5. Owned classification ───────────────────────────────────────────────

    #[test]
    fn diff_classify_owned_symlink() {
        let env = make_env();
        let paths = make_paths(&env);
        let src = env.home.path().join("src");
        std::fs::write(&src, "x").unwrap();
        // target is already a symlink to src
        let tgt = env.home.path().join(".tgt");
        symlink(&src, &tgt).unwrap();
        write_profile(&env.profiles_dir, "p", &[("src", "~/.tgt")]);

        let mut view = DiffView::empty();
        view.set_profile("p", &paths);
        assert_eq!(view.item_kind(0), Some(DiffItemKind::Owned));
    }

    // ── 6. ReplaceFile classification ─────────────────────────────────────────

    #[test]
    fn diff_classify_replace_file() {
        let env = make_env();
        let paths = make_paths(&env);
        std::fs::write(env.home.path().join("src"), "new").unwrap();
        std::fs::write(env.home.path().join(".tgt"), "old").unwrap();
        write_profile(&env.profiles_dir, "p", &[("src", "~/.tgt")]);

        let mut view = DiffView::empty();
        view.set_profile("p", &paths);
        assert_eq!(view.item_kind(0), Some(DiffItemKind::ReplaceFile));
    }

    // ── 7. ReplaceSymlink classification ──────────────────────────────────────

    #[test]
    fn diff_classify_replace_symlink() {
        let env = make_env();
        let paths = make_paths(&env);
        let src = env.home.path().join("src");
        std::fs::write(&src, "x").unwrap();
        let other = env.home.path().join("other");
        std::fs::write(&other, "y").unwrap();
        let tgt = env.home.path().join(".tgt");
        symlink(&other, &tgt).unwrap(); // points elsewhere
        write_profile(&env.profiles_dir, "p", &[("src", "~/.tgt")]);

        let mut view = DiffView::empty();
        view.set_profile("p", &paths);
        assert_eq!(view.item_kind(0), Some(DiffItemKind::ReplaceSymlink));
    }

    // ── 8. Conflict classification (directory) ────────────────────────────────

    #[test]
    fn diff_classify_target_is_directory() {
        let env = make_env();
        let paths = make_paths(&env);
        std::fs::write(env.home.path().join("src"), "x").unwrap();
        std::fs::create_dir(env.home.path().join(".tgt")).unwrap();
        write_profile(&env.profiles_dir, "p", &[("src", "~/.tgt")]);

        let mut view = DiffView::empty();
        view.set_profile("p", &paths);
        assert_eq!(view.item_kind(0), Some(DiffItemKind::Conflict));
    }

    // ── 9. compute_detail Owned → Identical ───────────────────────────────────

    #[test]
    fn diff_detail_owned_is_identical() {
        let env = make_env();
        let paths = make_paths(&env);
        let src = env.home.path().join("src");
        std::fs::write(&src, "x").unwrap();
        let tgt = env.home.path().join(".tgt");
        symlink(&src, &tgt).unwrap();
        write_profile(&env.profiles_dir, "p", &[("src", "~/.tgt")]);

        let mut view = DiffView::empty();
        view.set_profile("p", &paths);
        assert!(
            view.detail_is_identical(),
            "Owned must produce Identical detail"
        );
    }

    // ── 10. compute_detail Missing → WouldCreate ──────────────────────────────

    #[test]
    fn diff_detail_missing_is_would_create() {
        let env = make_env();
        let paths = make_paths(&env);
        std::fs::write(env.home.path().join("src"), "x").unwrap();
        write_profile(&env.profiles_dir, "p", &[("src", "~/.tgt")]);

        let mut view = DiffView::empty();
        view.set_profile("p", &paths);
        assert!(
            view.detail_is_would_create(),
            "Missing must produce WouldCreate detail"
        );
    }

    // ── 11. compute_detail ReplaceFile different content → Rendered ───────────

    #[test]
    fn diff_detail_replace_file_different_content_renders() {
        let env = make_env();
        let paths = make_paths(&env);
        std::fs::write(env.home.path().join("src"), "new content\n").unwrap();
        std::fs::write(env.home.path().join(".tgt"), "old content\n").unwrap();
        write_profile(&env.profiles_dir, "p", &[("src", "~/.tgt")]);

        let mut view = DiffView::empty();
        view.set_profile("p", &paths);
        assert!(
            view.detail_is_rendered(),
            "different file content must produce Rendered detail"
        );
        assert!(
            view.rendered_has_changes(),
            "diff must have Added or Removed lines"
        );
    }

    // ── 12. compute_detail ReplaceFile identical content → Identical ──────────

    #[test]
    fn diff_detail_replace_file_identical_content() {
        let env = make_env();
        let paths = make_paths(&env);
        let content = "same content\n";
        std::fs::write(env.home.path().join("src"), content).unwrap();
        std::fs::write(env.home.path().join(".tgt"), content).unwrap();
        write_profile(&env.profiles_dir, "p", &[("src", "~/.tgt")]);

        // Force ReplaceFile: target is a regular file; linker classifies it as ReplaceFile.
        // Even though content is identical, set_profile still creates the ReplaceFile item.
        let mut view = DiffView::empty();
        view.set_profile("p", &paths);
        assert_eq!(view.item_kind(0), Some(DiffItemKind::ReplaceFile));
        assert!(
            view.detail_is_identical(),
            "identical file content must produce Identical detail"
        );
    }

    // ── 13. j/k navigate, reset scroll ───────────────────────────────────────

    #[test]
    fn diff_jk_navigates_and_resets_scroll() {
        let env = make_env();
        let paths = make_paths(&env);
        std::fs::write(env.home.path().join("a"), "x").unwrap();
        std::fs::write(env.home.path().join("b"), "y").unwrap();
        write_profile(&env.profiles_dir, "p", &[("a", "~/.a"), ("b", "~/.b")]);

        let mut view = DiffView::empty();
        view.set_profile("p", &paths);
        assert_eq!(view.cursor(), 0);

        // Manually set a non-zero scroll to verify it resets
        view.diff_scroll = 5;
        let ctx = make_ctx(&paths, false);
        view.handle_event(&key(KeyCode::Char('j')), &ctx);
        assert_eq!(view.cursor(), 1);
        assert_eq!(view.diff_scroll(), 0, "j must reset diff_scroll");

        view.diff_scroll = 3;
        view.handle_event(&key(KeyCode::Char('k')), &ctx);
        assert_eq!(view.cursor(), 0);
        assert_eq!(view.diff_scroll(), 0, "k must reset diff_scroll");

        // k at 0 must not wrap
        view.handle_event(&key(KeyCode::Char('k')), &ctx);
        assert_eq!(view.cursor(), 0, "k at 0 must not go negative");
    }

    // ── 14. J/K scroll the diff body ─────────────────────────────────────────

    #[test]
    fn diff_jk_upper_scrolls_diff_body() {
        let env = make_env();
        let paths = make_paths(&env);
        // Create a file with enough lines to have scrollable diff
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nB\nc\nd\ne\nF\ng\nh\n";
        std::fs::write(env.home.path().join("src"), new).unwrap();
        std::fs::write(env.home.path().join(".tgt"), old).unwrap();
        write_profile(&env.profiles_dir, "p", &[("src", "~/.tgt")]);

        let mut view = DiffView::empty();
        view.set_profile("p", &paths);
        assert!(view.detail_is_rendered());
        assert_eq!(view.diff_scroll(), 0);

        let ctx = make_ctx(&paths, false);
        view.handle_event(&key(KeyCode::Char('J')), &ctx);
        assert_eq!(view.diff_scroll(), 1, "J must increment scroll");

        view.handle_event(&key(KeyCode::Char('J')), &ctx);
        assert_eq!(view.diff_scroll(), 2);

        view.handle_event(&key(KeyCode::Char('K')), &ctx);
        assert_eq!(view.diff_scroll(), 1, "K must decrement scroll");

        view.handle_event(&key(KeyCode::Char('K')), &ctx);
        assert_eq!(view.diff_scroll(), 0);

        // K at 0 must not underflow
        view.handle_event(&key(KeyCode::Char('K')), &ctx);
        assert_eq!(view.diff_scroll(), 0, "K at 0 must not underflow");
    }

    // ── 15. a with profile → ApplyProfile confirm modal ──────────────────────

    #[test]
    fn diff_a_opens_apply_confirm() {
        let env = make_env();
        let paths = make_paths(&env);
        std::fs::write(env.home.path().join("src"), "x").unwrap();
        write_profile(&env.profiles_dir, "myprofile", &[("src", "~/.tgt")]);

        let mut view = DiffView::empty();
        view.set_profile("myprofile", &paths);
        let ctx = make_ctx(&paths, false);
        let action = view.handle_event(&key(KeyCode::Char('a')), &ctx);
        assert!(
            matches!(
                action,
                Action::OpenModal(Modal::Confirm {
                    kind: ConfirmKind::ApplyProfile(ref n),
                    ..
                }) if n == "myprofile"
            ),
            "a must open ApplyProfile confirm, got {action:?}"
        );
    }

    // ── 16. q → SwitchView(Profiles) ─────────────────────────────────────────

    #[test]
    fn diff_q_switches_to_profiles() {
        let env = make_env();
        let paths = make_paths(&env);
        let mut view = DiffView::empty();
        let ctx = make_ctx(&paths, false);
        let action = view.handle_event(&key(KeyCode::Char('q')), &ctx);
        assert!(
            matches!(action, Action::SwitchView(ViewKind::Profiles)),
            "q must return SwitchView(Profiles)"
        );
    }

    // ── 17. a during busy → SetStatus ────────────────────────────────────────

    #[test]
    fn diff_a_blocked_during_busy() {
        let env = make_env();
        let paths = make_paths(&env);
        std::fs::write(env.home.path().join("src"), "x").unwrap();
        write_profile(&env.profiles_dir, "p", &[("src", "~/.tgt")]);

        let mut view = DiffView::empty();
        view.set_profile("p", &paths);
        let ctx = make_ctx(&paths, true); // busy=true
        let action = view.handle_event(&key(KeyCode::Char('a')), &ctx);
        assert!(
            matches!(action, Action::SetStatus(StatusMessage::Info(_))),
            "a while busy must return SetStatus, got {action:?}"
        );
    }

    // ── 18. compute_detail unreadable file → Unreadable ───────────────────────

    #[test]
    fn diff_detail_unreadable_source() {
        use std::os::unix::fs::PermissionsExt;

        let env = make_env();
        let paths = make_paths(&env);
        let src = env.home.path().join("src");
        std::fs::write(&src, "content").unwrap();
        std::fs::write(env.home.path().join(".tgt"), "old").unwrap();
        write_profile(&env.profiles_dir, "p", &[("src", "~/.tgt")]);

        // Make source unreadable before set_profile runs compute_detail
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o000)).unwrap();

        let mut view = DiffView::empty();
        view.set_profile("p", &paths);

        // Restore so TempDir can clean up
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(view.item_kind(0), Some(DiffItemKind::ReplaceFile));
        assert!(
            view.detail_is_unreadable(),
            "unreadable source must produce Unreadable detail"
        );
    }
}
