use super::common::{extract_binary, LogicalLines};
use super::types::{Mention, MentionSource};

/// Parse an sxhkdrc file.
///
/// sxhkd format: a keybinding line (starts at column 0) followed by one or
/// more indented command lines. We only emit mentions for command lines — the
/// first token after stripping leading whitespace.
pub(super) fn parse(contents: &str) -> Vec<Mention> {
    let mut mentions = Vec::new();

    for (line_num, logical) in LogicalLines::new(contents) {
        // Command lines start with whitespace; skip non-indented lines.
        if !logical.starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let trimmed = logical.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(binary) = extract_binary(trimmed) {
            mentions.push(Mention {
                binary,
                line: line_num,
                source: MentionSource::SxhkdCommand,
            });
        }
    }

    mentions
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn binary_names(mentions: &[Mention]) -> Vec<&str> {
        mentions.iter().map(|m| m.binary.as_str()).collect()
    }

    #[test]
    fn single_binding() {
        let src = "super + d\n    rofi -show drun";
        let ms = parse(src);
        assert_eq!(binary_names(&ms), ["rofi"]);
        assert_eq!(ms[0].source, MentionSource::SxhkdCommand);
        assert_eq!(ms[0].line, 2);
    }

    #[test]
    fn multiple_bindings() {
        let src = "super + d\n    rofi -show drun\n\nsuper + Return\n    alacritty";
        let ms = parse(src);
        assert_eq!(binary_names(&ms), ["rofi", "alacritty"]);
    }

    #[test]
    fn continuation_command() {
        let src = "super + d\n    rofi \\\n    -show drun";
        let ms = parse(src);
        assert_eq!(binary_names(&ms), ["rofi"]);
        assert_eq!(ms[0].line, 2);
    }

    #[test]
    fn leading_comment_skipped() {
        // A '#' as first non-ws on an indented line → skip
        let src = "super + d\n    # rofi -show drun";
        let ms = parse(src);
        assert!(ms.is_empty());
    }

    #[test]
    fn fifty_mentions_preserved() {
        let mut src = String::new();
        for i in 0..50u32 {
            src.push_str(&format!("super + {i}\n    rofi -show drun\n\n"));
        }
        let ms = parse(&src);
        assert_eq!(ms.len(), 50, "parser must not deduplicate");
        assert!(ms.iter().all(|m| m.binary == "rofi"));
    }

    #[test]
    fn variable_binary_skipped() {
        let src = "super + t\n    $TERMINAL";
        let ms = parse(src);
        assert!(ms.is_empty());
    }

    #[test]
    fn key_line_ignored() {
        // Keybinding lines (not indented) are not mentions.
        let src = "super + d";
        let ms = parse(src);
        assert!(ms.is_empty());
    }
}
