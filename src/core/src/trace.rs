//! Reading local run traces: OTLP/JSON `ExportTraceServiceRequest` files under `traces/` in the
//! data folder, one per run. `core::telemetry` writes them; files from earlier versions, written
//! by the original hand-rolled writer in the same format, read the same way.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{io::Read, path::Path};

pub const TRACE_DIRECTORY: &str = "traces";
pub(crate) const MAX_FILE: usize = 4 * 1024 * 1024;

/// Owner settings for one run. Callers read their own saved settings; the default records nothing.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TraceOptions {
    pub enabled: bool,
    /// Also record model replies and tool arguments and results, redacted and truncated.
    pub content: bool,
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

pub(crate) fn list_files(directory: &Path) -> Vec<String> {
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
                // The SDK writes a root's missing parent as an empty string.
                parent: span["parentSpanId"]
                    .as_str()
                    .filter(|parent| !parent.is_empty())
                    .map(str::to_owned),
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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A trace exactly as the beta.73 writer produced it.
    const LEGACY: &str = r#"{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"blackwall"}}]},"scopeSpans":[{"scope":{"name":"blackwall","version":"0.1.6-beta.73"},"spans":[{"attributes":[{"key":"gen_ai.operation.name","value":{"stringValue":"invoke_agent"}},{"key":"gen_ai.agent.name","value":{"stringValue":"blackwall"}},{"key":"blackwall.request_id","value":{"stringValue":"legacy"}},{"key":"gen_ai.request.model","value":{"stringValue":"llama3.2"}}],"endTimeUnixNano":"1761600002000000000","events":[],"kind":1,"name":"invoke_agent blackwall","spanId":"1111111111111111","startTimeUnixNano":"1761600000000000000","status":{"code":1},"traceId":"0123456789abcdef0123456789abcdef"},{"attributes":[{"key":"gen_ai.operation.name","value":{"stringValue":"chat"}},{"key":"gen_ai.request.model","value":{"stringValue":"llama3.2"}},{"key":"gen_ai.response.finish_reasons","value":{"arrayValue":{"values":[{"stringValue":"stop"}]}}},{"key":"gen_ai.usage.input_tokens","value":{"intValue":"40"}},{"key":"gen_ai.usage.output_tokens","value":{"intValue":"9"}}],"endTimeUnixNano":"1761600001500000000","events":[],"kind":3,"name":"chat llama3.2","parentSpanId":"1111111111111111","spanId":"2222222222222222","startTimeUnixNano":"1761600000100000000","status":{"code":1},"traceId":"0123456789abcdef0123456789abcdef"},{"attributes":[{"key":"gen_ai.operation.name","value":{"stringValue":"execute_tool"}},{"key":"gen_ai.tool.name","value":{"stringValue":"shell"}},{"key":"error.type","value":{"stringValue":"denied"}}],"endTimeUnixNano":"1761600001900000000","events":[{"timeUnixNano":"1761600001600000000","name":"approval_requested","attributes":[{"key":"blackwall.approval.kind","value":{"stringValue":"exec"}}]}],"kind":1,"name":"execute_tool shell","parentSpanId":"1111111111111111","spanId":"3333333333333333","startTimeUnixNano":"1761600001550000000","status":{"code":2,"message":"The operation was denied."},"traceId":"0123456789abcdef0123456789abcdef"}]}]}]}"#;

    #[test]
    fn legacy_trace_files_still_list_and_read() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-trace-legacy-{}", rand::random::<u64>()));
        let traces = directory.join(TRACE_DIRECTORY);
        std::fs::create_dir_all(&traces).unwrap();
        std::fs::write(
            traces.join("1761600000000-legacy.json"),
            format!("{LEGACY}\n"),
        )
        .unwrap();
        let summaries = list(&directory, 10);
        assert_eq!(summaries.len(), 1);
        let summary = &summaries[0];
        assert_eq!(summary.id, "1761600000000-legacy");
        assert_eq!((summary.model_calls, summary.tool_calls), (1, 1));
        assert_eq!((summary.input_tokens, summary.output_tokens), (40, 9));
        assert_eq!(summary.model.as_deref(), Some("llama3.2"));
        assert_eq!(summary.duration_ms, 2000);
        assert!(summary.error.is_none());
        let spans = read(&directory, &summary.id).unwrap();
        assert_eq!(spans.len(), 3);
        assert!(spans[0].parent.is_none());
        let shell = &spans[2];
        assert_eq!(shell.parent.as_deref(), Some("1111111111111111"));
        assert_eq!(shell.error.as_deref(), Some("The operation was denied."));
        assert_eq!(shell.events, ["approval_requested"]);
        assert_eq!(
            spans[1].attributes["gen_ai.response.finish_reasons"],
            json!(["stop"])
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
