use super::common::{extract_binary, strip_comment, LogicalLines};
use super::types::{Mention, MentionSource};

/// Shell keywords that never represent external binaries.
const SHELL_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "select", "local", "declare", "typeset", "export", "source", ".", "unset", "set",
    "return", "exit", "shift", "trap", "function", "time", "coproc",
];

/// First-token values that signal lines we should skip entirely.
const SKIP_FIRST: &[&str] = &["[", "[[", "set", "unset", "return", "shift", "trap"];

pub(super) fn parse(contents: &str) -> Vec<Mention> {
    let mut mentions = Vec::new();

    for (line_num, logical) in LogicalLines::new(contents) {
        // Strip comments respecting shell quoting.
        let line = strip_comment(&logical, false).trim();
        if line.is_empty() {
            continue;
        }
        // Skip lines that begin with control operators.
        if line.starts_with("&&")
            || line.starts_with("||")
            || line.starts_with(';')
            || line.starts_with('|')
        {
            continue;
        }

        let first = line.split_ascii_whitespace().next().unwrap_or("");

        if SKIP_FIRST.contains(&first) {
            continue;
        }

        // alias name=CMD
        if first == "alias" {
            let rest = line["alias".len()..].trim();
            if let Some(binary) = parse_alias_rhs(rest) {
                mentions.push(Mention {
                    binary,
                    line: line_num,
                    source: MentionSource::ShellAlias,
                });
            }
            continue;
        }

        // command -v BINARY
        if first == "command" {
            let rest = line["command".len()..].trim();
            if let Some(rest) = rest.strip_prefix("-v") {
                let candidate = rest.trim().split_ascii_whitespace().next().unwrap_or("");
                if !candidate.is_empty() && !candidate.starts_with('$') {
                    mentions.push(Mention {
                        binary: candidate.to_string(),
                        line: line_num,
                        source: MentionSource::ShellProbe,
                    });
                }
            }
            continue;
        }

        // Filter shell keywords.
        if SHELL_KEYWORDS.contains(&first) {
            continue;
        }

        // Top-level invocation.
        if let Some(binary) = extract_binary(line) {
            mentions.push(Mention {
                binary,
                line: line_num,
                source: MentionSource::ShellInvoke,
            });
        }
    }

    mentions
}

/// Extract the binary from the RHS of an alias definition.
///
/// Input is everything after the `alias` keyword: e.g. `ll='eza -la'`.
fn parse_alias_rhs(rest: &str) -> Option<String> {
    // Find `=` that separates alias name from value.
    let (_, value) = rest.split_once('=')?;
    extract_binary(value)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn binary_names(mentions: &[Mention]) -> Vec<&str> {
        mentions.iter().map(|m| m.binary.as_str()).collect()
    }

    #[test]
    fn alias_single_quotes() {
        let ms = parse("alias ll='eza -la'");
        assert_eq!(binary_names(&ms), ["eza"]);
        assert_eq!(ms[0].source, MentionSource::ShellAlias);
    }

    #[test]
    fn alias_double_quotes() {
        let ms = parse(r#"alias gs="git status""#);
        assert_eq!(binary_names(&ms), ["git"]);
        assert_eq!(ms[0].source, MentionSource::ShellAlias);
    }

    #[test]
    fn command_probe() {
        let ms = parse("command -v fzf");
        assert_eq!(binary_names(&ms), ["fzf"]);
        assert_eq!(ms[0].source, MentionSource::ShellProbe);
    }

    #[test]
    fn top_level_invoke() {
        let ms = parse("starship init zsh");
        assert_eq!(binary_names(&ms), ["starship"]);
        assert_eq!(ms[0].source, MentionSource::ShellInvoke);
    }

    #[test]
    fn export_skipped() {
        let ms = parse("export EDITOR=nvim");
        assert!(ms.is_empty());
    }

    #[test]
    fn if_keyword_skipped() {
        let ms = parse("if [[ -f ~/.zshrc ]]; then");
        assert!(ms.is_empty());
    }

    #[test]
    fn source_skipped() {
        let ms = parse("source ~/.zsh_aliases");
        assert!(ms.is_empty());
    }

    #[test]
    fn inline_comment_stripped() {
        let ms = parse("starship init zsh # prompt");
        assert_eq!(binary_names(&ms), ["starship"]);
    }

    #[test]
    fn comment_in_single_quotes_preserved() {
        // The '#' inside the alias value is not a comment.
        let ms = parse("alias x='grep --color=auto # not a comment'");
        assert_eq!(binary_names(&ms), ["grep"]);
    }

    #[test]
    fn multiple_invocations() {
        let src = "starship init zsh\neval \"$(zoxide init zsh)\"";
        let ms = parse(src);
        assert_eq!(binary_names(&ms), ["starship", "eval"]);
    }

    #[test]
    fn variable_binary_in_alias_skipped() {
        let ms = parse("alias t=$TERMINAL");
        // $TERMINAL starts with '$', extract_binary returns None.
        assert!(ms.is_empty());
    }

    #[test]
    fn skip_first_tokens() {
        let ms = parse("return 0\nshift\ntrap cleanup EXIT");
        assert!(ms.is_empty());
    }
}
