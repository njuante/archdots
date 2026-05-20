/// Strip a trailing comment from `line`.
///
/// When `comment_in_quotes` is `true` (most config formats), the first `#`
/// found anywhere ends the line. When `false` (shell mode), a `#` inside
/// single or double quotes is **not** treated as a comment marker.
pub(crate) fn strip_comment(line: &str, comment_in_quotes: bool) -> &str {
    if comment_in_quotes {
        // Simple: cut at the first '#'.
        return line.find('#').map_or(line, |i| &line[..i]);
    }
    // Shell mode: track quoting state.
    let mut in_single = false;
    let mut in_double = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Return `Some(line)` when the line's first non-whitespace character is not
/// `#`. Return `None` when the line is a comment or entirely whitespace.
///
/// Used by i3/sway and sxhkd where `#` is only meaningful as the very first
/// token on a line.
pub(crate) fn strip_leading_comment(line: &str) -> Option<&str> {
    let first = line.chars().find(|c| !c.is_whitespace())?;
    if first == '#' {
        None
    } else {
        Some(line)
    }
}

/// Iterator that joins physical lines ending with `\` into logical lines.
///
/// Each item is `(first_line_number, logical_line)` where the line number is
/// 1-based and refers to the first physical line of the group. The trailing
/// backslash is removed from continuations; the next physical line's content
/// is appended directly (preserving its leading whitespace).
pub(crate) struct LogicalLines<'a> {
    lines: Vec<&'a str>,
    pos: usize,
    line_num: u32,
}

impl<'a> LogicalLines<'a> {
    pub(crate) fn new(contents: &'a str) -> Self {
        Self {
            lines: contents.lines().collect(),
            pos: 0,
            line_num: 1,
        }
    }
}

impl Iterator for LogicalLines<'_> {
    type Item = (u32, String);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.lines.len() {
            return None;
        }
        let start = self.line_num;
        let mut joined = String::new();
        loop {
            if self.pos >= self.lines.len() {
                break;
            }
            let line = self.lines[self.pos];
            self.pos += 1;
            self.line_num += 1;
            if let Some(cont) = line.strip_suffix('\\') {
                joined.push_str(cont);
            } else {
                joined.push_str(line);
                break;
            }
        }
        Some((start, joined))
    }
}

/// Extract the binary name from a command string.
///
/// Rules (applied in order):
/// 1. If `cmd` is entirely wrapped in matching single or double quotes, the
///    outer quotes are stripped before tokenising.
/// 2. The first whitespace-delimited token is taken.
/// 3. If that token starts with `$` (variable reference) → `None`.
/// 4. If that token is an absolute path (starts with `/`) → return its
///    basename.
/// 5. If that token contains `/` (relative path) → `None` (heuristic: user
///    script, not a packaged binary).
/// 6. Otherwise return the token as-is.
///
/// Never panics. Returns `None` for empty input.
pub(crate) fn extract_binary(cmd: &str) -> Option<String> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return None;
    }
    // Strip matching outer quotes wrapping the whole string.
    let inner: &str = if cmd.len() >= 2 {
        let first = cmd.as_bytes()[0];
        let last = cmd.as_bytes()[cmd.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            &cmd[1..cmd.len() - 1]
        } else {
            cmd
        }
    } else {
        cmd
    };

    let first = inner.split_ascii_whitespace().next()?;

    if first.starts_with('$') {
        return None;
    }
    if first.starts_with('/') {
        return std::path::Path::new(first)
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string);
    }
    if first.contains('/') {
        return None;
    }
    Some(first.to_string())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // strip_comment ────────────────────────────────────────────────────────────

    #[test]
    fn strip_comment_config_mode_cuts_at_hash() {
        assert_eq!(
            strip_comment("exec picom & # comment", true),
            "exec picom & "
        );
    }

    #[test]
    fn strip_comment_config_mode_no_hash() {
        assert_eq!(strip_comment("exec picom &", true), "exec picom &");
    }

    #[test]
    fn strip_comment_shell_mode_respects_single_quotes() {
        assert_eq!(
            strip_comment("alias ll='ls # not a comment'", false),
            "alias ll='ls # not a comment'"
        );
    }

    #[test]
    fn strip_comment_shell_mode_cuts_outside_quotes() {
        assert_eq!(
            strip_comment("alias ll='ls' # real comment", false),
            "alias ll='ls' "
        );
    }

    #[test]
    fn strip_comment_shell_mode_double_quotes() {
        assert_eq!(
            strip_comment(r#"foo "bar # inside" # outside"#, false),
            r#"foo "bar # inside" "#
        );
    }

    // strip_leading_comment ────────────────────────────────────────────────────

    #[test]
    fn leading_comment_hash_first() {
        assert_eq!(strip_leading_comment("# comment"), None);
    }

    #[test]
    fn leading_comment_hash_after_space() {
        assert_eq!(strip_leading_comment("  # indented comment"), None);
    }

    #[test]
    fn leading_comment_normal_line() {
        assert!(strip_leading_comment("exec picom &").is_some());
    }

    #[test]
    fn leading_comment_empty_line() {
        assert_eq!(strip_leading_comment(""), None);
    }

    // LogicalLines ─────────────────────────────────────────────────────────────

    #[test]
    fn logical_lines_no_continuation() {
        let src = "a\nb\nc";
        let items: Vec<_> = LogicalLines::new(src).collect();
        assert_eq!(
            items,
            vec![(1, "a".into()), (2, "b".into()), (3, "c".into())]
        );
    }

    #[test]
    fn logical_lines_with_continuation() {
        let src = "a \\\nb\nc";
        let items: Vec<_> = LogicalLines::new(src).collect();
        assert_eq!(items, vec![(1, "a b".into()), (3, "c".into())]);
    }

    #[test]
    fn logical_lines_line_numbers_preserved() {
        let src = "x\\\ny\nz";
        let items: Vec<_> = LogicalLines::new(src).collect();
        assert_eq!(items[0].0, 1);
        assert_eq!(items[1].0, 3);
    }

    // extract_binary ───────────────────────────────────────────────────────────

    #[test]
    fn extract_binary_simple() {
        assert_eq!(extract_binary("rofi -show drun"), Some("rofi".into()));
    }

    #[test]
    fn extract_binary_variable_returns_none() {
        assert_eq!(extract_binary("$TERMINAL"), None);
    }

    #[test]
    fn extract_binary_absolute_path_returns_basename() {
        assert_eq!(extract_binary("/usr/bin/kitty"), Some("kitty".into()));
    }

    #[test]
    fn extract_binary_relative_path_returns_none() {
        assert_eq!(extract_binary("./scripts/setup.sh"), None);
    }

    #[test]
    fn extract_binary_outer_double_quotes() {
        assert_eq!(
            extract_binary(r#""kitty --class foo""#),
            Some("kitty".into())
        );
    }

    #[test]
    fn extract_binary_outer_single_quotes() {
        assert_eq!(extract_binary("'rofi -show run'"), Some("rofi".into()));
    }

    #[test]
    fn extract_binary_empty_returns_none() {
        assert_eq!(extract_binary(""), None);
        assert_eq!(extract_binary("   "), None);
    }
}
