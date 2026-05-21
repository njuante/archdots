//! TUI module: App, events, tasks, views, and the main event loop.

pub mod action;
pub mod app;
pub mod events;
pub mod tasks;
pub mod theme;
pub mod ui;
pub mod views;

pub use app::App;

use events::EventLoop;
use ratatui::backend::Backend;
use ratatui::Terminal;

/// Run the TUI event loop until the user quits.
///
/// Each iteration: drain completed task messages, read the next event,
/// step the app state, redraw if dirty or animating, check for quit.
///
/// # Errors
///
/// Propagates any error from `EventLoop::next`, `App::step`, or
/// `Terminal::draw`.
pub fn run_loop<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> anyhow::Result<()> {
    let mut event_loop = EventLoop::new();
    loop {
        app.drain_task_messages()?;
        let event = event_loop.next()?;
        let dirty = app.step(event)?;
        if dirty || app.is_animating() {
            terminal.draw(|f| app.render(f))?;
        }
        if app.should_quit() {
            break;
        }
    }
    Ok(())
}
