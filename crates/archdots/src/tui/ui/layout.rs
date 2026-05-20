use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Split `area` into the standard three-row layout:
/// `[top_bar (3 lines), body (fill), status_bar (1 line)]`.
#[must_use]
pub fn main_chunks(area: Rect) -> [Rect; 3] {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    [chunks[0], chunks[1], chunks[2]]
}
