//! Help overlay body builder for the `?` shortcut.

use crate::tui::views::ViewKind;

/// Build the scrollable body for the help overlay.
///
/// Combines the global keyboard shortcuts with the per-view hints returned
/// by the active view's `footer_hints()`.
pub fn build_help_body(
    active: ViewKind,
    view_hints: &[(&'static str, &'static str)],
    profiles_dir: &std::path::Path,
) -> String {
    const SEP: &str = "──────────────────────────────────";
    let mut body = String::from("archdots TUI — keyboard shortcuts\n\n");

    body.push_str(&format!("Global\n{SEP}\n"));
    for (key, desc) in [
        ("1-4", "switch tab"),
        ("Tab", "next tab"),
        ("?", "toggle this help"),
        ("q / Ctrl+C", "quit"),
    ] {
        body.push_str(&format!("  {key:<16}  {desc}\n"));
    }

    body.push('\n');
    body.push_str(&format!("{}\n{SEP}\n", active.name()));
    if view_hints.is_empty() {
        body.push_str("  (no view-specific shortcuts)\n");
    } else {
        for (key, desc) in view_hints {
            body.push_str(&format!("  {key:<16}  {desc}\n"));
        }
    }

    body.push('\n');
    body.push_str(&format!("Profiles: {}\n", profiles_dir.display()));
    body.push_str("Log:      $XDG_STATE_HOME/archdots/tui.log\n");

    body
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn help_body_contains_global_shortcuts() {
        let body = build_help_body(ViewKind::Profiles, &[], Path::new("/prof"));
        assert!(body.contains("1-4"), "must mention tab switch");
        assert!(body.contains("quit"), "must mention quit");
        assert!(body.contains('?'), "must mention help key");
        assert!(body.contains("Tab"), "must mention Tab");
    }

    #[test]
    fn help_body_includes_active_view_name() {
        let body = build_help_body(ViewKind::Snapshots, &[], Path::new("/p"));
        assert!(body.contains("Snapshots"), "must show Snapshots view name");
        let body2 = build_help_body(ViewKind::Deps, &[], Path::new("/p"));
        assert!(body2.contains("Deps"), "must show Deps view name");
    }

    #[test]
    fn help_body_includes_footer_hints() {
        let hints: &[(&'static str, &'static str)] = &[("Enter", "apply"), ("d", "diff")];
        let body = build_help_body(ViewKind::Profiles, hints, Path::new("/p"));
        assert!(body.contains("Enter"), "must include hint key");
        assert!(body.contains("apply"), "must include hint description");
        assert!(body.contains("diff"), "must include second hint");
    }

    #[test]
    fn help_body_contains_profiles_dir() {
        let body = build_help_body(ViewKind::Diff, &[], Path::new("/my/profiles"));
        assert!(body.contains("/my/profiles"), "must show profiles dir path");
    }
}
