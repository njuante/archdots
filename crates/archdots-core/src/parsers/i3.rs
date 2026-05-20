use super::common::{extract_binary, strip_leading_comment, LogicalLines};
use super::types::{Mention, MentionSource};

/// Parse an i3 or sway config file. Both formats share identical exec syntax.
pub(super) fn parse(contents: &str) -> Vec<Mention> {
    let mut mentions = Vec::new();

    for (line_num, logical) in LogicalLines::new(contents) {
        let Some(line) = strip_leading_comment(&logical) else {
            continue;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(binary) = try_exec(line) {
            mentions.push(Mention {
                binary,
                line: line_num,
                source: MentionSource::I3SwayExec,
            });
            continue;
        }

        if let Some(binary) = try_bindsym(line) {
            mentions.push(Mention {
                binary,
                line: line_num,
                source: MentionSource::I3SwayBindsym,
            });
        }
    }

    mentions
}

/// Match `exec [--no-startup-id] CMD` and `exec_always [--no-startup-id] CMD`.
fn try_exec(line: &str) -> Option<String> {
    let rest = if let Some(r) = line.strip_prefix("exec_always ") {
        r
    } else if let Some(r) = line.strip_prefix("exec ") {
        r
    } else {
        return None;
    };
    let rest = rest.trim();
    let rest = rest.strip_prefix("--no-startup-id").map_or(rest, str::trim);
    extract_binary(rest)
}

/// Match `bindsym KEY [--no-startup-id] exec CMD`.
fn try_bindsym(line: &str) -> Option<String> {
    let rest = line.strip_prefix("bindsym ")?;
    let words: Vec<&str> = rest.split_whitespace().collect();
    // words[0] is the key combo; find "exec" starting at words[1].
    let exec_idx = words
        .get(1..)?
        .iter()
        .position(|&w| w == "exec")
        .map(|i| i + 1)?;
    let after_exec = words.get(exec_idx + 1..)?;
    // Strip --no-startup-id if present.
    let cmd_words = if after_exec.first().copied() == Some("--no-startup-id") {
        &after_exec[1..]
    } else {
        after_exec
    };
    if cmd_words.is_empty() {
        return None;
    }
    extract_binary(&cmd_words.join(" "))
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn binary_names(mentions: &[Mention]) -> Vec<&str> {
        mentions.iter().map(|m| m.binary.as_str()).collect()
    }

    #[test]
    fn exec_simple() {
        let ms = parse("exec waybar");
        assert_eq!(binary_names(&ms), ["waybar"]);
        assert_eq!(ms[0].source, MentionSource::I3SwayExec);
    }

    #[test]
    fn exec_always() {
        let ms = parse("exec_always picom");
        assert_eq!(binary_names(&ms), ["picom"]);
        assert_eq!(ms[0].source, MentionSource::I3SwayExec);
    }

    #[test]
    fn bindsym_exec() {
        let ms = parse("bindsym $mod+d exec rofi -show drun");
        assert_eq!(binary_names(&ms), ["rofi"]);
        assert_eq!(ms[0].source, MentionSource::I3SwayBindsym);
    }

    #[test]
    fn bindsym_no_startup_id() {
        let ms = parse("bindsym $mod+Return exec --no-startup-id alacritty");
        assert_eq!(binary_names(&ms), ["alacritty"]);
    }

    #[test]
    fn comment_line_skipped() {
        let ms = parse("# exec waybar");
        assert!(ms.is_empty());
    }

    #[test]
    fn non_exec_directive_skipped() {
        let ms = parse("set $mod Mod4");
        assert!(ms.is_empty());
    }

    #[test]
    fn multiple_directives() {
        let src = "exec waybar\nexec_always picom\nbindsym $mod+d exec rofi -show drun";
        let ms = parse(src);
        assert_eq!(binary_names(&ms), ["waybar", "picom", "rofi"]);
    }

    #[test]
    fn variable_binary_skipped() {
        let ms = parse("exec $TERMINAL");
        assert!(ms.is_empty());
    }

    #[test]
    fn exec_no_startup_id_toplevel() {
        let ms = parse("exec --no-startup-id dunst");
        assert_eq!(binary_names(&ms), ["dunst"]);
        assert_eq!(ms[0].source, MentionSource::I3SwayExec);
    }
}
