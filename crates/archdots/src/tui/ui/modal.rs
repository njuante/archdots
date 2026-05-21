use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::tui::{action::ConfirmKind, theme::Theme};

/// A modal overlay rendered over the active view.
#[derive(Debug, Clone)]
pub enum Modal {
    /// Confirmation dialog with yes/no choice.
    Confirm {
        prompt: String,
        details: Option<String>,
        kind: ConfirmKind,
    },
    /// Error message with optional copy action.
    Error { title: String, body: String },
    /// Scrollable informational text (e.g. help, snapshot manifest).
    ScrollableInfo {
        title: String,
        body: String,
        scroll: u16,
    },
}

/// Result of passing a key event to a modal.
#[derive(Debug)]
pub enum ModalOutcome {
    /// The modal consumed the key and stays open.
    KeepOpen,
    /// The modal should be closed (user pressed Esc or [n]).
    Close,
    /// The user confirmed; App should call `execute_confirm(kind)`.
    Confirm(ConfirmKind),
    /// The user pressed [c]; the given string should be copied to the clipboard.
    Copy(String),
}

impl Modal {
    /// Render this modal as a centered overlay.
    pub fn render(&self, frame: &mut Frame<'_>, theme: &Theme) {
        let area = centered_rect(60, 40, frame.area());
        frame.render_widget(Clear, area);

        match self {
            Self::Confirm {
                prompt, details, ..
            } => {
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.accent))
                    .title(Span::styled(
                        " Confirm ",
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ));
                let inner = block.inner(area);
                frame.render_widget(block, area);

                let mut lines: Vec<Line<'_>> = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        prompt.as_str(),
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                ];
                if let Some(d) = details {
                    lines.push(Line::from(""));
                    lines.push(Line::from(d.as_str()));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled(
                        "[Enter] Yes",
                        Style::default().fg(theme.ok).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("   "),
                    Span::styled("[Esc] No", Style::default().fg(theme.muted)),
                ]));

                frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
            }
            Self::Error { title, body } => {
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.err))
                    .title(Span::styled(
                        format!(" {title} "),
                        Style::default().fg(theme.err).add_modifier(Modifier::BOLD),
                    ));
                let inner = block.inner(area);
                frame.render_widget(block, area);

                let lines = vec![
                    Line::from(""),
                    Line::from(body.as_str()),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("[Esc] Close", Style::default().fg(theme.muted)),
                        Span::raw("   "),
                        Span::styled("[c] Copy", Style::default().fg(theme.accent)),
                    ]),
                ];
                frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
            }
            Self::ScrollableInfo {
                title,
                body,
                scroll,
            } => {
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border))
                    .title(Span::styled(
                        format!(" {title} "),
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                let inner = block.inner(area);
                frame.render_widget(block, area);

                let lines: Vec<Line<'_>> = body.lines().map(Line::from).collect();
                frame.render_widget(
                    Paragraph::new(lines)
                        .scroll((*scroll, 0))
                        .wrap(Wrap { trim: false }),
                    inner,
                );
            }
        }
    }

    /// Handle a key event. Returns how the modal and `App` should respond.
    pub fn handle_key(&mut self, key: KeyEvent) -> ModalOutcome {
        match self {
            Self::Confirm { kind, .. } => match key.code {
                KeyCode::Enter => ModalOutcome::Confirm(kind.clone()),
                KeyCode::Esc | KeyCode::Char('n' | 'q') => ModalOutcome::Close,
                _ => ModalOutcome::KeepOpen,
            },
            Self::Error { body, .. } => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => ModalOutcome::Close,
                KeyCode::Char('c') => ModalOutcome::Copy(body.clone()),
                _ => ModalOutcome::KeepOpen,
            },
            Self::ScrollableInfo { scroll, body, .. } => {
                let max_scroll =
                    u16::try_from(body.lines().count().saturating_sub(1)).unwrap_or(u16::MAX);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => ModalOutcome::Close,
                    KeyCode::Char('j') | KeyCode::Down => {
                        *scroll = scroll.saturating_add(1).min(max_scroll);
                        ModalOutcome::KeepOpen
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        *scroll = scroll.saturating_sub(1);
                        ModalOutcome::KeepOpen
                    }
                    KeyCode::Char('c') => ModalOutcome::Copy(body.clone()),
                    _ => ModalOutcome::KeepOpen,
                }
            }
        }
    }
}

/// Compute a centered `Rect` with `percent_x`/`percent_y` of the given area.
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
