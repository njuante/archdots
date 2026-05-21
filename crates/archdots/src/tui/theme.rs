#![allow(dead_code)] // Fields used in later sessions.

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
    /// Respects `NO_COLOR`, `TERM=dumb`, and `LANG`/`LC_ALL` for Unicode.
    #[must_use]
    pub fn detect() -> Self {
        let use_color = std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true);

        let use_unicode = std::env::var("LANG")
            .or_else(|_| std::env::var("LC_ALL"))
            .map(|l| l.contains("UTF-8") || l.contains("utf8"))
            .unwrap_or(false);

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
        std::env::set_var("NO_COLOR", "1");
        let theme = Theme::detect();
        std::env::remove_var("NO_COLOR");
        assert!(!theme.use_color);
        assert!(matches!(theme.accent, Color::Reset));
    }

    #[test]
    fn lang_c_disables_unicode() {
        std::env::remove_var("NO_COLOR");
        let orig_lang = std::env::var("LANG").ok();
        let orig_lc = std::env::var("LC_ALL").ok();
        std::env::set_var("LANG", "C");
        std::env::remove_var("LC_ALL");
        let theme = Theme::detect();
        // Restore
        match orig_lang {
            Some(v) => std::env::set_var("LANG", v),
            None => std::env::remove_var("LANG"),
        }
        match orig_lc {
            Some(v) => std::env::set_var("LC_ALL", v),
            None => std::env::remove_var("LC_ALL"),
        }
        assert!(!theme.use_unicode);
    }

    #[test]
    fn lang_utf8_enables_unicode() {
        std::env::remove_var("NO_COLOR");
        let orig_lang = std::env::var("LANG").ok();
        let orig_lc = std::env::var("LC_ALL").ok();
        std::env::set_var("LANG", "en_US.UTF-8");
        std::env::remove_var("LC_ALL");
        let theme = Theme::detect();
        // Restore
        match orig_lang {
            Some(v) => std::env::set_var("LANG", v),
            None => std::env::remove_var("LANG"),
        }
        match orig_lc {
            Some(v) => std::env::set_var("LC_ALL", v),
            None => std::env::remove_var("LC_ALL"),
        }
        assert!(theme.use_unicode);
    }
}
