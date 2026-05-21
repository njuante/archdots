//! Entry point for `archdots tui`.

use std::io::Stdout;

use ratatui::{backend::CrosstermBackend, Terminal};

use crate::tui::{self, app::AppPaths, theme::Theme};

// ─── Terminal RAII guard ──────────────────────────────────────────────────────

/// Owns the terminal handle and restores raw-mode/alt-screen on drop.
///
/// This guard is critical: if the TUI panics the `Drop` impl ensures the
/// terminal is always restored, preventing the user's shell from losing echo
/// or remaining in the alternate screen.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn new() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode()
            .map_err(|e| anyhow::anyhow!("cannot enter raw mode (is this a TTY?): {e}"))?;
        let mut stdout = std::io::stdout();
        crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)
            .map_err(|e| anyhow::anyhow!("cannot enter alternate screen: {e}"))?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            self.terminal.backend_mut(),
            crossterm::terminal::LeaveAlternateScreen,
        );
    }
}

// ─── Entry point ─────────────────────────────────────────────────────────────

/// Launch the interactive TUI.
///
/// Sets up file-based logging (truncated on each launch), enters raw mode +
/// alternate screen, runs the event loop, and restores the terminal on every
/// exit path via the `TerminalGuard` `Drop` impl.
pub fn run() -> anyhow::Result<()> {
    // 1. Resolve paths.
    let home = crate::xdg::home_dir()?;
    let profiles_dir = crate::xdg::profiles_dir()?;
    let data_home = crate::xdg::data_home()?;
    let state_home = crate::xdg::state_home()?;

    // 2. Setup file-based logging (truncate on start so each session is fresh).
    //    Failure to create the log file is non-fatal: we just skip logging.
    let log_dir = state_home.join("archdots");
    let _appender_guard = if let Ok(()) = std::fs::create_dir_all(&log_dir) {
        let log_path = log_dir.join("tui.log");
        if let Ok(log_file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
        {
            let (non_blocking, guard) = tracing_appender::non_blocking(log_file);
            tracing_subscriber::fmt()
                .with_writer(non_blocking)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_env("ARCHDOTS_LOG")
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .with_target(false)
                .init();
            Some(guard)
        } else {
            None
        }
    } else {
        None
    };

    // 3. Build paths and theme.
    let paths = AppPaths {
        home,
        profiles_dir,
        data_home,
        state_home,
    };
    let theme = Theme::detect();

    // 4. Build App.
    let mut app = tui::App::new(paths, theme)?;

    // 5. Enter raw mode / alternate screen; Drop guard restores on exit.
    let mut guard = TerminalGuard::new()?;

    // 6. Run event loop.
    tui::run_loop(guard.terminal_mut(), &mut app)
}
