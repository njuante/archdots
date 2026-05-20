use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::tui::{app::BackgroundState, theme::Theme, ui::StatusMessage};

/// Spinner frames (Braille pattern, 10 frames at 20 Hz ≈ 0.5 s per cycle).
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// ASCII fallback spinner for `use_unicode = false`.
const SPINNER_ASCII: [&str; 4] = ["|", "/", "-", "\\"];

/// Render the one-line status bar at the bottom of the screen.
pub fn render_status_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    msg: &StatusMessage,
    background: &BackgroundState,
    theme: &Theme,
) {
    let now_millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let line = match background {
        BackgroundState::Running { kind, started, .. } => {
            let elapsed = started.elapsed().as_secs();
            let frame_idx = (now_millis / 50) as usize;
            let spinner = if theme.use_unicode {
                SPINNER_FRAMES[frame_idx % SPINNER_FRAMES.len()]
            } else {
                SPINNER_ASCII[frame_idx % SPINNER_ASCII.len()]
            };
            let kind_str = format!("{kind:?}");
            let label = kind_str
                .split_once('{')
                .map_or(kind_str.as_str(), |(l, _)| l)
                .trim();
            Line::from(vec![
                Span::styled(spinner, Style::default().fg(theme.accent)),
                Span::raw(format!(" {label}... ({elapsed}s)")),
            ])
        }
        BackgroundState::Idle => match msg {
            StatusMessage::None => Line::from(Span::styled(
                "press ? for help",
                Style::default().fg(theme.muted),
            )),
            StatusMessage::Info(s) => Line::from(Span::raw(s.as_str())),
            StatusMessage::Success(s) => {
                Line::from(Span::styled(s.as_str(), Style::default().fg(theme.ok)))
            }
            StatusMessage::Error(s) => {
                Line::from(Span::styled(s.as_str(), Style::default().fg(theme.err)))
            }
        },
    };

    frame.render_widget(Paragraph::new(line), area);
}
