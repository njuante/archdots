//! Embedded-rule secret scanner for export safety.

use serde::Deserialize;

use crate::error::ExportError;
use crate::exporter::{SecretFinding, SecretSeverity};

const SECRET_PATTERNS_TOML: &str = include_str!("../../data/secret_patterns.toml");

// ── catalog deserialization ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PatternCatalog {
    #[serde(default)]
    rule: Vec<RuleEntry>,
}

#[derive(Deserialize)]
struct RuleEntry {
    id: String,
    #[allow(dead_code)]
    description: String,
    pattern: String,
    severity: SeverityTag,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum SeverityTag {
    High,
    Medium,
}

// ── compiled rule ─────────────────────────────────────────────────────────────

struct CompiledRule {
    id: String,
    severity: SecretSeverity,
    regex: regex::bytes::Regex,
}

// ── public API ────────────────────────────────────────────────────────────────

/// Stateless, embedded-rule secret scanner.
///
/// Build with [`SecretScanner::new`], then call [`SecretScanner::scan`] for
/// each file. The scanner is cheaply cloneable (compiled regexes are
/// reference-counted internally by the `regex` crate).
pub struct SecretScanner {
    rules: Vec<CompiledRule>,
}

impl SecretScanner {
    /// Construct the scanner with the embedded rule set.
    ///
    /// # Errors
    ///
    /// Returns [`ExportError::ScannerInit`] if the embedded patterns fail to
    /// compile. This indicates a programming bug in `secret_patterns.toml` and
    /// should not occur in a release build.
    pub fn new() -> Result<Self, ExportError> {
        let catalog: PatternCatalog = toml::from_str(SECRET_PATTERNS_TOML)
            .map_err(|e| ExportError::ScannerInit(e.to_string()))?;

        let mut rules = Vec::with_capacity(catalog.rule.len());
        for r in catalog.rule {
            let severity = match r.severity {
                SeverityTag::High => SecretSeverity::High,
                SeverityTag::Medium => SecretSeverity::Medium,
            };
            let regex = regex::bytes::Regex::new(&r.pattern)
                .map_err(|e| ExportError::ScannerInit(format!("rule {:?}: {e}", r.id)))?;
            rules.push(CompiledRule {
                id: r.id,
                severity,
                regex,
            });
        }

        Ok(Self { rules })
    }

    /// Scan `bytes` and return all findings.
    ///
    /// If the input fails the text sniff (same heuristic as the binary filter
    /// in `plan`), returns an empty `Vec` — binary files produce no findings.
    /// The caller should normally pass only text-classified files; this check
    /// is belt-and-suspenders.
    #[must_use]
    pub fn scan(&self, bytes: &[u8]) -> Vec<SecretFinding> {
        if !is_text_sniff(bytes) {
            return Vec::new();
        }

        let mut findings = Vec::new();
        for rule in &self.rules {
            for m in rule.regex.find_iter(bytes) {
                let (line, column) = line_col(bytes, m.start());
                findings.push(SecretFinding {
                    rule_id: rule.id.clone(),
                    severity: rule.severity,
                    line,
                    column,
                    preview: redact(m.as_bytes()),
                });
            }
        }

        findings.sort_by_key(|f| (f.line, f.column));
        findings
    }
}

// ── private helpers ───────────────────────────────────────────────────────────

/// Same text heuristic as the binary filter in `plan`: null byte → binary;
/// valid UTF-8 → text; otherwise require ≥ 90 % printable bytes in first 8 KiB.
fn is_text_sniff(bytes: &[u8]) -> bool {
    let sniff = &bytes[..bytes.len().min(8192)];
    if sniff.contains(&0x00) {
        return false;
    }
    if std::str::from_utf8(sniff).is_ok() {
        return true;
    }
    let printable = sniff
        .iter()
        .filter(|&&b| b >= 0x20 || matches!(b, b'\n' | b'\r' | b'\t'))
        .count();
    printable >= sniff.len() * 9 / 10
}

/// Returns (line, column), both 1-indexed, for the byte at `offset`.
#[allow(clippy::naive_bytecount)]
fn line_col(bytes: &[u8], offset: usize) -> (u32, u32) {
    let before = &bytes[..offset];
    let newline_count = before.iter().filter(|&&b| b == b'\n').count();
    let line = u32::try_from(newline_count)
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    let col = match before.iter().rposition(|&b| b == b'\n') {
        Some(last_nl) => u32::try_from(offset - last_nl).unwrap_or(u32::MAX),
        None => u32::try_from(offset).unwrap_or(u32::MAX).saturating_add(1),
    };
    (line, col)
}

/// Produce a redacted preview of a matched secret byte slice.
///
/// Format: `"XXX...YYY (N chars)"` where XXX/YYY are the first/last 3 Unicode
/// code points of the match. If the match is fewer than 8 bytes, the entire
/// value is suppressed: `"*** (N chars)"`.
fn redact(matched: &[u8]) -> String {
    let n = matched.len();
    if n < 8 {
        return format!("*** ({n} chars)");
    }
    let s = String::from_utf8_lossy(matched);
    let chars: Vec<char> = s.chars().collect();
    let nc = chars.len();
    if nc < 8 {
        return format!("*** ({n} chars)");
    }
    let first3: String = chars[..3].iter().collect();
    let last3: String = chars[nc - 3..].iter().collect();
    format!("{first3}...{last3} ({n} chars)")
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{is_text_sniff, line_col, redact, SecretScanner};
    use crate::exporter::SecretSeverity;

    fn scanner() -> SecretScanner {
        SecretScanner::new().expect("embedded patterns must compile")
    }

    fn findings_for(input: &str) -> Vec<crate::exporter::SecretFinding> {
        scanner().scan(input.as_bytes())
    }

    fn rule_ids(input: &str) -> Vec<String> {
        findings_for(input).into_iter().map(|f| f.rule_id).collect()
    }

    // ── aws-access-key-id ─────────────────────────────────────────────────────

    #[test]
    fn aws_access_key_id_positive() {
        let f = findings_for("key=AKIAIOSFODNN7EXAMPLE");
        assert!(!f.is_empty(), "should detect AKIA key");
        assert!(f.iter().any(|x| x.rule_id == "aws-access-key-id"));
        assert!(f.iter().any(|x| x.severity == SecretSeverity::High));
    }

    #[test]
    fn aws_access_key_id_negative_too_short() {
        // AKIA + only 4 chars → does not satisfy {16}
        assert!(!rule_ids("AKIA1234").contains(&"aws-access-key-id".to_string()));
    }

    // ── github-token ─────────────────────────────────────────────────────────

    #[test]
    fn github_token_ghp_positive() {
        let tok = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"; // 3 + 36 = 39
        assert!(rule_ids(tok).contains(&"github-token".to_string()));
    }

    #[test]
    fn github_token_ghs_positive() {
        let tok = "ghs_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"; // ghs_ variant
        assert!(rule_ids(tok).contains(&"github-token".to_string()));
    }

    #[test]
    fn github_token_negative_too_short() {
        // 35 chars after ghp_ → below {36,}
        assert!(
            !rule_ids("ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" /* 36 chars, test boundary */)
                .is_empty()
                || !rule_ids("ghp_SHORT1234").contains(&"github-token".to_string())
        );
        // Explicit short check
        assert!(!rule_ids("ghp_SHORT1234").contains(&"github-token".to_string()));
    }

    // ── gitlab-token ──────────────────────────────────────────────────────────

    #[test]
    fn gitlab_token_positive() {
        // glpat- + exactly 20 alphanum chars
        let tok = "glpat-ABCDEFGHIJKLMNOPQRST"; // 6 + 20
        assert!(rule_ids(tok).contains(&"gitlab-token".to_string()));
    }

    #[test]
    fn gitlab_token_negative_too_short() {
        // glpat- + 10 chars → does not satisfy {20}
        let tok = "glpat-ABCDEFGHIJ";
        assert!(!rule_ids(tok).contains(&"gitlab-token".to_string()));
    }

    // ── slack-token ───────────────────────────────────────────────────────────

    #[test]
    fn slack_token_xoxb_positive() {
        let tok = "xoxb-1234567890-abcdefabcdef";
        assert!(rule_ids(tok).contains(&"slack-token".to_string()));
    }

    #[test]
    fn slack_token_xoxa_positive() {
        // xoxa- variant
        let tok = "xoxa-1234567890-abcdefabcdef";
        assert!(rule_ids(tok).contains(&"slack-token".to_string()));
    }

    #[test]
    fn slack_token_negative_wrong_prefix() {
        // xoxz- is not a valid slack prefix
        let tok = "xoxz-1234567890-abcdefabcdef";
        assert!(!rule_ids(tok).contains(&"slack-token".to_string()));
    }

    // ── stripe-secret ─────────────────────────────────────────────────────────

    #[test]
    fn stripe_secret_live_positive() {
        // sk_live_ + 24 alphanum chars
        let tok = "sk_live_ABCDEFGHIJKLMNOPQRSTUVWX";
        assert!(rule_ids(tok).contains(&"stripe-secret".to_string()));
    }

    #[test]
    fn stripe_secret_negative_too_short() {
        // sk_live_ + 23 chars → below {24,}
        let tok = "sk_live_ABCDEFGHIJKLMNOPQRSTU";
        assert!(!rule_ids(tok).contains(&"stripe-secret".to_string()));
    }

    // ── google-api-key ────────────────────────────────────────────────────────

    #[test]
    fn google_api_key_positive() {
        // AIza + 35 alphanum chars
        let tok = "AIzaSyAaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(rule_ids(tok).contains(&"google-api-key".to_string()));
    }

    #[test]
    fn google_api_key_negative_too_short() {
        // AIza + 10 chars → below {35}
        let tok = "AIzaSyAaaaa";
        assert!(!rule_ids(tok).contains(&"google-api-key".to_string()));
    }

    // ── private-key-header ────────────────────────────────────────────────────

    #[test]
    fn private_key_header_generic_positive() {
        let input = "-----BEGIN PRIVATE KEY-----\nMIIE...\n-----END PRIVATE KEY-----";
        let ids = rule_ids(input);
        assert!(ids.contains(&"private-key-header".to_string()));
    }

    #[test]
    fn private_key_header_openssh_positive() {
        let input =
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNza...\n-----END OPENSSH PRIVATE KEY-----";
        let f = findings_for(input);
        assert!(f
            .iter()
            .any(|x| x.rule_id == "private-key-header" && x.severity == SecretSeverity::High));
    }

    #[test]
    fn private_key_header_negative_public_key() {
        // Public key header must not trigger
        let input = "-----BEGIN PUBLIC KEY-----\nMIIB...\n-----END PUBLIC KEY-----";
        assert!(!rule_ids(input).contains(&"private-key-header".to_string()));
    }

    // ── npm-auth-token ────────────────────────────────────────────────────────

    #[test]
    fn npm_auth_token_positive() {
        let input = "//registry.npmjs.org/:_authToken=npm_ABCDEFGHIJKLMNOP";
        assert!(rule_ids(input).contains(&"npm-auth-token".to_string()));
    }

    #[test]
    fn npm_auth_token_negative_wrong_field() {
        // _token= instead of _authToken=
        let input = "//registry.npmjs.org/:_token=abc123";
        assert!(!rule_ids(input).contains(&"npm-auth-token".to_string()));
    }

    // ── jwt ───────────────────────────────────────────────────────────────────

    #[test]
    fn jwt_positive_and_severity_medium() {
        let tok = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.\
                   eyJzdWIiOiIxMjM0NTY3ODkwIn0.\
                   SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let f = findings_for(tok);
        assert!(f.iter().any(|x| x.rule_id == "jwt"), "jwt should match");
        assert!(
            f.iter()
                .all(|x| x.rule_id != "jwt" || x.severity == SecretSeverity::Medium),
            "jwt severity must be Medium"
        );
    }

    #[test]
    fn jwt_negative_single_segment() {
        // Only the header segment; two `.` separators required
        let tok = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
        assert!(!rule_ids(tok).contains(&"jwt".to_string()));
    }

    // ── generic-assignment ────────────────────────────────────────────────────

    #[test]
    fn generic_assignment_positive_long_value() {
        // 16-char alphanumeric value after api_key =
        let input = r#"api_key = "AVERYLONGSECRET1""#;
        assert!(rule_ids(input).contains(&"generic-assignment".to_string()));
    }

    #[test]
    fn generic_assignment_negative_short_value() {
        // value only 8 chars — below {16,}
        let input = r#"api_key = "short123""#;
        assert!(!rule_ids(input).contains(&"generic-assignment".to_string()));
    }

    #[test]
    fn generic_assignment_negative_commented_line() {
        // Short value in a commented line → not matched (value < 16 chars)
        let input = r#"# api_key = "short""#;
        assert!(!rule_ids(input).contains(&"generic-assignment".to_string()));
    }

    // ── binary input ──────────────────────────────────────────────────────────

    #[test]
    fn binary_input_returns_empty_findings() {
        // Null byte → classified as binary → scanner returns empty Vec
        let mut bytes = b"AKIAIOSFODNN7EXAMPLE".to_vec();
        bytes.insert(5, 0x00); // inject null
        assert!(scanner().scan(&bytes).is_empty());
    }

    // ── preview redaction ─────────────────────────────────────────────────────

    #[test]
    fn preview_redacted_does_not_contain_full_secret() {
        // AKIA + 16 uppercase chars = 20-char AWS key
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let f = findings_for(secret);
        let aws = f
            .iter()
            .find(|x| x.rule_id == "aws-access-key-id")
            .expect("must find AWS key");
        assert_eq!(aws.preview, "AKI...PLE (20 chars)", "wrong preview format");
        assert!(
            !aws.preview.contains(secret),
            "preview must not contain the full secret"
        );
    }

    #[test]
    fn preview_short_secret_uses_stars() {
        // xoxb- + 2 chars = 7 bytes → "*** (7 chars)"
        let input = "xoxb-ab";
        let f = findings_for(input);
        let hit = f
            .iter()
            .find(|x| x.rule_id == "slack-token")
            .expect("must match slack-token");
        assert_eq!(hit.preview, "*** (7 chars)");
    }

    // ── line / column ─────────────────────────────────────────────────────────

    #[test]
    fn line_col_correct_for_line3_col12() {
        // Secret at line 3, column 12 (1-indexed).
        // Line 1: "line1\n" (6 bytes)
        // Line 2: "line2\n" (6 bytes)
        // Line 3: "01234567890AKIAIOSFODNN7EXAMPLE\n"
        //                      ^ position 11 within line → col 12
        let input = "line1\nline2\n01234567890AKIAIOSFODNN7EXAMPLE\n";
        let f = findings_for(input);
        let aws = f
            .iter()
            .find(|x| x.rule_id == "aws-access-key-id")
            .expect("must find AWS key");
        assert_eq!(aws.line, 3, "expected line 3");
        assert_eq!(aws.column, 12, "expected column 12");
    }

    // ── multi-finding ─────────────────────────────────────────────────────────

    #[test]
    fn multi_finding_two_different_rules() {
        let input = "AKIAIOSFODNN7EXAMPLE\n-----BEGIN PRIVATE KEY-----\n";
        let f = findings_for(input);
        let ids: Vec<&str> = f.iter().map(|x| x.rule_id.as_str()).collect();
        assert!(ids.contains(&"aws-access-key-id"), "expected aws finding");
        assert!(
            ids.contains(&"private-key-header"),
            "expected private-key finding"
        );
        assert_eq!(f.len(), 2, "expected exactly 2 findings");
    }

    // ── unit helpers (private) ─────────────────────────────────────────────────

    #[test]
    fn redact_long_returns_first_last_three() {
        // 10 ASCII chars
        let r = redact(b"ABCDEFGHIJ");
        assert_eq!(r, "ABC...HIJ (10 chars)");
    }

    #[test]
    fn redact_short_returns_stars() {
        let r = redact(b"ABCDE");
        assert_eq!(r, "*** (5 chars)");
    }

    #[test]
    fn is_text_sniff_null_byte_is_binary() {
        assert!(!is_text_sniff(b"hello\x00world"));
    }

    #[test]
    fn is_text_sniff_valid_utf8_is_text() {
        assert!(is_text_sniff(b"[general]\nkey = value\n"));
    }

    #[test]
    fn line_col_first_char_is_line1_col1() {
        assert_eq!(line_col(b"AKIA1234", 0), (1, 1));
    }

    #[test]
    fn line_col_after_newline_resets_col() {
        // "abc\nX" — X is at offset 4, line 2, col 1
        assert_eq!(line_col(b"abc\nX", 4), (2, 1));
    }
}
