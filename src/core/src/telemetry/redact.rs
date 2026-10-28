//! Redaction carried over unchanged from the original trace writer: secret-looking JSON keys,
//! `name: value` lines, quoted pairs inside text, and the audit-log patterns. Values removed here
//! are remembered so the processor can scrub repeats from the whole run.
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

/// Length of a JSON string body up to its closing quote, honouring escapes.
fn closing_quote(text: &str) -> Option<usize> {
    let mut escaped = false;
    for (index, c) in text.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return Some(index),
            _ => {}
        }
    }
    None
}

/// Redact `"secretName": value` pairs inside text, such as JSON quoted in a model answer.
fn redact_quoted_pairs(text: &str, found: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        let Some(end) = closing_quote(after) else {
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
        let length = match value.strip_prefix('"') {
            Some(body) => closing_quote(body).map_or(value.len(), |end| end + 2),
            None => value
                .find([',', '}', ']', ' ', '\t'])
                .unwrap_or(value.len()),
        };
        if length == 0 {
            continue;
        }
        if let Some(body) = value[..length].strip_prefix('"') {
            let body = body.strip_suffix('"').unwrap_or(body);
            // Keep the decoded form too, as the trace stores text JSON-escaped.
            found
                .push(serde_json::from_str(&format!("\"{body}\"")).unwrap_or_else(|_| body.into()));
        }
        out.push_str(&rest[..rest.len() - value.len()]);
        out.push_str("\"[redacted]\"");
        rest = &value[length..];
    }
    out.push_str(rest);
    out
}

/// Audit redaction plus `name: value` lines, as in YAML and `.env`-style files.
fn redact_lines(text: &str, found: &mut Vec<String>) -> String {
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
                found.push(value.trim().trim_matches('"').to_owned());
                format!("{name}: [redacted]")
            }
            _ => {
                let line = redact_quoted_pairs(line, found);
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
        let answer = clip(
            r#"Contents: ```{"service":"demo","apiKey":"s\"4","port":80,"token": 5}``` done"#,
            &mut vec![],
        );
        assert_eq!(
            answer,
            r#"Contents: ```{"service":"demo","apiKey":"[redacted]","port":80,"token": "[redacted]"}``` done"#
        );
    }
}
