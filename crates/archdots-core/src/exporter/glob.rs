//! Hand-rolled Unix glob matcher for export path filters.
//!
//! Supports `*` (any chars except `/`), `?` (one char, not `/`), and `**`
//! (any chars including `/`). Used by `--allow-path`, `--allow-binary`, and
//! `--allow-secret`.

/// Returns `true` when `pattern` matches `path`.
///
/// Matching rules (unix paths, no filesystem access):
/// - `*` matches any sequence of characters **except** `/`.
/// - `?` matches exactly one character that is **not** `/`.
/// - `**/` matches zero or more path components (including their trailing `/`).
/// - `**` matches any sequence of characters including `/`.
/// - All other characters are matched literally.
#[must_use]
pub(super) fn glob_matches(pattern: &str, path: &str) -> bool {
    do_match(pattern.as_bytes(), path.as_bytes())
}

fn do_match(pat: &[u8], path: &[u8]) -> bool {
    match (pat, path) {
        // Both exhausted: full match.
        ([], []) => true,
        // `**/` — zero or more path components + separator.
        ([b'*', b'*', b'/', rest @ ..], _) => {
            // Try matching rest against current position (zero dirs consumed).
            do_match(rest, path) || (!path.is_empty() && do_match(pat, &path[1..]))
        }
        // `**` at end or before a non-`/` — matches any remaining chars.
        ([b'*', b'*', rest @ ..], _) => {
            do_match(rest, path) || (!path.is_empty() && do_match(pat, &path[1..]))
        }
        // `*` — matches any chars except `/`.
        ([b'*', rest @ ..], _) => {
            do_match(rest, path)
                || (!path.is_empty() && path[0] != b'/' && do_match(pat, &path[1..]))
        }
        // `?` — matches exactly one non-`/` char.
        ([b'?', rest @ ..], [ph, path_rest @ ..]) if *ph != b'/' => do_match(rest, path_rest),
        // Literal match.
        ([pc, rest @ ..], [ph, path_rest @ ..]) if pc == ph => do_match(rest, path_rest),
        // Pattern exhausted with remaining path, `?` with empty/`/` path,
        // literal mismatch, or any other non-matching combination.
        _ => false,
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::glob_matches;

    #[test]
    fn star_matches_segment_not_slash() {
        assert!(glob_matches("*.toml", "foo.toml"));
        assert!(
            !glob_matches("*.toml", "foo/bar.toml"),
            "* must not cross a path separator"
        );
    }

    #[test]
    fn double_star_matches_multiple_segments() {
        assert!(glob_matches(
            ".config/**/*.conf",
            ".config/foo/bar/baz.conf"
        ));
        assert!(glob_matches(
            ".config/**/*.conf",
            ".config/hypr/hyprland.conf"
        ));
        assert!(
            !glob_matches(".config/**/*.conf", ".local/foo.conf"),
            "prefix must still match"
        );
    }

    #[test]
    fn double_star_slash_matches_zero_dirs() {
        // **/foo should match foo at the root too.
        assert!(glob_matches("**/foo.conf", "foo.conf"));
        assert!(glob_matches("**/foo.conf", "a/b/foo.conf"));
    }

    #[test]
    fn question_mark_matches_one_non_slash_char() {
        assert!(glob_matches("?.toml", "a.toml"));
        assert!(
            !glob_matches("?.toml", "ab.toml"),
            "? matches exactly one char"
        );
        assert!(!glob_matches("?.toml", "/.toml"), "? must not match /");
    }

    #[test]
    fn no_match_returns_false() {
        assert!(!glob_matches("*.rs", "foo.toml"));
        assert!(!glob_matches(".ssh/**", ".config/foo"));
        assert!(!glob_matches("exact", "exacts"));
    }

    #[test]
    fn exact_match() {
        assert!(glob_matches(".netrc", ".netrc"));
        assert!(!glob_matches(".netrc", ".netrc2"));
        assert!(!glob_matches(".netrc", ".netr"));
    }

    #[test]
    fn double_star_at_end() {
        assert!(glob_matches(".ssh/**", ".ssh/id_ed25519"));
        assert!(glob_matches(".ssh/**", ".ssh/subdir/key.pem"));
        assert!(!glob_matches(".ssh/**", ".config/foo"));
    }
}
