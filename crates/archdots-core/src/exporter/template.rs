//! Minimal template renderer for export files.
//!
//! Supports exactly three constructs:
//! - `{var}` — simple substitution
//! - `{IF cond}...{ENDIF}` — conditional block
//! - `{FOR item IN list}...{ENDFOR}` — iteration
//!
//! Truthy test: a `List` is truthy when non-empty; a `Bool` is its value;
//! a `Str` is truthy when non-empty.

use std::collections::HashMap;

use crate::error::ExportError;

// ── public types ──────────────────────────────────────────────────────────────

/// Holds the variables available during template rendering.
pub(crate) struct TemplateContext {
    vars: HashMap<String, TemplateValue>,
}

/// A value in the template context.
pub(crate) enum TemplateValue {
    /// String inserted verbatim (caller is responsible for escaping).
    Str(String),
    /// Boolean for `{IF cond}` tests.
    Bool(bool),
    /// List of strings for `{FOR item IN list}` and `{IF cond}` tests.
    List(Vec<String>),
}

impl TemplateContext {
    pub(crate) fn new() -> Self {
        Self {
            vars: HashMap::new(),
        }
    }

    pub(crate) fn set_str(&mut self, key: impl Into<String>, val: impl Into<String>) {
        self.vars.insert(key.into(), TemplateValue::Str(val.into()));
    }

    pub(crate) fn set_bool(&mut self, key: impl Into<String>, val: bool) {
        self.vars.insert(key.into(), TemplateValue::Bool(val));
    }

    pub(crate) fn set_list(&mut self, key: impl Into<String>, val: Vec<String>) {
        self.vars.insert(key.into(), TemplateValue::List(val));
    }
}

// ── public render entry point ─────────────────────────────────────────────────

/// Render `template` with `ctx`, returning the substituted string.
///
/// An unresolved placeholder returns [`ExportError::TemplateRender`].
pub(crate) fn render(template: &str, ctx: &TemplateContext) -> Result<String, ExportError> {
    render_inner(template, ctx, None)
}

// ── Markdown escape utilities ─────────────────────────────────────────────────

/// Escape user-controlled content for Markdown **inline** positions (table cells,
/// headings, inline spans). Prevents pipe injection and newline-based row
/// injection. Call this before inserting any user string into `{var}` used in
/// a table cell.
pub(crate) fn escape_md_inline(s: &str) -> String {
    s.replace('\r', "").replace('\n', " ").replace('|', r"\|")
}

/// Escape user-controlled content for Markdown **block** positions (paragraphs).
/// Prevents heading injection by prepending `\` to `#` at the start of each line.
pub(crate) fn escape_md_block(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, line) in s.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed_start = line.trim_start_matches([' ', '\t']);
        if trimmed_start.starts_with('#') {
            let leading = &line[..line.len() - trimmed_start.len()];
            out.push_str(leading);
            out.push('\\');
            out.push_str(trimmed_start);
        } else {
            out.push_str(line);
        }
    }
    out
}

// ── private rendering engine ──────────────────────────────────────────────────

/// Recursive renderer. `loop_var` holds the current `(name, value)` pair for
/// the innermost `FOR` iteration body.
fn render_inner(
    template: &str,
    ctx: &TemplateContext,
    loop_var: Option<(&str, &str)>,
) -> Result<String, ExportError> {
    let mut out = String::with_capacity(template.len());
    let mut remaining = template;

    loop {
        match remaining.find('{') {
            None => {
                out.push_str(remaining);
                break;
            }
            Some(pos) => {
                out.push_str(&remaining[..pos]);
                remaining = &remaining[pos..];

                if remaining.starts_with("{{") {
                    // Escaped literal `{`.
                    out.push('{');
                    remaining = &remaining[2..];
                } else if remaining.starts_with("{IF ") {
                    let (cond_var, inner, consumed) = parse_block(remaining, "IF", "ENDIF")?;
                    if is_truthy(&cond_var, ctx, loop_var)? {
                        out.push_str(&render_inner(inner, ctx, loop_var)?);
                    }
                    remaining = &remaining[consumed..];
                } else if remaining.starts_with("{FOR ") {
                    let (item_var, list_var, inner, consumed) = parse_for(remaining)?;
                    let list = get_list_values(&list_var, ctx)?;
                    for item in &list {
                        out.push_str(&render_inner(inner, ctx, Some((&item_var, item)))?);
                    }
                    remaining = &remaining[consumed..];
                } else if remaining.starts_with("{ENDIF}") || remaining.starts_with("{ENDFOR}") {
                    return Err(ExportError::TemplateRender(
                        "unexpected block end marker (ENDIF/ENDFOR without matching open)"
                            .to_string(),
                    ));
                } else {
                    // Simple variable substitution {varname}
                    let close = remaining.find('}').ok_or_else(|| {
                        ExportError::TemplateRender("unclosed '{' in template".to_string())
                    })?;
                    let var_name = &remaining[1..close];
                    let value = get_str_value(var_name, ctx, loop_var)?;
                    out.push_str(&value);
                    remaining = &remaining[close + 1..];
                }
            }
        }
    }

    Ok(out)
}

/// Parse a `{IF cond}inner{ENDIF}` or `{FOR ...}inner{ENDFOR}` block.
///
/// Returns `(condition_or_header_content, inner_slice, total_bytes_consumed)`.
fn parse_block<'a>(
    s: &'a str,
    open_kw: &str,
    close_tag_name: &str,
) -> Result<(String, &'a str, usize), ExportError> {
    // s starts with e.g. "{IF cond}"
    let header_close = s
        .find('}')
        .ok_or_else(|| ExportError::TemplateRender(format!("unclosed {{{open_kw} block header")))?;
    // After "{IF " (4 chars) or "{FOR " (5 chars) — handled separately for FOR.
    let skip = open_kw.len() + 2; // '{{' + kw + ' '
    let cond = s[skip..header_close].trim().to_string();

    let after_header = &s[header_close + 1..];
    let close_tag = format!("{{{close_tag_name}}}");
    let inner_end = after_header
        .find(close_tag.as_str())
        .ok_or_else(|| ExportError::TemplateRender(format!("missing {close_tag}")))?;
    let inner = &after_header[..inner_end];
    let consumed = header_close + 1 + inner_end + close_tag.len();

    Ok((cond, inner, consumed))
}

/// Parse a `{FOR item IN list}inner{ENDFOR}` block.
///
/// Returns `(item_var, list_var, inner_slice, total_bytes_consumed)`.
fn parse_for(s: &str) -> Result<(String, String, &str, usize), ExportError> {
    // s starts with "{FOR item IN list}"
    let header_close = s
        .find('}')
        .ok_or_else(|| ExportError::TemplateRender("unclosed {FOR block header".to_string()))?;
    let header_content = s[5..header_close].trim(); // "{FOR " = 5 chars

    let parts: Vec<&str> = header_content.splitn(3, ' ').collect();
    if parts.len() != 3 || parts[1] != "IN" {
        return Err(ExportError::TemplateRender(format!(
            "invalid FOR syntax: {{FOR {header_content}}}"
        )));
    }
    let item_var = parts[0].to_string();
    let list_var = parts[2].to_string();

    let after_header = &s[header_close + 1..];
    let close_tag = "{ENDFOR}";
    let inner_end = after_header
        .find(close_tag)
        .ok_or_else(|| ExportError::TemplateRender("missing {ENDFOR}".to_string()))?;
    let inner = &after_header[..inner_end];
    let consumed = header_close + 1 + inner_end + close_tag.len();

    Ok((item_var, list_var, inner, consumed))
}

fn is_truthy(
    var_name: &str,
    ctx: &TemplateContext,
    loop_var: Option<(&str, &str)>,
) -> Result<bool, ExportError> {
    if let Some((lv, lval)) = loop_var {
        if var_name == lv {
            return Ok(!lval.is_empty());
        }
    }
    match ctx.vars.get(var_name) {
        Some(TemplateValue::Bool(b)) => Ok(*b),
        Some(TemplateValue::List(l)) => Ok(!l.is_empty()),
        Some(TemplateValue::Str(s)) => Ok(!s.is_empty()),
        None => Err(ExportError::TemplateRender(format!(
            "unresolved condition variable: {{{var_name}}}"
        ))),
    }
}

fn get_list_values(var_name: &str, ctx: &TemplateContext) -> Result<Vec<String>, ExportError> {
    match ctx.vars.get(var_name) {
        Some(TemplateValue::List(l)) => Ok(l.clone()),
        Some(_) => Err(ExportError::TemplateRender(format!(
            "variable '{var_name}' is not a list"
        ))),
        None => Err(ExportError::TemplateRender(format!(
            "unresolved list variable: {{{var_name}}}"
        ))),
    }
}

fn get_str_value(
    var_name: &str,
    ctx: &TemplateContext,
    loop_var: Option<(&str, &str)>,
) -> Result<String, ExportError> {
    if let Some((lv, lval)) = loop_var {
        if var_name == lv {
            return Ok(lval.to_string());
        }
    }
    match ctx.vars.get(var_name) {
        Some(TemplateValue::Str(s)) => Ok(s.clone()),
        Some(TemplateValue::Bool(b)) => Ok(b.to_string()),
        Some(TemplateValue::List(_)) => Err(ExportError::TemplateRender(format!(
            "variable '{var_name}' is a list, not a string; use {{FOR}}"
        ))),
        None => Err(ExportError::TemplateRender(format!(
            "unresolved placeholder: {{{var_name}}}"
        ))),
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{escape_md_block, escape_md_inline, render, TemplateContext};

    fn ctx_with_str(key: &str, val: &str) -> TemplateContext {
        let mut c = TemplateContext::new();
        c.set_str(key, val);
        c
    }

    // ── substitution ─────────────────────────────────────────────────────────

    #[test]
    fn simple_substitution() {
        let ctx = ctx_with_str("name", "hyprland-rice");
        let out = render("# {name}\n", &ctx).unwrap();
        assert_eq!(out, "# hyprland-rice\n");
    }

    #[test]
    fn unresolved_placeholder_is_error() {
        let ctx = TemplateContext::new();
        let err = render("{missing}", &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unresolved") && msg.contains("missing"),
            "{msg}"
        );
    }

    // ── IF with Bool ─────────────────────────────────────────────────────────

    #[test]
    fn if_bool_true_renders_inner() {
        let mut ctx = TemplateContext::new();
        ctx.set_bool("flag", true);
        let out = render("A{IF flag}B{ENDIF}C", &ctx).unwrap();
        assert_eq!(out, "ABC");
    }

    #[test]
    fn if_bool_false_skips_inner() {
        let mut ctx = TemplateContext::new();
        ctx.set_bool("flag", false);
        let out = render("A{IF flag}B{ENDIF}C", &ctx).unwrap();
        assert_eq!(out, "AC");
    }

    // ── IF with List ─────────────────────────────────────────────────────────

    #[test]
    fn if_nonempty_list_is_truthy() {
        let mut ctx = TemplateContext::new();
        ctx.set_list("items", vec!["a".to_string()]);
        let out = render("{IF items}yes{ENDIF}", &ctx).unwrap();
        assert_eq!(out, "yes");
    }

    #[test]
    fn if_empty_list_is_falsy() {
        let mut ctx = TemplateContext::new();
        ctx.set_list("items", vec![]);
        let out = render("{IF items}yes{ENDIF}", &ctx).unwrap();
        assert_eq!(out, "");
    }

    // ── FOR ──────────────────────────────────────────────────────────────────

    #[test]
    fn for_iterates_over_list() {
        let mut ctx = TemplateContext::new();
        ctx.set_list("rows", vec!["r1".to_string(), "r2".to_string()]);
        let out = render("{FOR r IN rows}| {r} |\n{ENDFOR}", &ctx).unwrap();
        assert_eq!(out, "| r1 |\n| r2 |\n");
    }

    #[test]
    fn for_empty_list_renders_nothing() {
        let mut ctx = TemplateContext::new();
        ctx.set_list("rows", vec![]);
        let out = render("A{FOR r IN rows}{r}{ENDFOR}B", &ctx).unwrap();
        assert_eq!(out, "AB");
    }

    // ── Markdown escape ───────────────────────────────────────────────────────

    #[test]
    fn escape_md_inline_escapes_pipe() {
        let s = escape_md_inline("foo | bar");
        assert_eq!(s, r"foo \| bar");
    }

    #[test]
    fn escape_md_inline_collapses_newline() {
        let s = escape_md_inline("line1\nline2");
        assert_eq!(s, "line1 line2");
    }

    #[test]
    fn escape_md_block_escapes_heading() {
        let s = escape_md_block("# pwned\n## injected\nnormal line");
        assert!(
            !s.contains("\n# ") && !s.starts_with("# "),
            "heading must be escaped; got: {s:?}"
        );
        assert!(s.contains(r"\# pwned"), "must have escaped heading");
    }

    #[test]
    fn escape_md_block_leaves_normal_lines_intact() {
        let s = escape_md_block("just a paragraph\nwith two lines");
        assert_eq!(s, "just a paragraph\nwith two lines");
    }

    #[test]
    fn user_description_with_heading_does_not_inject_markdown() {
        // Simulate what render_readme does: escape description then substitute.
        let raw_description = "# pwned\n## also injected";
        let escaped = escape_md_block(raw_description);
        let mut ctx = TemplateContext::new();
        ctx.set_str("desc", escaped);
        let out = render("{desc}", &ctx).unwrap();
        // Output must not contain a bare Markdown heading.
        for line in out.lines() {
            let trimmed = line.trim_start();
            assert!(
                !trimmed.starts_with("# ") && !trimmed.starts_with("## "),
                "heading injection must be prevented; line: {line:?}"
            );
        }
    }

    #[test]
    fn user_author_with_pipe_does_not_break_table() {
        let raw_author = "Alice | Malicious";
        let escaped = escape_md_inline(raw_author);
        let mut ctx = TemplateContext::new();
        ctx.set_str("author", escaped);
        let out = render("| Author | {author} |", &ctx).unwrap();
        // The injected `|` must be preceded by a backslash escape.
        assert!(
            out.contains(r"\|"),
            "pipe in user content must be escaped as \\|; got: {out:?}"
        );
        // And the raw unescaped sequence "Alice | Malicious" must not appear.
        assert!(
            !out.contains("Alice | Malicious"),
            "unescaped pipe must not appear; got: {out:?}"
        );
    }
}
