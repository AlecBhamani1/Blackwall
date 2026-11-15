//! Trace redaction: secret-looking JSON keys, `name: value` lines, quoted pairs inside text, PEM
//! private keys, API tokens with a known prefix and shape, and the audit-log patterns. Values
//! removed here are remembered so the processor can scrub repeats from the whole run.
use serde_json::{json, Value};

pub(super) const MAX_TEXT: usize = 4096;

const SECRET_HINTS: [&str; 6] = [
    "key",
    "token",
    "secret",
    "password",
    "authorization",
    "cookie",
];

fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SECRET_HINTS.iter().any(|hint| name.contains(hint))
}

fn redact_json(value: &mut Value, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (name, value) in map.iter_mut() {
                if secret_name(name) {
                    strings(value, found);
                    *value = json!("[redacted]");
                } else {
                    redact_json(value, found);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| redact_json(item, found)),
        Value::String(text) => *text = redact_lines(text, found),
        _ => {}
    }
}

/// Every string inside a redacted value, for scrubbing repeats. Other scalars are too common.
fn strings(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::String(text) => found.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|item| strings(item, found)),
        Value::Object(map) => map.values().for_each(|item| strings(item, found)),
        _ => {}
    }
}

/// Length of a quoted string body up to its closing `quote`, honouring escapes.
fn closing_quote(text: &str, quote: char) -> Option<usize> {
    let mut escaped = false;
    for (index, c) in text.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            _ if c == quote => return Some(index),
            _ => {}
        }
    }
    None
}

/// Redact `"secretName": value` and `'secretName': value` pairs inside text, such as JSON or a
/// Python dict quoted in a model answer.
fn redact_quoted_pairs(text: &str, quote: char, found: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(quote) {
        let after = &rest[start + 1..];
        let Some(end) = closing_quote(after, quote) else {
            break;
        };
        let key = &after[..end];
        out.push_str(&rest[..start + end + 2]);
        rest = &after[end + 1..];
        let Some(value) = rest.trim_start().strip_prefix(':').map(str::trim_start) else {
            continue;
        };
        if !secret_name(key) || value.starts_with(['{', '[']) {
            continue;
        }
        let length = match value.strip_prefix(quote) {
            Some(body) => closing_quote(body, quote).map_or(value.len(), |end| end + 2),
            None => value
                .find([',', '}', ']', ' ', '\t'])
                .unwrap_or(value.len()),
        };
        if length == 0 {
            continue;
        }
        if let Some(body) = value[..length].strip_prefix(quote) {
            let body = body.strip_suffix(quote).unwrap_or(body);
            // Keep the decoded form too, as the trace stores text JSON-escaped.
            found.push(match quote {
                '"' => serde_json::from_str(&format!("\"{body}\"")).unwrap_or_else(|_| body.into()),
                _ => body.into(),
            });
        }
        out.push_str(&rest[..rest.len() - value.len()]);
        out.push_str(&format!("{quote}[redacted]{quote}"));
        rest = &value[length..];
    }
    out.push_str(rest);
    out
}

/// Prefixes of well-known API tokens. A match also needs the tail after the prefix to have the
/// provider's shape (see [`token_shape`]), so names such as `sk-learn` are kept.
const TOKEN_PREFIXES: [&str; 9] = [
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
];

/// Whether `tail`, the token characters after `prefix`, looks like a real token rather than a name.
fn token_shape(prefix: &str, tail: &str) -> bool {
    let has = |test: fn(&u8) -> bool| tail.bytes().any(|b| test(&b));
    let alphanumeric = tail.bytes().all(|b| b.is_ascii_alphanumeric());
    match prefix {
        // OpenAI and Anthropic keys mix cases; others are long lowercase hex. Hyphenated
        // lowercase names such as `sk-documentation-example-2026` are neither.
        "sk-" => {
            tail.len() >= 32
                && has(u8::is_ascii_digit)
                && has(u8::is_ascii_lowercase)
                && (alphanumeric || has(u8::is_ascii_uppercase))
        }
        // 22 base62 characters, `_`, then 59 more.
        "github_pat_" => tail
            .split_once('_')
            .is_some_and(|(id, secret)| base62(id, 22) && base62(secret, 59)),
        // Slack: numeric team and user IDs, then a long alphanumeric secret.
        "xoxb-" | "xoxp-" => {
            let parts: Vec<&str> = tail.split('-').collect();
            parts.len() >= 3
                && parts[..parts.len() - 1]
                    .iter()
                    .all(|id| id.len() >= 8 && id.bytes().all(|b| b.is_ascii_digit()))
                && parts[parts.len() - 1].len() >= 24
                && parts[parts.len() - 1]
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric())
        }
        // GitHub `gh?_` tokens: exactly 36 base62 characters, which may be letters only.
        _ => base62(tail, 36),
    }
}

fn base62(text: &str, length: usize) -> bool {
    text.len() == length && text.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}

/// Redact bare API tokens with a known prefix, wherever they appear.
fn redact_tokens(text: &str, found: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((start, prefix)) = TOKEN_PREFIXES
        .iter()
        .filter_map(|prefix| rest.find(prefix).map(|at| (at, prefix)))
        .min()
    {
        let after = &rest[start + prefix.len()..];
        let tail = after.find(|c| !token_char(c)).unwrap_or(after.len());
        let token = &rest[start..start + prefix.len() + tail];
        // No word-boundary check: the shape alone decides, so `KEY_ghp_…` is caught too.
        if token_shape(prefix, &after[..tail]) {
            out.push_str(&rest[..start]);
            out.push_str("[redacted]");
            found.push(token.to_owned());
        } else {
            out.push_str(&rest[..start + prefix.len()]);
            rest = &rest[start + prefix.len()..];
            continue;
        }
        rest = &rest[start + token.len()..];
    }
    out.push_str(rest);
    out
}

/// Redact PEM private key blocks, from `-----BEGIN … PRIVATE KEY-----` to the matching end line
/// or, if it is missing, the end of the text.
fn redact_private_keys(text: &str, found: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("-----BEGIN ") {
        let label_start = start + "-----BEGIN ".len();
        let label = rest[label_start..]
            .find("-----")
            .map(|end| &rest[label_start..label_start + end])
            .filter(|label| label.ends_with("PRIVATE KEY") && !label.contains('\n'));
        let Some(label) = label else {
            out.push_str(&rest[..label_start]);
            rest = &rest[label_start..];
            continue;
        };
        let footer = format!("-----END {label}-----");
        let end = rest[start..]
            .find(&footer)
            .map_or(rest.len(), |at| start + at + footer.len());
        out.push_str(&rest[..start]);
        out.push_str("[redacted]");
        found.push(rest[start..end].to_owned());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Audit redaction plus `name: value` lines, as in YAML and `.env`-style files, private keys, and
/// bare API tokens.
fn redact_lines(text: &str, found: &mut Vec<String>) -> String {
    let text = redact_tokens(&redact_private_keys(text, found), found);
    text.lines()
        .map(|line| match line.split_once(':') {
            Some((name, value))
                if !value.trim().is_empty()
                    && secret_name(name)
                    && name
                        .trim()
                        .trim_matches('"')
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')) =>
            {
                // A trailing comma ends a JSON or YAML item; it is not part of the value.
                let value = value.trim().trim_end_matches(',').trim_end();
                found.push(value.trim_matches(['"', '\'']).to_owned());
                format!("{name}: [redacted]")
            }
            _ => {
                let line = redact_quoted_pairs(line, '"', found);
                let line = redact_quoted_pairs(&line, '\'', found);
                let redacted = crate::policy::redact_text(&line);
                // Remember what audit redaction removed, so repeats elsewhere are caught too.
                for (before, after) in line.split_whitespace().zip(redacted.split_whitespace()) {
                    if before != after {
                        let value = match (before.split_once('='), after.contains("=[redacted]")) {
                            (Some((_, value)), true) => value,
                            _ => before,
                        };
                        found.push(value.to_owned());
                    }
                }
                redacted
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Bounded, redacted text for content attributes and error messages. Removed secret values are
/// added to `found`.
pub(super) fn clip(text: &str, found: &mut Vec<String>) -> String {
    let text = match serde_json::from_str::<Value>(text) {
        Ok(mut value) if value.is_object() || value.is_array() => {
            redact_json(&mut value, found);
            value.to_string()
        }
        _ => redact_lines(text, found),
    };
    if text.len() <= MAX_TEXT {
        return text;
    }
    let mut end = MAX_TEXT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &text[..end])
}

/// Removes every remembered secret, raw and JSON-escaped, from text. Longest values go first.
pub(super) struct Scrubber {
    forms: Vec<String>,
}

impl Scrubber {
    pub(super) fn new(mut secrets: Vec<String>) -> Self {
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        secrets.dedup();
        // Content is stored as text that may itself hold JSON, so match the escaped form as well.
        let forms = secrets
            .iter()
            .flat_map(|secret| {
                let escaped = serde_json::to_string(secret).unwrap_or_default();
                let inner = escaped
                    .get(1..escaped.len().saturating_sub(1))
                    .unwrap_or_default()
                    .to_owned();
                [secret.clone(), inner]
            })
            .filter(|form| !form.is_empty())
            .collect();
        Self { forms }
    }

    pub(super) fn scrub(&self, text: &str) -> String {
        self.forms.iter().fold(text.to_owned(), |text, secret| {
            text.replace(secret.as_str(), "[redacted]")
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn clip_redacts_and_truncates() {
        assert_eq!(
            clip("a\npassword=hunter2 b", &mut vec![]),
            "a\npassword=[redacted] b"
        );
        let long = "é".repeat(MAX_TEXT);
        let clipped = clip(&long, &mut vec![]);
        assert!(clipped.ends_with("…[truncated]"));
        assert!(clipped.len() <= MAX_TEXT + "…[truncated]".len());
    }

    #[test]
    fn clip_redacts_json_and_yaml_secrets() {
        let mut found = vec![];
        let json = clip(
            r#"{"apiKey":"sk-1","nested":[{"password":"p2"}],"text":"x token=t3"}"#,
            &mut found,
        );
        found.sort();
        assert_eq!(found, ["p2", "sk-1", "t3"]);
        let mut found = vec![];
        let nested = clip(r#"{"token":{"value":"hunter2"},"secret":true}"#, &mut found);
        assert_eq!(nested, r#"{"secret":"[redacted]","token":"[redacted]"}"#);
        assert_eq!(found, ["hunter2"]);
        assert!(
            !["sk-1", "p2", "t3"]
                .iter()
                .any(|secret| json.contains(secret)),
            "{json}"
        );
        assert!(json.contains("[redacted]"));
        let yaml = clip(
            "name: demo\npassword: hunter2\n\"api_key\": \"k\"",
            &mut vec![],
        );
        assert_eq!(
            yaml,
            "name: demo\npassword: [redacted]\n\"api_key\": [redacted]"
        );
        let mut found = vec![];
        let listing = clip(
            "{\n  \"api_key\": \"sk-abc\",\n  'password': 'hunter2', x: 1\n}",
            &mut found,
        );
        assert_eq!(found, ["sk-abc", "hunter2"]);
        assert!(!listing.contains("hunter2"), "{listing}");
        let answer = clip(
            r#"Contents: ```{"service":"demo","apiKey":"s\"4","port":80,"token": 5}``` done"#,
            &mut vec![],
        );
        assert_eq!(
            answer,
            r#"Contents: ```{"service":"demo","apiKey":"[redacted]","port":80,"token": "[redacted]"}``` done"#
        );
    }

    #[test]
    fn clip_redacts_private_keys_and_bare_tokens() {
        // Token-shaped fixtures are assembled at runtime so no complete fake key or token
        // appears literally in source (keeps secret scanners and push protection quiet).
        let armor = |label: &str| format!("-----{label}-----");
        let mut found = vec![];
        let pem = format!(
            "{}\nMIIEpAIBAAKCAQEA\n{}",
            armor("BEGIN RSA PRIVATE KEY"),
            armor("END RSA PRIVATE KEY")
        );
        let text = clip(&format!("key:\n{pem}\ndone"), &mut found);
        assert_eq!(text, "key:\n[redacted]\ndone");
        assert_eq!(found, [pem]);
        let escaped = clip(
            &format!(r#"{{"note":"{}\nAAAA"}}"#, armor("BEGIN PRIVATE KEY")),
            &mut vec![],
        );
        assert_eq!(escaped, r#"{"note":"[redacted]"}"#);
        // Real shapes: OpenAI project key, a letters-only GitHub token embedded after `_`,
        // and a DeepSeek-style hex key.
        let (sk, gh, git, xox) = ("sk", "gh", "git", "xox");
        let openai = format!("{sk}-proj-Ab3dEfGhIjKlMnOpQrStUvWxYz012345");
        let github = format!("{gh}p_abcdefghijklmnopqrstuvwxyzABCDEFGHIJ");
        let hex = format!("{sk}-0123456789abcdef0123456789abcdef");
        let slack = format!("{xox}b-1234567890-9876543210987-AbCdEfGhIjKlMnOpQrStUvWx");
        let pat = format!(
            "{git}hub_pat_{}_{}",
            "A1".repeat(11),
            "b2C".repeat(19) + "d3"
        );
        let mut found = vec![];
        let text = clip(
            &format!("use {openai}, GH_TOKEN_{github}; {hex} {slack} {pat}"),
            &mut found,
        );
        assert_eq!(
            text,
            "use [redacted], GH_TOKEN_[redacted]; [redacted] [redacted] [redacted]"
        );
        assert_eq!(found, [openai, github, hex, slack, pat]);
        // Names that only resemble tokens are kept and not remembered for the run.
        let mut found = vec![];
        let prose = format!(
            "{sk}-learn, {sk}-documentation-example-2026-and-more, ask-0123456789abcdefghij, \
             {gh}p_short1234567890abcdefghij, {xox}b-documentation-example-2026, \
             {xox}p-your-workspace-token-goes-here, {gh}p_thisisalongdocumentationplaceholdervalue, \
             {git}hub_pat_this_is_a_documentation_placeholder_for_the_new_fine_grained_token_format_here_too, \
             gl{}-documentation-example-2026",
            "pat"
        );
        assert_eq!(clip(&prose, &mut found), prose);
        assert!(found.is_empty(), "{found:?}");
    }
}
