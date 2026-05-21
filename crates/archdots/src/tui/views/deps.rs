use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use archdots_core::{
    packages::AurHelper,
    validator::{DepEntry, DepKind, DepSource, DepStatus, ValidationReport},
};

use crate::tui::{
    action::Action,
    tasks::BackgroundKind,
    theme::Theme,
    ui::StatusMessage,
    views::{View, ViewCtx},
};

// ─── DepsSection ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepsSection {
    Required,
    RequiredAur,
    Optional,
    Implicit,
}

impl DepsSection {
    fn dep_kind(self) -> DepKind {
        match self {
            Self::Required => DepKind::Pacman,
            Self::RequiredAur => DepKind::Aur,
            Self::Optional => DepKind::OptionalPacman,
            Self::Implicit => DepKind::ImplicitFromConfig,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Required => "Required (pacman)",
            Self::RequiredAur => "Required (AUR)",
            Self::Optional => "Optional (pacman)",
            Self::Implicit => "Implicit (mentioned in configs)",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Required => Self::RequiredAur,
            Self::RequiredAur => Self::Optional,
            Self::Optional => Self::Implicit,
            Self::Implicit => Self::Required,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Required => Self::Implicit,
            Self::RequiredAur => Self::Required,
            Self::Optional => Self::RequiredAur,
            Self::Implicit => Self::Optional,
        }
    }

    fn all() -> &'static [Self] {
        &[
            Self::Required,
            Self::RequiredAur,
            Self::Optional,
            Self::Implicit,
        ]
    }
}

// ─── DepsView ─────────────────────────────────────────────────────────────────

pub struct DepsView {
    pub profile: Option<String>,
    pub report: Option<ValidationReport>,
    pub cursor: usize,
    pub section: DepsSection,
    pub deep_used: bool,
    pub loading: bool,
    pub error: Option<String>,
}

impl DepsView {
    pub fn empty() -> Self {
        Self {
            profile: None,
            report: None,
            cursor: 0,
            section: DepsSection::Required,
            deep_used: false,
            loading: false,
            error: None,
        }
    }

    pub fn set_profile(&mut self, name: String) {
        self.profile = Some(name);
        self.report = None;
        self.error = None;
        self.loading = true;
        self.cursor = 0;
        self.section = DepsSection::Required;
    }

    pub fn apply_report(&mut self, report: ValidationReport) {
        self.report = Some(report);
        self.loading = false;
        self.error = None;
        self.cursor = 0;
        self.section = DepsSection::Required;
    }

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.loading = false;
        self.report = None;
    }

    pub fn section_entries(&self) -> Vec<&DepEntry> {
        let Some(ref report) = self.report else {
            return Vec::new();
        };
        let kind = self.section.dep_kind();
        report.entries.iter().filter(|e| e.kind == kind).collect()
    }

    pub fn selected_entry(&self) -> Option<&DepEntry> {
        let entries = self.section_entries();
        entries.get(self.cursor).copied()
    }

    fn aur_helper_label(report: &ValidationReport) -> &'static str {
        match report.aur_helper {
            Some(AurHelper::Paru) => "paru",
            Some(AurHelper::Yay) => "yay",
            _ => "none detected",
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_key(&mut self, key: KeyEvent, ctx: &ViewCtx<'_>) -> Action {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                let len = self.section_entries().len();
                if len > 0 {
                    self.cursor = (self.cursor + 1).min(len - 1);
                }
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                Action::None
            }
            KeyCode::Char('g') => {
                self.cursor = 0;
                Action::None
            }
            KeyCode::Char('G') => {
                let len = self.section_entries().len();
                if len > 0 {
                    self.cursor = len - 1;
                }
                Action::None
            }
            KeyCode::Char('J') => {
                self.section = self.section.next();
                self.cursor = 0;
                Action::None
            }
            KeyCode::Char('K') => {
                self.section = self.section.prev();
                self.cursor = 0;
                Action::None
            }
            // c works even while busy (local clipboard op, no FS)
            KeyCode::Char('c') => {
                let Some(entry) = self.selected_entry() else {
                    return Action::None;
                };
                if !matches!(entry.status, DepStatus::Missing) {
                    return Action::SetStatus(StatusMessage::Info(format!(
                        "nothing to install for {}",
                        entry.name
                    )));
                }
                let cmd = if matches!(entry.kind, DepKind::Aur) {
                    let helper = self.report.as_ref().and_then(|r| r.aur_helper).map_or(
                        "<aur-helper>",
                        |h| match h {
                            AurHelper::Paru => "paru",
                            AurHelper::Yay => "yay",
                            _ => "<aur-helper>",
                        },
                    );
                    if helper == "<aur-helper>" {
                        format!("{helper} -S {}  # no AUR helper detected", entry.name)
                    } else {
                        format!("{helper} -S {}", entry.name)
                    }
                } else {
                    format!("sudo pacman -S {}", entry.name)
                };
                Action::CopyToClipboard(cmd)
            }
            KeyCode::Char('R') => {
                if ctx.busy {
                    return Action::SetStatus(StatusMessage::Info("operation in progress".into()));
                }
                let Some(ref name) = self.profile else {
                    return Action::None;
                };
                let name = name.clone();
                self.loading = true;
                self.error = None;
                Action::SpawnTask(BackgroundKind::Check {
                    profile: name,
                    deep: self.deep_used,
                })
            }
            KeyCode::Char('D') => {
                if ctx.busy {
                    return Action::SetStatus(StatusMessage::Info("operation in progress".into()));
                }
                let Some(ref name) = self.profile else {
                    return Action::None;
                };
                let name = name.clone();
                self.deep_used = !self.deep_used;
                self.loading = true;
                self.error = None;
                Action::SpawnTask(BackgroundKind::Check {
                    profile: name,
                    deep: self.deep_used,
                })
            }
            _ => Action::None,
        }
    }
}

// ─── Status icon helpers ──────────────────────────────────────────────────────

pub fn status_icon_str(status: &DepStatus, use_unicode: bool) -> &'static str {
    if use_unicode {
        match status {
            DepStatus::Installed => "✓",
            DepStatus::Missing => "✗",
            DepStatus::UnknownBinary { .. } => "?",
            _ => "!",
        }
    } else {
        match status {
            DepStatus::Installed => "[ok]",
            DepStatus::Missing => "[X]",
            DepStatus::UnknownBinary { .. } => "[?]",
            _ => "[!]",
        }
    }
}

pub fn status_color(status: &DepStatus, theme: &Theme) -> ratatui::style::Color {
    match status {
        DepStatus::Installed => theme.ok,
        DepStatus::Missing => theme.err,
        DepStatus::UnknownBinary { .. } => theme.warn,
        _ => theme.accent, // cyan for Indeterminate
    }
}

// ─── View impl ────────────────────────────────────────────────────────────────

impl View for DepsView {
    #[allow(clippy::too_many_lines)]
    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border))
            .title(" Deps ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // ── No profile selected ──
        if self.profile.is_none() {
            let v_offset = inner.height.saturating_sub(1) / 2;
            let render_area = Rect {
                y: inner.y + v_offset,
                height: inner.height.saturating_sub(v_offset),
                ..inner
            };
            frame.render_widget(
                Paragraph::new("No profile selected. Press [c] on a profile in the Profiles view.")
                    .style(Style::default().fg(theme.muted))
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: false }),
                render_area,
            );
            return;
        }

        let profile_name = self.profile.as_deref().unwrap_or("");

        // ── Loading ──
        if self.loading {
            let v_offset = inner.height.saturating_sub(1) / 2;
            let render_area = Rect {
                y: inner.y + v_offset,
                height: inner.height.saturating_sub(v_offset),
                ..inner
            };
            frame.render_widget(
                Paragraph::new(format!("Validating {profile_name}..."))
                    .style(Style::default().fg(theme.muted))
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: false }),
                render_area,
            );
            return;
        }

        // ── Error ──
        if let Some(ref err) = self.error {
            let v_offset = inner.height.saturating_sub(1) / 2;
            let render_area = Rect {
                y: inner.y + v_offset,
                height: inner.height.saturating_sub(v_offset),
                ..inner
            };
            frame.render_widget(
                Paragraph::new(err.as_str())
                    .style(Style::default().fg(theme.err))
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: false }),
                render_area,
            );
            return;
        }

        // ── Normal report view ──
        let Some(ref report) = self.report else {
            return;
        };

        let aur_helper = Self::aur_helper_label(report);
        let deep_tag = if self.deep_used { " [deep]" } else { "" };

        // Layout: header (2 lines) | body (fill) | footer (2 lines)
        let [header_area, body_area, footer_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .areas(inner);

        // Header
        let hdr_left = format!("Profile: {profile_name}");
        let hdr_right = format!("AUR helper: {aur_helper}{deep_tag}");
        let width = header_area.width as usize;
        let total = hdr_left.len() + hdr_right.len();
        let header_text = if total + 2 <= width {
            let pad = width.saturating_sub(total);
            format!("{hdr_left}{hdr_right:>pad$}")
        } else {
            format!("{hdr_left}  {hdr_right}")
        };
        let sep = if theme.use_unicode {
            "─".repeat(header_area.width as usize)
        } else {
            "-".repeat(header_area.width as usize)
        };
        frame.render_widget(
            Paragraph::new(vec![Line::from(header_text), Line::from(sep.clone())]),
            header_area,
        );

        // Body: sections + entries
        let mut lines: Vec<Line<'static>> = Vec::new();
        for &sec in DepsSection::all() {
            let kind = sec.dep_kind();
            let entries: Vec<&DepEntry> =
                report.entries.iter().filter(|e| e.kind == kind).collect();

            let sec_style = if sec == self.section {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            lines.push(Line::from(Span::styled(sec.title().to_owned(), sec_style)));

            if entries.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  (none)".to_owned(),
                    Style::default().fg(theme.muted),
                )));
            } else {
                let is_active = sec == self.section;
                for (idx, entry) in entries.iter().enumerate() {
                    let selected = is_active && idx == self.cursor;
                    let marker = if selected {
                        if theme.use_unicode {
                            "▶ "
                        } else {
                            "> "
                        }
                    } else {
                        "  "
                    };

                    let icon = status_icon_str(&entry.status, theme.use_unicode);
                    let icon_color = status_color(&entry.status, theme);
                    let entry_style = if selected {
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };

                    let mention_suffix =
                        if let DepSource::InferredFromConfig { mentions } = &entry.source {
                            if mentions.is_empty() {
                                String::new()
                            } else {
                                let first = &mentions[0];
                                let loc = format!("  {}:{}", first.file.display(), first.line);
                                if mentions.len() > 1 {
                                    format!("{loc}  (+{} more)", mentions.len() - 1)
                                } else {
                                    loc
                                }
                            }
                        } else {
                            String::new()
                        };

                    lines.push(Line::from(vec![
                        Span::styled(marker.to_owned(), entry_style),
                        Span::styled(icon.to_owned(), Style::default().fg(icon_color)),
                        Span::raw(" ".to_owned()),
                        Span::styled(entry.name.clone(), entry_style),
                        Span::styled(mention_suffix, Style::default().fg(theme.muted)),
                    ]));
                }
            }
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body_area);

        // Footer
        let footer_text = "Heuristic \u{2014} review manually \u{2022} [c]copy [R]refresh [D]deep";
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(sep),
                Line::from(Span::styled(
                    footer_text.to_owned(),
                    Style::default().fg(theme.muted),
                )),
            ]),
            footer_area,
        );
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
            ("j/k", "navigate"),
            ("J/K", "section"),
            ("c", "copy cmd"),
            ("R", "refresh"),
            ("D", "deep"),
        ]
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use archdots_core::validator::{DepEntry, DepKind, DepSource, DepStatus, ValidationReport};

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn fake_paths() -> crate::tui::app::AppPaths {
        crate::tui::app::AppPaths {
            home: PathBuf::from("/tmp"),
            profiles_dir: PathBuf::from("/tmp/profiles"),
            data_home: PathBuf::from("/tmp"),
            state_home: PathBuf::from("/tmp"),
        }
    }

    fn make_ctx(busy: bool) -> crate::tui::views::ViewCtx<'static> {
        // We need a static reference; use a thread-local for the paths.
        // Since ViewCtx borrows AppPaths, we use a leaked box in tests.
        let paths = Box::leak(Box::new(fake_paths()));
        crate::tui::views::ViewCtx { paths, busy }
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

    fn make_entry(name: &str, kind: DepKind, status: DepStatus) -> DepEntry {
        DepEntry {
            name: name.to_owned(),
            kind,
            status,
            source: DepSource::DeclaredInProfile,
        }
    }

    fn make_report(entries: Vec<DepEntry>) -> ValidationReport {
        ValidationReport {
            schema_version: 1,
            profile_name: "test".to_owned(),
            aur_helper: None,
            entries,
            warnings: vec![],
        }
    }

    fn make_report_with_helper(entries: Vec<DepEntry>, helper: AurHelper) -> ValidationReport {
        ValidationReport {
            schema_version: 1,
            profile_name: "test".to_owned(),
            aur_helper: Some(helper),
            entries,
            warnings: vec![],
        }
    }

    // ── 1. empty() initial state ──────────────────────────────────────────────

    #[test]
    fn deps_empty_initial_state() {
        let view = DepsView::empty();
        assert!(view.profile.is_none());
        assert!(view.report.is_none());
        assert!(!view.loading);
        assert!(view.error.is_none());
        assert_eq!(view.cursor, 0);
        assert_eq!(view.section, DepsSection::Required);
        assert!(!view.deep_used);
    }

    // ── 2. set_profile marks loading=true, sets profile ──────────────────────

    #[test]
    fn deps_set_profile_marks_loading() {
        let mut view = DepsView::empty();
        view.set_profile("laptop".into());
        assert_eq!(view.profile.as_deref(), Some("laptop"));
        assert!(view.loading);
        assert!(view.report.is_none());
        assert!(view.error.is_none());
    }

    // ── 3. apply_report clears loading, sets report ───────────────────────────

    #[test]
    fn deps_apply_report_clears_loading() {
        let mut view = DepsView::empty();
        view.set_profile("laptop".into());
        let report = make_report(vec![]);
        view.apply_report(report);
        assert!(!view.loading);
        assert!(view.report.is_some());
        assert!(view.error.is_none());
    }

    // ── 4. set_error clears loading, sets error ───────────────────────────────

    #[test]
    fn deps_set_error_clears_loading() {
        let mut view = DepsView::empty();
        view.set_profile("laptop".into());
        view.set_error("pacman not found".into());
        assert!(!view.loading);
        assert!(view.report.is_none());
        assert_eq!(view.error.as_deref(), Some("pacman not found"));
    }

    // ── 5. section_entries returns only entries for active section ─────────────

    #[test]
    fn deps_section_entries_filters_by_section() {
        let mut view = DepsView::empty();
        let entries = vec![
            make_entry("a", DepKind::Pacman, DepStatus::Installed),
            make_entry("b", DepKind::Aur, DepStatus::Missing),
            make_entry("c", DepKind::OptionalPacman, DepStatus::Missing),
            make_entry("d", DepKind::ImplicitFromConfig, DepStatus::Missing),
        ];
        view.apply_report(make_report(entries));
        view.section = DepsSection::Required;
        let r = view.section_entries();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "a");

        view.section = DepsSection::RequiredAur;
        let r = view.section_entries();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "b");
    }

    // ── 6. j/k navigates within section, no wrap ─────────────────────────────

    #[test]
    fn deps_jk_navigates_within_section_no_wrap() {
        let mut view = DepsView::empty();
        let entries = vec![
            make_entry("a", DepKind::Pacman, DepStatus::Installed),
            make_entry("b", DepKind::Pacman, DepStatus::Missing),
            make_entry("c", DepKind::Pacman, DepStatus::Missing),
        ];
        view.apply_report(make_report(entries));
        view.section = DepsSection::Required;
        assert_eq!(view.cursor, 0);

        let ctx = make_ctx(false);
        view.handle_event(&key(KeyCode::Char('j')), &ctx);
        assert_eq!(view.cursor, 1);
        view.handle_event(&key(KeyCode::Char('j')), &ctx);
        assert_eq!(view.cursor, 2);
        // At end, no wrap
        view.handle_event(&key(KeyCode::Char('j')), &ctx);
        assert_eq!(view.cursor, 2, "j at end must not wrap");
        // k back
        view.handle_event(&key(KeyCode::Char('k')), &ctx);
        assert_eq!(view.cursor, 1);
        // k at 0 does not go negative
        view.handle_event(&key(KeyCode::Char('k')), &ctx);
        view.handle_event(&key(KeyCode::Char('k')), &ctx);
        assert_eq!(view.cursor, 0, "k at 0 must not wrap");
    }

    // ── 7. J changes to next section, cursor reset to 0 ──────────────────────

    #[test]
    fn deps_j_upper_advances_section_resets_cursor() {
        let mut view = DepsView::empty();
        let entries = vec![
            make_entry("a", DepKind::Pacman, DepStatus::Installed),
            make_entry("b", DepKind::Aur, DepStatus::Missing),
        ];
        view.apply_report(make_report(entries));
        view.section = DepsSection::Required;
        view.cursor = 0;
        let ctx = make_ctx(false);
        view.handle_event(&key(KeyCode::Char('J')), &ctx);
        assert_eq!(view.section, DepsSection::RequiredAur);
        assert_eq!(view.cursor, 0);
        view.handle_event(&key(KeyCode::Char('J')), &ctx);
        assert_eq!(view.section, DepsSection::Optional);
        view.handle_event(&key(KeyCode::Char('J')), &ctx);
        assert_eq!(view.section, DepsSection::Implicit);
        // Wrap from Implicit → Required
        view.handle_event(&key(KeyCode::Char('J')), &ctx);
        assert_eq!(view.section, DepsSection::Required);
    }

    // ── 8. K with wrap from Required → Implicit ───────────────────────────────

    #[test]
    fn deps_k_upper_wraps_from_required_to_implicit() {
        let mut view = DepsView::empty();
        view.apply_report(make_report(vec![]));
        view.section = DepsSection::Required;
        let ctx = make_ctx(false);
        view.handle_event(&key(KeyCode::Char('K')), &ctx);
        assert_eq!(
            view.section,
            DepsSection::Implicit,
            "K from Required must wrap to Implicit"
        );
        assert_eq!(view.cursor, 0);
    }

    // ── 9. c on Installed entry → SetStatus "nothing to install" ──────────────

    #[test]
    fn deps_c_on_installed_returns_set_status() {
        let mut view = DepsView::empty();
        let entries = vec![make_entry(
            "hyprland",
            DepKind::Pacman,
            DepStatus::Installed,
        )];
        view.apply_report(make_report(entries));
        view.section = DepsSection::Required;
        view.cursor = 0;
        let ctx = make_ctx(false);
        let action = view.handle_event(&key(KeyCode::Char('c')), &ctx);
        assert!(
            matches!(action, Action::SetStatus(StatusMessage::Info(ref m)) if m.contains("hyprland")),
            "c on Installed must return SetStatus containing name, got {action:?}"
        );
    }

    // ── 10. c on Missing pacman → CopyToClipboard "sudo pacman -S <name>" ─────

    #[test]
    fn deps_c_on_missing_pacman_returns_clipboard() {
        let mut view = DepsView::empty();
        let entries = vec![make_entry(
            "brightnessctl",
            DepKind::Pacman,
            DepStatus::Missing,
        )];
        view.apply_report(make_report(entries));
        view.section = DepsSection::Required;
        view.cursor = 0;
        let ctx = make_ctx(false);
        let action = view.handle_event(&key(KeyCode::Char('c')), &ctx);
        assert!(
            matches!(action, Action::CopyToClipboard(ref cmd) if cmd == "sudo pacman -S brightnessctl"),
            "c on Missing pacman must return CopyToClipboard with pacman cmd, got {action:?}"
        );
    }

    // ── 11. c on Missing AUR with paru → CopyToClipboard "paru -S <name>" ─────

    #[test]
    fn deps_c_on_missing_aur_with_paru() {
        let mut view = DepsView::empty();
        let entries = vec![make_entry("hyprshot", DepKind::Aur, DepStatus::Missing)];
        view.apply_report(make_report_with_helper(entries, AurHelper::Paru));
        view.section = DepsSection::RequiredAur;
        view.cursor = 0;
        let ctx = make_ctx(false);
        let action = view.handle_event(&key(KeyCode::Char('c')), &ctx);
        assert!(
            matches!(action, Action::CopyToClipboard(ref cmd) if cmd == "paru -S hyprshot"),
            "c on Missing AUR with paru must return 'paru -S hyprshot', got {action:?}"
        );
    }

    // ── 12. c on Missing AUR with no helper → placeholder command ─────────────

    #[test]
    fn deps_c_on_missing_aur_no_helper() {
        let mut view = DepsView::empty();
        let entries = vec![make_entry("hyprshot", DepKind::Aur, DepStatus::Missing)];
        view.apply_report(make_report(entries)); // no helper
        view.section = DepsSection::RequiredAur;
        view.cursor = 0;
        let ctx = make_ctx(false);
        let action = view.handle_event(&key(KeyCode::Char('c')), &ctx);
        assert!(
            matches!(action, Action::CopyToClipboard(ref cmd)
                if cmd.contains("<aur-helper>") && cmd.contains("hyprshot")),
            "c on Missing AUR without helper must include placeholder, got {action:?}"
        );
    }

    // ── 13. R → SpawnTask Check with current deep value ───────────────────────

    #[test]
    fn deps_r_spawns_check_task() {
        let mut view = DepsView::empty();
        view.set_profile("laptop".into());
        view.apply_report(make_report(vec![]));
        view.deep_used = false;
        let ctx = make_ctx(false);
        let action = view.handle_event(&key(KeyCode::Char('R')), &ctx);
        assert!(
            matches!(
                action,
                Action::SpawnTask(BackgroundKind::Check {
                    ref profile,
                    deep: false
                }) if profile == "laptop"
            ),
            "R must spawn Check task with deep=false, got {action:?}"
        );
        assert!(view.loading, "R must set loading=true");
    }

    // ── 14. D → toggle deep_used, SpawnTask Check with inverted deep ──────────

    #[test]
    fn deps_d_toggles_deep_and_spawns_check() {
        let mut view = DepsView::empty();
        view.set_profile("laptop".into());
        view.apply_report(make_report(vec![]));
        view.deep_used = false;
        let ctx = make_ctx(false);
        let action = view.handle_event(&key(KeyCode::Char('D')), &ctx);
        assert!(view.deep_used, "D must toggle deep_used to true");
        assert!(
            matches!(
                action,
                Action::SpawnTask(BackgroundKind::Check {
                    ref profile,
                    deep: true
                }) if profile == "laptop"
            ),
            "D must spawn Check with deep=true, got {action:?}"
        );
        assert!(view.loading, "D must set loading=true");

        // Second D toggles back
        view.apply_report(make_report(vec![])); // clear loading
        let action2 = view.handle_event(&key(KeyCode::Char('D')), &ctx);
        assert!(!view.deep_used, "second D must toggle deep_used to false");
        assert!(
            matches!(
                action2,
                Action::SpawnTask(BackgroundKind::Check { deep: false, .. })
            ),
            "second D must spawn Check with deep=false, got {action2:?}"
        );
    }

    // ── 15. profile=None → section_entries empty ──────────────────────────────

    #[test]
    fn deps_no_profile_section_entries_empty() {
        let view = DepsView::empty();
        assert!(view.section_entries().is_empty());
        assert!(view.selected_entry().is_none());
    }

    // ── 16. R during busy → SetStatus ─────────────────────────────────────────

    #[test]
    fn deps_r_blocked_during_busy() {
        let mut view = DepsView::empty();
        view.set_profile("laptop".into());
        view.apply_report(make_report(vec![]));
        let ctx = make_ctx(true); // busy=true
        let action = view.handle_event(&key(KeyCode::Char('R')), &ctx);
        assert!(
            matches!(action, Action::SetStatus(_)),
            "R while busy must return SetStatus, got {action:?}"
        );
    }

    // ── 17. D during busy → SetStatus ─────────────────────────────────────────

    #[test]
    fn deps_d_blocked_during_busy() {
        let mut view = DepsView::empty();
        view.set_profile("laptop".into());
        view.apply_report(make_report(vec![]));
        let deep_before = view.deep_used;
        let ctx = make_ctx(true); // busy=true
        let action = view.handle_event(&key(KeyCode::Char('D')), &ctx);
        assert!(
            matches!(action, Action::SetStatus(_)),
            "D while busy must return SetStatus, got {action:?}"
        );
        assert_eq!(
            view.deep_used, deep_before,
            "D while busy must not toggle deep_used"
        );
    }

    // ── 18. section_entries with no report → empty ────────────────────────────

    #[test]
    fn deps_section_entries_no_report_empty() {
        let mut view = DepsView::empty();
        view.profile = Some("test".into()); // profile set but no report yet
        assert!(view.section_entries().is_empty());
    }

    // ── 19. g/G moves cursor to extremes ─────────────────────────────────────

    #[test]
    fn deps_g_and_capital_g_move_to_extremes() {
        let mut view = DepsView::empty();
        let entries = vec![
            make_entry("a", DepKind::Pacman, DepStatus::Installed),
            make_entry("b", DepKind::Pacman, DepStatus::Missing),
            make_entry("c", DepKind::Pacman, DepStatus::Missing),
        ];
        view.apply_report(make_report(entries));
        view.section = DepsSection::Required;
        let ctx = make_ctx(false);
        view.handle_event(&key(KeyCode::Char('G')), &ctx);
        assert_eq!(view.cursor, 2, "G must move to last entry");
        view.handle_event(&key(KeyCode::Char('g')), &ctx);
        assert_eq!(view.cursor, 0, "g must move to first entry");
    }
}
