//! Config-file parsers for common ricing dotfiles.
//!
//! The central entry points are [`parse`] (run a specific parser) and
//! [`infer_kind`] (determine which parser to use from a file path).
//!
//! All parsers are **lenient by design**: malformed input degrades to a
//! partial or empty result — they never return an error.

mod bspwm;
mod common;
mod hyprland;
mod i3;
mod shell;
mod sxhkd;
mod types;

pub use types::{Mention, MentionSource, ParserKind};

use std::path::Path;

/// Run the appropriate parser for `kind` on `contents`.
///
/// Never errors. Malformed or unrecognised input degrades to a smaller
/// (or empty) `Vec`.
#[must_use]
pub fn parse(kind: ParserKind, contents: &str) -> Vec<Mention> {
    match kind {
        ParserKind::Bspwm => bspwm::parse(contents),
        ParserKind::Sxhkd => sxhkd::parse(contents),
        ParserKind::Hyprland => hyprland::parse(contents),
        ParserKind::I3 | ParserKind::Sway => i3::parse(contents),
        ParserKind::Zsh | ParserKind::Bash | ParserKind::Shell => shell::parse(contents),
    }
}

/// Infer the parser kind from a file path.
///
/// Returns `None` when the path is not recognised as a parseable config file.
#[must_use]
pub fn infer_kind(path: &Path) -> Option<ParserKind> {
    let fname = path.file_name()?.to_str()?;
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str());

    match fname {
        "bspwmrc" => return Some(ParserKind::Bspwm),
        "sxhkdrc" => return Some(ParserKind::Sxhkd),
        "hyprland.conf" => return Some(ParserKind::Hyprland),
        ".zshrc" | ".zshenv" | ".zprofile" | ".zlogin" => return Some(ParserKind::Zsh),
        ".bashrc" | ".bash_profile" | ".profile" => return Some(ParserKind::Bash),
        ".xprofile" | ".xinitrc" => return Some(ParserKind::Shell),
        _ => {}
    }

    match (parent, fname) {
        (Some("i3"), "config") => Some(ParserKind::I3),
        (Some("sway"), "config") => Some(ParserKind::Sway),
        _ => None,
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn infer_bspwm() {
        assert_eq!(
            infer_kind(&p("/home/user/.config/bspwm/bspwmrc")),
            Some(ParserKind::Bspwm)
        );
    }

    #[test]
    fn infer_sxhkd() {
        assert_eq!(
            infer_kind(&p("/home/user/.config/sxhkd/sxhkdrc")),
            Some(ParserKind::Sxhkd)
        );
    }

    #[test]
    fn infer_hyprland() {
        assert_eq!(
            infer_kind(&p("/home/user/.config/hypr/hyprland.conf")),
            Some(ParserKind::Hyprland)
        );
    }

    #[test]
    fn infer_i3() {
        assert_eq!(
            infer_kind(&p("/home/user/.config/i3/config")),
            Some(ParserKind::I3)
        );
    }

    #[test]
    fn infer_sway() {
        assert_eq!(
            infer_kind(&p("/home/user/.config/sway/config")),
            Some(ParserKind::Sway)
        );
    }

    #[test]
    fn infer_zshrc() {
        assert_eq!(infer_kind(&p("/home/user/.zshrc")), Some(ParserKind::Zsh));
    }

    #[test]
    fn infer_bashrc() {
        assert_eq!(infer_kind(&p("/home/user/.bashrc")), Some(ParserKind::Bash));
    }

    #[test]
    fn infer_xprofile() {
        assert_eq!(
            infer_kind(&p("/home/user/.xprofile")),
            Some(ParserKind::Shell)
        );
    }

    #[test]
    fn infer_unknown_returns_none() {
        assert_eq!(infer_kind(&p("/home/user/.config/foo/bar")), None);
        assert_eq!(infer_kind(&p("/etc/passwd")), None);
    }

    #[test]
    fn parse_dispatches_i3_and_sway_identically() {
        let src = "exec waybar";
        let i3_ms = parse(ParserKind::I3, src);
        let sway_ms = parse(ParserKind::Sway, src);
        assert_eq!(i3_ms, sway_ms);
        assert_eq!(i3_ms[0].binary, "waybar");
    }

    #[test]
    fn parse_dispatches_shell_kinds() {
        let src = "starship init zsh";
        assert_eq!(parse(ParserKind::Zsh, src)[0].binary, "starship");
        assert_eq!(parse(ParserKind::Bash, src)[0].binary, "starship");
        assert_eq!(parse(ParserKind::Shell, src)[0].binary, "starship");
    }
}
