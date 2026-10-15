//! Local agent-run traces in OpenTelemetry JSON (OTLP/JSON `ExportTraceServiceRequest`), using the
//! GenAI semantic conventions. Each run writes one file under `traces/` in the data folder; nothing
//! is sent anywhere. Timings, token counts, and outcomes are always recorded; prompts and outputs
//! only when the owner turns on content capture.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

pub const TRACE_DIRECTORY: &str = "traces";
/// The newest traces kept; older files are removed after each run.
const KEEP: usize = 200;
const MAX_SPANS: usize = 1000;
const MAX_TEXT: usize = 4096;
const MAX_FILE: usize = 4 * 1024 * 1024;

/// Owner settings for one run. Callers read their own saved settings; the default records nothing.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TraceOptions {
    pub enabled: bool,
    /// Also record model replies and tool arguments and results, redacted and truncated.
    pub content: bool,
}

type Attributes = Vec<(String, Value)>;

struct Span {
    id: String,
    parent: Option<String>,
    name: String,
    kind: u8,
    start: u128,
    end: Option<u128>,
    attributes: Attributes,
    events: Vec<(u128, String, Attributes)>,
    error: Option<String>,
}

/// One run's spans. Written once, when the run ends or is dropped.
pub struct Trace {
    path: PathBuf,
    directory: PathBuf,
    trace_id: String,
    pub content: bool,
    spans: Mutex<Vec<Span>>,
    secrets: Mutex<Vec<String>>,
    dropped: Mutex<usize>,
}

/// A started span; pass it back to `end`.
#[derive(Clone)]
pub struct SpanId(String);

fn now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

fn hex(bytes: usize) -> String {
    (0..bytes)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect()
}

const SECRET_HINTS: [&str; 6] = [
    "key",
    "token",
    "secret",
    "password",
    "authorization",
    "cookie",
];
const CONTENT_KEYS: [&str; 3] = [
    "gen_ai.tool.call.arguments",
    "gen_ai.tool.call.result",
    "gen_ai.output.messages",
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
fn clip(text: &str, found: &mut Vec<String>) -> String {
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

/// An OTLP `AnyValue`.
fn encode(value: &Value) -> Value {
    match value {
        Value::Number(n) if n.is_i64() || n.is_u64() => json!({"intValue": n.to_string()}),
        Value::Number(n) => json!({"doubleValue": n}),
        Value::Bool(b) => json!({"boolValue": b}),
        Value::String(s) => json!({"stringValue": s}),
        Value::Array(items) => {
            json!({"arrayValue": {"values": items.iter().map(encode).collect::<Vec<_>>()}})
        }
        other => json!({"stringValue": other.to_string()}),
    }
}

/// The JSON value of an OTLP `AnyValue`, for display.
fn decode(value: &Value) -> Option<Value> {
    value
        .get("stringValue")
        .cloned()
        .or_else(|| {
            value["intValue"]
                .as_str()
                .and_then(|n| n.parse::<i64>().ok())
                .map(Value::from)
        })
        .or_else(|| value.get("boolValue").cloned())
        .or_else(|| value.get("doubleValue").cloned())
        .or_else(|| {
            value["arrayValue"]["values"]
                .as_array()
                .map(|items| Value::Array(items.iter().filter_map(decode).collect()))
        })
}

impl Trace {
    /// Start a run's trace in `directory/traces`, or `None` when tracing is off.
    pub fn start(directory: &Path, request_id: &str, options: TraceOptions) -> Option<Self> {
        if !options.enabled {
            return None;
        }
        let safe: String = request_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .take(64)
            .collect();
        let started = now();
        let directory = directory.join(TRACE_DIRECTORY);
        let trace = Self {
            path: directory.join(format!("{:013}-{safe}.json", started / 1_000_000)),
            directory,
            trace_id: hex(16),
            content: options.content,
            spans: Mutex::new(vec![]),
            secrets: Mutex::new(vec![]),
            dropped: Mutex::new(0),
        };
        trace.begin_at(
            None,
            "invoke_agent blackwall",
            1,
            started,
            vec![
                ("gen_ai.operation.name".into(), json!("invoke_agent")),
                ("gen_ai.agent.name".into(), json!("blackwall")),
                ("blackwall.request_id".into(), json!(request_id)),
            ],
        );
        Some(trace)
    }

    /// The run's root span.
    pub fn root(&self) -> SpanId {
        SpanId(
            self.spans
                .lock()
                .ok()
                .and_then(|spans| spans.first().map(|span| span.id.clone()))
                .unwrap_or_default(),
        )
    }

    fn begin_at(
        &self,
        parent: Option<&SpanId>,
        name: &str,
        kind: u8,
        start: u128,
        attributes: Vec<(String, Value)>,
    ) -> SpanId {
        let id = hex(8);
        if let Ok(mut spans) = self.spans.lock() {
            if spans.len() >= MAX_SPANS {
                if let Ok(mut dropped) = self.dropped.lock() {
                    *dropped += 1;
                }
                return SpanId(String::new());
            }
            spans.push(Span {
                id: id.clone(),
                parent: parent.map(|parent| parent.0.clone()),
                name: name.into(),
                kind,
                start,
                end: None,
                attributes,
                events: vec![],
                error: None,
            });
        }
        SpanId(id)
    }

    /// Start a child span. `kind` follows OTLP: 1 internal, 3 client.
    pub fn begin(
        &self,
        parent: &SpanId,
        name: &str,
        kind: u8,
        attributes: Vec<(String, Value)>,
    ) -> SpanId {
        self.begin_at(Some(parent), name, kind, now(), attributes)
    }

    fn with_span(&self, id: &SpanId, change: impl FnOnce(&mut Span)) {
        if let Ok(mut spans) = self.spans.lock() {
            if let Some(span) = spans.iter_mut().find(|span| span.id == id.0) {
                change(span);
            }
        }
    }

    /// Add attributes to a span that is still open or already ended.
    pub fn set(&self, id: &SpanId, attributes: Vec<(String, Value)>) {
        self.with_span(id, |span| span.attributes.extend(attributes));
    }

    pub fn event(&self, id: &SpanId, name: &str, attributes: Vec<(String, Value)>) {
        self.with_span(id, |span| {
            span.events.push((now(), name.into(), attributes));
        });
    }

    /// End a span; an error message marks it failed.
    /// Redacted, bounded text for a content attribute. Secret values it removes are also removed
    /// from the rest of the trace when it is written, such as when the model repeats one.
    pub fn clip(&self, text: &str) -> String {
        let mut found = vec![];
        let text = clip(text, &mut found);
        if let Ok(mut secrets) = self.secrets.lock() {
            secrets.extend(found.into_iter().filter(|value| value.chars().count() >= 6));
        }
        text
    }

    pub fn end(&self, id: &SpanId, error: Option<&str>) {
        let error = error.map(|error| self.clip(error));
        self.with_span(id, |span| {
            span.end.get_or_insert_with(now);
            if error.is_some() {
                if !span.attributes.iter().any(|(key, _)| key == "error.type") {
                    span.attributes.push(("error.type".into(), json!("_OTHER")));
                }
                span.error = error;
            }
        });
    }

    /// The OTLP/JSON request. `without_content` leaves out content attributes, for oversized runs.
    fn document(&self, without_content: bool) -> Value {
        let mut secrets = self.secrets.lock().map(|s| s.clone()).unwrap_or_default();
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        secrets.dedup();
        // Content is stored as text that may itself hold JSON, so match the escaped form as well.
        let forms: Vec<String> = secrets
            .iter()
            .flat_map(|secret| {
                let escaped = serde_json::to_string(secret).unwrap_or_default();
                [secret.clone(), escaped[1..escaped.len() - 1].to_owned()]
            })
            .collect();
        let scrub = |text: &str| {
            forms.iter().fold(text.to_owned(), |text, secret| {
                text.replace(secret.as_str(), "[redacted]")
            })
        };
        let spans = self.spans.lock().map(|spans| {
            spans
                .iter()
                .map(|span| {
                    let attributes = |pairs: &[(String, Value)]| {
                        pairs
                            .iter()
                            .filter(|(key, _)| {
                                !without_content || !CONTENT_KEYS.contains(&key.as_str())
                            })
                            .map(|(key, value)| {
                                let value = match value {
                                    Value::String(text) if CONTENT_KEYS.contains(&key.as_str()) => {
                                        json!(scrub(text))
                                    }
                                    other => other.clone(),
                                };
                                json!({"key": key, "value": encode(&value)})
                            })
                            .collect::<Vec<_>>()
                    };
                    // A span still open when the run ended was cut off by cancellation.
                    let error = match (&span.error, span.end) {
                        (Some(message), _) => Some(message.as_str()),
                        (None, None) => Some("Cancelled."),
                        (None, Some(_)) => None,
                    };
                    let mut entry = json!({
                        "traceId": self.trace_id,
                        "spanId": span.id,
                        "name": span.name,
                        "kind": span.kind,
                        "startTimeUnixNano": span.start.to_string(),
                        "endTimeUnixNano": span.end.unwrap_or_else(now).to_string(),
                        "attributes": attributes(&span.attributes),
                        "events": span.events.iter().map(|(time, name, pairs)| json!({
                            "timeUnixNano": time.to_string(), "name": name, "attributes": attributes(pairs)
                        })).collect::<Vec<_>>(),
                        "status": match error {
                            Some(message) => json!({"code": 2, "message": scrub(message)}),
                            None => json!({"code": 1}),
                        },
                    });
                    if let Some(parent) = &span.parent {
                        entry["parentSpanId"] = json!(parent);
                    }
                    entry
                })
                .collect::<Vec<_>>()
        });
        let dropped = self.dropped.lock().map_or(0, |dropped| *dropped);
        let mut resource = vec![json!({"key":"service.name","value":{"stringValue":"blackwall"}})];
        if dropped > 0 {
            resource.push(
                json!({"key":"blackwall.trace.dropped_spans","value":encode(&json!(dropped))}),
            );
        }
        if without_content {
            resource
                .push(json!({"key":"blackwall.trace.content_removed","value":{"boolValue":true}}));
        }
        json!({"resourceSpans":[{
            "resource":{"attributes":resource},
            "scopeSpans":[{
                "scope":{"name":"blackwall","version":env!("CARGO_PKG_VERSION")},
                "spans": spans.unwrap_or_default(),
            }]
        }]})
    }

    /// Write the file and trim old traces. Trace failures never affect the run.
    fn write(&self) {
        let render = |without_content| {
            serde_json::to_vec(&self.document(without_content)).unwrap_or_default()
        };
        let mut body = render(false);
        if body.len() > MAX_FILE {
            body = render(true);
        }
        if body.is_empty() || body.len() > MAX_FILE {
            return;
        }
        body.push(b'\n');
        if std::fs::create_dir_all(&self.directory).is_err()
            || std::fs::symlink_metadata(&self.directory).is_ok_and(|m| m.file_type().is_symlink())
        {
            return;
        }
        let _ = crate::policy::replace_owner_file(&self.directory, &self.path, &body);
        let mut files = list_files(&self.directory);
        if files.len() > KEEP {
            files.sort();
            for old in &files[..files.len() - KEEP] {
                let _ = std::fs::remove_file(self.directory.join(old));
            }
        }
    }
}

/// Ends the root span and writes the trace, including when the run is cancelled.
pub(crate) struct TraceGuard {
    pub trace: Trace,
    pub outcome: Option<Result<(), String>>,
}
impl Drop for TraceGuard {
    fn drop(&mut self) {
        let root = self.trace.root();
        match &self.outcome {
            Some(Ok(())) => self.trace.end(&root, None),
            Some(Err(error)) => self.trace.end(&root, Some(error)),
            None => self.trace.end(&root, Some("The run was stopped.")),
        }
        self.trace.write();
    }
}

fn list_files(directory: &Path) -> Vec<String> {
    std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".json"))
                .collect()
        })
        .unwrap_or_default()
}

/// One span read back from a trace file, for display.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpanView {
    pub id: String,
    pub parent: Option<String>,
    pub name: String,
    pub start_ms: u64,
    pub duration_ms: u64,
    pub error: Option<String>,
    pub attributes: serde_json::Map<String, Value>,
    pub events: Vec<String>,
}

/// A trace file summarized for a list.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceSummary {
    pub id: String,
    pub started_ms: u64,
    pub duration_ms: u64,
    pub error: Option<String>,
    pub model_calls: usize,
    pub tool_calls: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub model: Option<String>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Read one trace by ID (its file name without `.json`). Blocking.
pub fn read(directory: &Path, id: &str) -> Result<Vec<SpanView>, String> {
    if !valid_id(id) {
        return Err("No trace has that ID.".into());
    }
    let path = directory.join(TRACE_DIRECTORY).join(format!("{id}.json"));
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => {
            if metadata.len() > (MAX_FILE + 1) as u64 {
                return Err("This trace is too large to read.".into());
            }
        }
        _ => return Err("No trace has that ID.".into()),
    }
    let mut body = Vec::new();
    std::fs::File::open(&path)
        .and_then(|file| file.take((MAX_FILE + 2) as u64).read_to_end(&mut body))
        .map_err(|_| "No trace has that ID.")?;
    if body.len() > MAX_FILE + 1 {
        return Err("This trace is too large to read.".into());
    }
    let document: Value =
        serde_json::from_slice(&body).map_err(|_| "This trace could not be read.")?;
    let nanos = |value: &Value| {
        value
            .as_str()
            .and_then(|s| s.parse::<u128>().ok())
            .unwrap_or(0)
    };
    let pairs = |value: &Value| {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|pair| Some((pair["key"].as_str()?.to_owned(), decode(&pair["value"])?)))
            .collect::<serde_json::Map<_, _>>()
    };
    let spans = document["resourceSpans"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|resource| resource["scopeSpans"].as_array().into_iter().flatten())
        .flat_map(|scope| scope["spans"].as_array().into_iter().flatten())
        .map(|span| {
            let start = nanos(&span["startTimeUnixNano"]);
            let end = nanos(&span["endTimeUnixNano"]).max(start);
            SpanView {
                id: span["spanId"].as_str().unwrap_or_default().into(),
                parent: span["parentSpanId"].as_str().map(str::to_owned),
                name: span["name"].as_str().unwrap_or_default().into(),
                start_ms: (start / 1_000_000) as u64,
                duration_ms: ((end - start) / 1_000_000) as u64,
                error: (span["status"]["code"] == 2).then(|| {
                    span["status"]["message"]
                        .as_str()
                        .unwrap_or("Failed")
                        .to_owned()
                }),
                attributes: pairs(&span["attributes"]),
                events: span["events"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|event| event["name"].as_str().map(str::to_owned))
                    .collect(),
            }
        })
        .collect();
    Ok(spans)
}

/// The newest traces first, at most `limit`. Unreadable files are skipped. Blocking.
pub fn list(directory: &Path, limit: usize) -> Vec<TraceSummary> {
    let mut files = list_files(&directory.join(TRACE_DIRECTORY));
    files.sort();
    files.reverse();
    files
        .into_iter()
        .filter_map(|name| name.strip_suffix(".json").map(str::to_owned))
        .take(limit)
        .filter_map(|id| {
            let spans = read(directory, &id).ok()?;
            let root = spans.iter().find(|span| span.parent.is_none())?;
            let operation = |span: &&SpanView, name: &str| {
                span.attributes.get("gen_ai.operation.name") == Some(&json!(name))
            };
            let models: Vec<_> = spans
                .iter()
                .filter(|span| operation(span, "chat"))
                .collect();
            let tokens = |key: &str| {
                models
                    .iter()
                    .filter_map(|span| span.attributes.get(key).and_then(Value::as_u64))
                    .sum()
            };
            Some(TraceSummary {
                started_ms: root.start_ms,
                duration_ms: root.duration_ms,
                error: root.error.clone(),
                model_calls: models.len(),
                tool_calls: spans
                    .iter()
                    .filter(|span| operation(span, "execute_tool"))
                    .count(),
                input_tokens: tokens("gen_ai.usage.input_tokens"),
                output_tokens: tokens("gen_ai.usage.output_tokens"),
                model: models.iter().find_map(|span| {
                    span.attributes
                        .get("gen_ai.request.model")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                }),
                id,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder() -> std::path::PathBuf {
        let directory =
            std::env::temp_dir().join(format!("blackwall-trace-unit-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn disabled_traces_write_nothing() {
        let directory = folder();
        assert!(Trace::start(&directory, "r", TraceOptions::default()).is_none());
        assert!(!directory.join(TRACE_DIRECTORY).exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stopped_run_is_written_as_failed_and_reads_back() {
        let directory = folder();
        let options = TraceOptions {
            enabled: true,
            content: false,
        };
        let trace = Trace::start(&directory, "../run 1", options).unwrap();
        let root = trace.root();
        let child = trace.begin(
            &root,
            "chat m",
            3,
            vec![("gen_ai.operation.name".into(), json!("chat"))],
        );
        trace.set(&child, vec![("gen_ai.usage.input_tokens".into(), json!(7))]);
        trace.end(&child, Some("token=abc123 failed"));
        drop(TraceGuard {
            trace,
            outcome: None,
        });
        let summaries = list(&directory, 10);
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].id.ends_with("-run1"));
        assert_eq!(summaries[0].error.as_deref(), Some("The run was stopped."));
        assert_eq!(summaries[0].input_tokens, 7);
        let spans = read(&directory, &summaries[0].id).unwrap();
        assert_eq!(spans.len(), 2);
        let error = spans[1].error.as_deref().unwrap();
        assert!(!error.contains("abc123"), "{error}");
        assert!(read(&directory, "../x").is_err());
        assert!(read(&directory, "missing").is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn keeps_only_the_newest_traces() {
        let directory = folder();
        let traces = directory.join(TRACE_DIRECTORY);
        std::fs::create_dir_all(&traces).unwrap();
        for index in 0..KEEP + 5 {
            std::fs::write(traces.join(format!("{index:013}-old.json")), "{}").unwrap();
        }
        let options = TraceOptions {
            enabled: true,
            content: false,
        };
        drop(TraceGuard {
            trace: Trace::start(&directory, "new", options).unwrap(),
            outcome: Some(Ok(())),
        });
        let files = list_files(&traces);
        assert_eq!(files.len(), KEEP);
        assert!(!files.contains(&format!("{:013}-old.json", 5)));
        assert!(files.iter().any(|name| name.ends_with("-new.json")));
        std::fs::remove_dir_all(directory).unwrap();
    }

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

    #[test]
    fn open_spans_are_cancelled_and_oversized_traces_drop_content() {
        let directory = folder();
        let options = TraceOptions {
            enabled: true,
            content: true,
        };
        let trace = Trace::start(&directory, "big", options).unwrap();
        let root = trace.root();
        // About 8 KiB on each of 501 spans passes the 4 MiB file limit.
        let text = "x".repeat(MAX_FILE / (MAX_SPANS / 2));
        for _ in 0..MAX_SPANS / 2 + 1 {
            let span = trace.begin(
                &root,
                "execute_tool read_file",
                1,
                vec![("gen_ai.tool.call.result".into(), json!(text))],
            );
            trace.end(&span, None);
        }
        let open = trace.begin(
            &root,
            "chat m",
            3,
            vec![("gen_ai.response.finish_reasons".into(), json!(["stop"]))],
        );
        trace.set(&open, vec![]);
        drop(TraceGuard {
            trace,
            outcome: Some(Ok(())),
        });
        let id = list(&directory, 1).remove(0).id;
        let spans = read(&directory, &id).unwrap();
        assert!(spans
            .iter()
            .all(|span| !span.attributes.contains_key("gen_ai.tool.call.result")));
        let open = spans.iter().find(|span| span.name == "chat m").unwrap();
        assert_eq!(open.error.as_deref(), Some("Cancelled."));
        assert_eq!(
            open.attributes["gen_ai.response.finish_reasons"],
            json!(["stop"])
        );
        let body =
            std::fs::read_to_string(directory.join(TRACE_DIRECTORY).join(format!("{id}.json")))
                .unwrap();
        assert!(body.contains("blackwall.trace.content_removed"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn secrets_found_anywhere_are_scrubbed_from_the_whole_trace() {
        let directory = folder();
        let options = TraceOptions {
            enabled: true,
            content: true,
        };
        let trace = Trace::start(&directory, "scrub", options).unwrap();
        let root = trace.root();
        // The model repeats the value before and after the tool result that reveals it.
        for text in [
            "The key is `longsecret99`.",
            r#"{"apiKey":"longsecret99"}"#,
            "**apiKey**: longsecret99",
            r#"{"apiKey":"quote\"secret","echo":"quote\"secret","flag":"true"}"#,
            r#"Shown as ```{"note":"quote\"secret"}```"#,
        ] {
            let span = trace.begin(
                &root,
                "chat m",
                3,
                vec![("gen_ai.output.messages".into(), json!(trace.clip(text)))],
            );
            trace.end(&span, Some("failed near longsecret99"));
        }
        drop(TraceGuard {
            trace,
            outcome: Some(Ok(())),
        });
        let id = list(&directory, 1).remove(0).id;
        let body =
            std::fs::read_to_string(directory.join(TRACE_DIRECTORY).join(format!("{id}.json")))
                .unwrap();
        assert!(!body.contains("longsecret99"), "{body}");
        assert!(!body.contains("quote"), "{body}");
        assert!(body.contains(r#"flag\":\"true"#), "{body}");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
