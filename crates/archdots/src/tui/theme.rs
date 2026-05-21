use ratatui::style::Color;

/// Terminal color and capability settings derived from the environment.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Primary accent color (active tabs, selected items).
    pub accent: Color,
    /// Color for success indicators.
    pub ok: Color,
    /// Color for warnings.
    pub warn: Color,
    /// Color for errors.
    pub err: Color,
    /// Color for de-emphasized text.
    pub muted: Color,
    /// Color for box borders.
    pub border: Color,
    /// `false` when `LANG`/`LC_ALL` does not include UTF-8.
    pub use_unicode: bool,
    /// `false` when `NO_COLOR` is set or `TERM=dumb`.
    #[allow(dead_code)]
    // available for callers querying terminal capabilities; colors are pre-resolved in detect()
    pub use_color: bool,
}

impl Default for Theme {
    fn default() -> Self {
        Self::detect()
    }
}

impl Theme {
    /// Detect theme capabilities from the environment.
    ///
    /// Respects `NO_COLOR`, `TERM=dumb`, and `LC_ALL`/`LANG` for Unicode
    /// (POSIX precedence: `LC_ALL` overrides `LANG`).
    #[must_use]
    pub fn detect() -> Self {
        Self::detect_with(|name| std::env::var(name).ok())
    }

    /// Like `detect` but resolves env vars via `get` instead of `std::env`.
    ///
    /// Tests pass a deterministic closure so multiple tests can run in
    /// parallel without touching the real process environment.
    pub(crate) fn detect_with<F: Fn(&str) -> Option<String>>(get: F) -> Self {
        let use_color = get("NO_COLOR").is_none() && get("TERM").is_none_or(|t| t != "dumb");

        // POSIX: LC_ALL overrides LANG.
        let use_unicode = get("LC_ALL")
            .or_else(|| get("LANG"))
            .is_some_and(|l| l.contains("UTF-8") || l.contains("utf8"));

        if use_color {
            Self {
                accent: Color::Cyan,
                ok: Color::Green,
                warn: Color::Yellow,
                err: Color::Red,
                muted: Color::DarkGray,
                border: Color::DarkGray,
                use_unicode,
                use_color,
            }
        } else {
            Self {
                accent: Color::Reset,
                ok: Color::Reset,
                warn: Color::Reset,
                err: Color::Reset,
                muted: Color::Reset,
                border: Color::Reset,
                use_unicode,
                use_color,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_disables_color() {
        let theme = Theme::detect_with(|name| match name {
            "NO_COLOR" => Some("1".into()),
            _ => None,
        });
        assert!(!theme.use_color);
        assert!(matches!(theme.accent, Color::Reset));
    }

    #[test]
    fn lang_c_disables_unicode() {
        let theme = Theme::detect_with(|name| match name {
            "LANG" => Some("C".into()),
            _ => None,
        });
        assert!(!theme.use_unicode);
    }

    #[test]
    fn lang_utf8_enables_unicode() {
        let theme = Theme::detect_with(|name| match name {
            "LANG" => Some("en_US.UTF-8".into()),
            _ => None,
        });
        assert!(theme.use_unicode);
    }

    #[test]
    fn lc_all_takes_priority_over_lang() {
        // LC_ALL=en_US.UTF-8 wins even when LANG=C
        let theme = Theme::detect_with(|name| match name {
            "LC_ALL" => Some("en_US.UTF-8".into()),
            "LANG" => Some("C".into()),
            _ => None,
        });
        assert!(theme.use_unicode, "LC_ALL must override LANG");
    }
}
