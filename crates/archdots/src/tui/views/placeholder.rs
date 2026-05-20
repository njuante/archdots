use ratatui::{
    layout::{Alignment, Rect},
    style::Style,
    text::{Line, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::tui::{
    action::Action,
    theme::Theme,
    views::{View, ViewCtx, ViewKind},
};

/// Placeholder view rendered for tabs not yet implemented.
///
/// Shows a centred message until the real view is built in a later session.
pub struct PlaceholderView {
    view_kind: ViewKind,
    session_number: u32,
}

impl PlaceholderView {
    #[must_use]
    pub fn new(view_kind: ViewKind, session_number: u32) -> Self {
        Self {
            view_kind,
            session_number,
        }
    }
}

impl View for PlaceholderView {
    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border))
            .title(format!(" {} ", self.view_kind.name()));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines = vec![
            Line::from(""),
            Line::from(format!("Phase 4 — Session {} pending", self.session_number)),
            Line::from(""),
            Line::from(format!("{} view will appear here", self.view_kind.name())),
            Line::from("once implemented."),
            Line::from(""),
        ];

        // Vertical centering: offset by half the available height.
        let text_height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let v_offset = inner.height.saturating_sub(text_height) / 2;
        let render_area = Rect {
            y: inner.y + v_offset,
            height: inner.height.saturating_sub(v_offset),
            ..inner
        };

        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: false }),
            render_area,
        );
    }

    fn handle_event(&mut self, _ev: &crossterm::event::Event, _ctx: &ViewCtx<'_>) -> Action {
        Action::None
    }

    fn on_focus(&mut self, _ctx: &ViewCtx<'_>) {}

    fn footer_hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("?", "help"), ("q", "quit")]
    }
}
