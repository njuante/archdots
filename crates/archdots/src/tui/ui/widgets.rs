#![allow(dead_code)] // Methods used in later sessions.

/// Spinner state for animated indicators.
///
/// The frame index is incremented by the caller each tick; the widget
/// just renders the current frame.
#[derive(Debug, Default, Clone, Copy)]
pub struct Spinner {
    frame: usize,
}

impl Spinner {
    /// Advance to the next animation frame.
    pub fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    /// Return the current spinner glyph, using Unicode or ASCII depending on
    /// the `use_unicode` flag.
    #[must_use]
    pub fn glyph(self, use_unicode: bool) -> &'static str {
        if use_unicode {
            const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            FRAMES[self.frame % FRAMES.len()]
        } else {
            const ASCII: [&str; 4] = ["|", "/", "-", "\\"];
            ASCII[self.frame % ASCII.len()]
        }
    }
}
