use std::time::{Duration, Instant};

/// Events produced by the event loop.
#[derive(Debug)]
pub enum Event {
    /// 20 Hz heartbeat (every 50 ms). Drives spinner animation.
    Tick,
    /// A key, mouse, or resize event from crossterm.
    Input(crossterm::event::Event),
}

/// Polls crossterm input at 20 Hz. Task completions are read by `App`
/// directly via the `mpsc` receiver; `EventLoop` only emits `Tick` and `Input`.
pub struct EventLoop {
    tick_rate: Duration,
    last_tick: Instant,
}

impl EventLoop {
    /// Build a new event loop with the hardcoded 20 Hz (50 ms) tick rate.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tick_rate: Duration::from_millis(50),
            last_tick: Instant::now(),
        }
    }

    /// Block until either a crossterm event arrives or the tick deadline passes.
    ///
    /// Returns `Input` if an event was available before the deadline, `Tick`
    /// otherwise.
    ///
    /// # Errors
    ///
    /// Returns an error if `crossterm::event::read` fails.
    pub fn next(&mut self) -> anyhow::Result<Event> {
        let elapsed = self.last_tick.elapsed();
        let timeout = self.tick_rate.saturating_sub(elapsed);

        if crossterm::event::poll(timeout)? {
            let ev = crossterm::event::read()?;
            return Ok(Event::Input(ev));
        }

        self.last_tick = Instant::now();
        Ok(Event::Tick)
    }
}

impl Default for EventLoop {
    fn default() -> Self {
        Self::new()
    }
}
