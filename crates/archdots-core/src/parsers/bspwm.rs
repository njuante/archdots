use super::common::{extract_binary, strip_comment, LogicalLines};
use super::types::{Mention, MentionSource};

pub(super) fn parse(contents: &str) -> Vec<Mention> {
    let mut mentions = Vec::new();

    for (line_num, logical) in LogicalLines::new(contents) {
        let line = strip_comment(&logical, true).trim();
        if line.is_empty() {
            continue;
        }

        // bspc subcommands are config/rule directives, not binaries to run.
        if line.split_ascii_whitespace().next() == Some("bspc") {
            continue;
        }

        // Pattern 3 (most specific): pgrep -x X || X &
        if let Some(binary) = parse_pgrep_idiom(line) {
            mentions.push(Mention {
                binary,
                line: line_num,
                source: MentionSource::BspwmExec,
            });
            continue;
        }

        // Pattern 1: exec X [args] [&]
        if let Some(rest) = line.strip_prefix("exec ") {
            let rest = rest.trim_end_matches('&').trim();
            if let Some(binary) = extract_binary(rest) {
                mentions.push(Mention {
                    binary,
                    line: line_num,
                    source: MentionSource::BspwmExec,
                });
            }
            continue;
        }

        // Pattern 2: X [args] &
        if line.ends_with('&') {
            let cmd = line.trim_end_matches('&').trim();
            if let Some(binary) = extract_binary(cmd) {
                mentions.push(Mention {
                    binary,
                    line: line_num,
                    source: MentionSource::BspwmExec,
                });
            }
        }
    }

    mentions
}

/// Detect `pgrep -x X || X &` and return the binary from the RHS.
fn parse_pgrep_idiom(line: &str) -> Option<String> {
    let line = line.trim_end_matches('&').trim();
    let (lhs, rhs) = line.split_once("||")?;
    if !lhs.trim().starts_with("pgrep") {
        return None;
    }
    extract_binary(rhs.trim())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn binary_names(mentions: &[Mention]) -> Vec<&str> {
        mentions.iter().map(|m| m.binary.as_str()).collect()
    }

    #[test]
    fn exec_line() {
        let ms = parse("exec picom &");
        assert_eq!(binary_names(&ms), ["picom"]);
        assert_eq!(ms[0].source, MentionSource::BspwmExec);
        assert_eq!(ms[0].line, 1);
    }

    #[test]
    fn bare_background() {
        let ms = parse("picom &");
        assert_eq!(binary_names(&ms), ["picom"]);
    }

    #[test]
    fn pgrep_idiom() {
        let ms = parse("pgrep -x sxhkd || sxhkd &");
        assert_eq!(binary_names(&ms), ["sxhkd"]);
    }

    #[test]
    fn skip_bspc() {
        let ms = parse("bspc rule -a Firefox");
        assert!(ms.is_empty());
    }

    #[test]
    fn skip_comment_line() {
        let ms = parse("# exec picom &");
        assert!(ms.is_empty());
    }

    #[test]
    fn inline_comment_stripped() {
        let ms = parse("exec picom & # start compositor");
        assert_eq!(binary_names(&ms), ["picom"]);
    }

    #[test]
    fn multiple_lines() {
        let src = "exec picom &\nexec waybar &\nexec dunst &";
        let ms = parse(src);
        assert_eq!(binary_names(&ms), ["picom", "waybar", "dunst"]);
        assert_eq!(ms[0].line, 1);
        assert_eq!(ms[1].line, 2);
        assert_eq!(ms[2].line, 3);
    }

    #[test]
    fn pgrep_idiom_with_redirect() {
        // `pgrep -x sxhkd > /dev/null || sxhkd &`
        let ms = parse("pgrep -x sxhkd > /dev/null || sxhkd &");
        assert_eq!(binary_names(&ms), ["sxhkd"]);
    }

    #[test]
    fn variable_binary_skipped() {
        let ms = parse("exec $TERMINAL &");
        assert!(ms.is_empty());
    }

    #[test]
    fn continuation_line() {
        let src = "exec picom \\\n  --config ~/.config/picom.conf &";
        let ms = parse(src);
        assert_eq!(binary_names(&ms), ["picom"]);
        assert_eq!(ms[0].line, 1);
    }
}
