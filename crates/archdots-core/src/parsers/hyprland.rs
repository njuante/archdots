use super::common::{extract_binary, strip_comment, LogicalLines};
use super::types::{Mention, MentionSource};

pub(super) fn parse(contents: &str) -> Vec<Mention> {
    let mut mentions = Vec::new();

    for (line_num, logical) in LogicalLines::new(contents) {
        let line = strip_comment(&logical, true).trim();
        if line.is_empty() {
            continue;
        }

        // All hyprland directives use `key = value`.
        let Some((raw_key, value)) = line.split_once('=') else {
            continue;
        };
        let key = raw_key.trim();
        let value = value.trim();

        match key {
            "exec" => {
                if let Some(b) = extract_binary(value) {
                    mentions.push(Mention {
                        binary: b,
                        line: line_num,
                        source: MentionSource::HyprlandExec,
                    });
                }
            }
            "exec-once" => {
                if let Some(b) = extract_binary(value) {
                    mentions.push(Mention {
                        binary: b,
                        line: line_num,
                        source: MentionSource::HyprlandExecOnce,
                    });
                }
            }
            k if is_bind_key(k) => {
                // Format: MODS, KEY, dispatch, args
                // We only care about `exec` dispatches.
                let parts: Vec<&str> = value.splitn(4, ',').collect();
                if parts.len() >= 4 && parts[2].trim().eq_ignore_ascii_case("exec") {
                    if let Some(b) = extract_binary(parts[3].trim()) {
                        mentions.push(Mention {
                            binary: b,
                            line: line_num,
                            source: MentionSource::HyprlandExec,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    mentions
}

fn is_bind_key(key: &str) -> bool {
    matches!(
        key,
        "bind" | "bindl" | "bindle" | "bindm" | "binde" | "bindr"
    )
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
        let ms = parse("exec = waybar");
        assert_eq!(binary_names(&ms), ["waybar"]);
        assert_eq!(ms[0].source, MentionSource::HyprlandExec);
    }

    #[test]
    fn exec_once() {
        let ms = parse("exec-once = swww init");
        assert_eq!(binary_names(&ms), ["swww"]);
        assert_eq!(ms[0].source, MentionSource::HyprlandExecOnce);
        assert_eq!(ms[0].line, 1);
    }

    #[test]
    fn bind_exec() {
        let ms = parse("bind = SUPER, Q, exec, kitty");
        assert_eq!(binary_names(&ms), ["kitty"]);
        assert_eq!(ms[0].source, MentionSource::HyprlandExec);
    }

    #[test]
    fn bindl_exec() {
        let ms = parse("bindl = , XF86AudioMute, exec, pamixer --toggle-mute");
        assert_eq!(binary_names(&ms), ["pamixer"]);
    }

    #[test]
    fn variable_skipped() {
        let ms = parse("exec = $TERMINAL");
        assert!(ms.is_empty());
    }

    #[test]
    fn quoted_command() {
        let ms = parse(r#"exec = "kitty --class foo""#);
        assert_eq!(binary_names(&ms), ["kitty"]);
    }

    #[test]
    fn inline_comment_stripped() {
        let ms = parse("exec = waybar # status bar");
        assert_eq!(binary_names(&ms), ["waybar"]);
    }

    #[test]
    fn non_exec_directive_skipped() {
        let ms = parse("monitor = DP-1, 1920x1080, 0x0, 1");
        assert!(ms.is_empty());
    }

    #[test]
    fn multiple_execs() {
        let src = "exec-once = swww init\nexec = waybar\nbind = SUPER, Q, exec, kitty";
        let ms = parse(src);
        assert_eq!(binary_names(&ms), ["swww", "waybar", "kitty"]);
    }

    #[test]
    fn bind_non_exec_dispatch_skipped() {
        let ms = parse("bind = SUPER, F, fullscreen, 0");
        assert!(ms.is_empty());
    }
}
