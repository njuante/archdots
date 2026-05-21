use similar::{ChangeTag, TextDiff};

/// Classification of a single diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffTag {
    Context,
    Added,
    Removed,
}

/// Compute a line-by-line diff between `old` and `new`.
///
/// Returns one `(tag, text)` pair per line in diff order.
/// `text` includes any trailing newline present in the source content.
pub fn compute_diff_lines(old: &str, new: &str) -> Vec<(DiffTag, String)> {
    TextDiff::from_lines(old, new)
        .iter_all_changes()
        .map(|change| {
            let tag = match change.tag() {
                ChangeTag::Equal => DiffTag::Context,
                ChangeTag::Delete => DiffTag::Removed,
                ChangeTag::Insert => DiffTag::Added,
            };
            (tag, change.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_identical_strings_all_context() {
        let lines = compute_diff_lines("hello\nworld\n", "hello\nworld\n");
        assert!(!lines.is_empty(), "must produce at least one line");
        assert!(
            lines.iter().all(|(tag, _)| *tag == DiffTag::Context),
            "identical strings must produce only Context lines"
        );
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn diff_changed_line_produces_remove_and_add() {
        let old = "line1\nline2\nline3\n";
        let new = "line1\nLINE2\nline3\n";
        let lines = compute_diff_lines(old, new);
        let removed: Vec<_> = lines
            .iter()
            .filter(|(t, _)| *t == DiffTag::Removed)
            .collect();
        let added: Vec<_> = lines.iter().filter(|(t, _)| *t == DiffTag::Added).collect();
        assert_eq!(removed.len(), 1, "one removed line");
        assert_eq!(added.len(), 1, "one added line");
        assert!(
            removed[0].1.contains("line2"),
            "removed must contain 'line2'"
        );
        assert!(added[0].1.contains("LINE2"), "added must contain 'LINE2'");
    }
}
