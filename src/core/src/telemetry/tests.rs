#![allow(clippy::unwrap_used)]
use super::*;
use crate::trace::{list, read, TraceOptions, TRACE_DIRECTORY};
use file_spans::KEEP;
use serde_json::json;

fn folder() -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "blackwall-telemetry-unit-{}",
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

fn start(directory: &Path, content: bool) -> Telemetry {
    Telemetry::start(
        directory,
        TelemetryOptions {
            traces: true,
            content,
            ..Default::default()
        },
    )
}

fn trace_body(directory: &Path) -> String {
    let id = list(directory, 1).remove(0).id;
    std::fs::read_to_string(directory.join(TRACE_DIRECTORY).join(format!("{id}.json"))).unwrap()
}

/// Every metric data point in the metrics folder as `(name, attributes, point)`.
fn metric_points(directory: &Path) -> Vec<(String, serde_json::Map<String, Value>, Value)> {
    let mut points = vec![];
    let Ok(entries) = std::fs::read_dir(directory.join(METRICS_DIRECTORY)) else {
        return points;
    };
    for entry in entries {
        let body = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        for line in body.lines() {
            let request: Value = serde_json::from_str(line).unwrap();
            for resource in request["resourceMetrics"].as_array().unwrap() {
                for scope in resource["scopeMetrics"].as_array().unwrap() {
                    for metric in scope["metrics"].as_array().unwrap() {
                        let name = metric["name"].as_str().unwrap().to_owned();
                        let data = metric
                            .get("histogram")
                            .or_else(|| metric.get("sum"))
                            .unwrap();
                        for point in data["dataPoints"].as_array().unwrap() {
                            let attributes = point["attributes"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|pair| {
                                    let value = &pair["value"];
                                    let value = value
                                        .get("stringValue")
                                        .cloned()
                                        .unwrap_or_else(|| value.clone());
                                    (pair["key"].as_str().unwrap().to_owned(), value)
                                })
                                .collect();
                            points.push((name.clone(), attributes, point.clone()));
                        }
                    }
                }
            }
        }
    }
    points
}

fn number(value: &Value) -> f64 {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        .unwrap_or_default()
}

#[test]
fn disabled_traces_write_nothing() {
    let directory = folder();
    let telemetry = Telemetry::start(&directory, TelemetryOptions::default());
    let scope = telemetry.run("r", Mode::Agent);
    scope.record_model_call("m", "localhost:11434", None, 1.0, None);
    scope.finish(RunOutcome::Complete);
    telemetry.shutdown();
    assert!(!directory.join(TRACE_DIRECTORY).exists());
    assert!(!directory.join(METRICS_DIRECTORY).exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn stopped_run_is_written_as_failed_and_reads_back() {
    let directory = folder();
    let telemetry = start(&directory, false);
    let trace = telemetry.run("../run 1", Mode::Agent);
    let root = trace.root();
    let child = trace.begin(
        &root,
        "chat m",
        3,
        vec![("gen_ai.operation.name".into(), json!("chat"))],
    );
    trace.set(&child, vec![("gen_ai.usage.input_tokens".into(), json!(7))]);
    trace.end(&child, Some("token=abc123 failed"));
    drop(trace);
    telemetry.flush();
    let summaries = list(&directory, 10);
    assert_eq!(summaries.len(), 1);
    assert!(summaries[0].id.ends_with("-run1"));
    // With content capture off, a failure carries only its error class (§13 M2).
    assert_eq!(summaries[0].error.as_deref(), Some("cancelled"));
    assert_eq!(summaries[0].input_tokens, 7);
    // SDK timestamps are carried through: a real start time and a non-negative duration.
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(summaries[0].started_ms > 0);
    assert!(summaries[0].started_ms <= now_ms && now_ms - summaries[0].started_ms < 60_000);
    assert!(summaries[0].duration_ms < 60_000);
    let spans = read(&directory, &summaries[0].id).unwrap();
    assert_eq!(spans.len(), 2);
    assert!(spans[0].parent.is_none());
    assert_eq!(spans[1].parent.as_deref(), Some(spans[0].id.as_str()));
    let error = spans[1].error.as_deref().unwrap();
    assert!(!error.contains("abc123"), "{error}");
    assert!(read(&directory, "../x").is_err());
    assert!(read(&directory, "missing").is_err());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn stopped_run_keeps_its_message_with_content_on() {
    let directory = folder();
    let telemetry = start(&directory, true);
    drop(telemetry.run("stopped", Mode::Agent));
    telemetry.flush();
    let summary = list(&directory, 1).remove(0);
    assert_eq!(summary.error.as_deref(), Some("The run was stopped."));
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
    let telemetry = start(&directory, false);
    telemetry
        .run("new", Mode::Agent)
        .finish(RunOutcome::Complete);
    telemetry.flush();
    let files = crate::trace::list_files(&traces);
    assert_eq!(files.len(), KEEP);
    assert!(!files.contains(&format!("{:013}-old.json", 5)));
    assert!(files.iter().any(|name| name.ends_with("-new.json")));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn open_spans_are_cancelled_and_oversized_traces_drop_content() {
    let directory = folder();
    let telemetry = start(&directory, true);
    let trace = telemetry.run("big", Mode::Agent);
    let root = trace.root();
    // About 8 KiB on each of 501 spans passes the 4 MiB file limit.
    let text = "x".repeat(file_spans::MAX_FILE / (MAX_SPANS / 2));
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
    trace.finish(RunOutcome::Complete);
    telemetry.flush();
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
    assert!(trace_body(&directory).contains("blackwall.trace.content_removed"));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn spans_over_the_limit_are_counted_not_written() {
    let directory = folder();
    let telemetry = start(&directory, false);
    let trace = telemetry.run("many", Mode::Agent);
    let root = trace.root();
    for _ in 0..MAX_SPANS + 3 {
        let span = trace.begin(&root, "execute_tool list_files", 1, vec![]);
        trace.end(&span, None);
    }
    trace.finish(RunOutcome::Complete);
    telemetry.flush();
    let id = list(&directory, 1).remove(0).id;
    assert_eq!(read(&directory, &id).unwrap().len(), MAX_SPANS);
    assert!(trace_body(&directory).contains("blackwall.trace.dropped_spans"));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn secrets_found_anywhere_are_scrubbed_from_the_whole_trace() {
    let directory = folder();
    let telemetry = start(&directory, true);
    let trace = telemetry.run("scrub", Mode::Agent);
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
    trace.finish(RunOutcome::Complete);
    telemetry.flush();
    let body = trace_body(&directory);
    assert!(!body.contains("longsecret99"), "{body}");
    assert!(!body.contains("quote"), "{body}");
    assert!(body.contains(r#"flag\":\"true"#), "{body}");
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn secret_seen_late_is_scrubbed_from_earlier_exported_spans() {
    let directory = folder();
    let telemetry = start(&directory, true);
    let trace = telemetry.run("late", Mode::Agent);
    let root = trace.root();
    // This span has ended, so the SDK has already handed it to the processor.
    let early = trace.begin(
        &root,
        "chat m",
        3,
        vec![(
            "gen_ai.output.messages".into(),
            json!(trace.clip("Use latesecret77 here.")),
        )],
    );
    trace.event(
        &early,
        "approval_requested",
        vec![("blackwall.approval.kind".into(), json!("latesecret77"))],
    );
    trace.end(&early, None);
    let late = trace.begin(
        &root,
        "execute_tool read_file",
        1,
        vec![(
            "gen_ai.tool.call.result".into(),
            json!(trace.clip(r#"{"password":"latesecret77"}"#)),
        )],
    );
    trace.end(&late, None);
    trace.finish(RunOutcome::Complete);
    telemetry.flush();
    let body = trace_body(&directory);
    assert!(!body.contains("latesecret77"), "{body}");
    assert!(body.contains("Use [redacted] here."), "{body}");
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn content_keys_filtered_in_processor() {
    let directory = folder();
    let telemetry = start(&directory, false);
    let trace = telemetry.run("filtered", Mode::Agent);
    let root = trace.root();
    // A caller that forgets to check `content()` still cannot record content.
    let span = trace.begin(
        &root,
        "chat m",
        3,
        vec![
            ("gen_ai.operation.name".into(), json!("chat")),
            ("gen_ai.input.messages".into(), json!("PROMPT-SENTINEL")),
            (
                "gen_ai.system_instructions".into(),
                json!("SYSTEM-SENTINEL"),
            ),
            ("gen_ai.output.messages".into(), json!("OUTPUT-SENTINEL")),
            ("gen_ai.tool.call.arguments".into(), json!("ARG-SENTINEL")),
            ("gen_ai.tool.call.id".into(), json!("CALLID-SENTINEL")),
            ("blackwall.reasoning".into(), json!("REASONING-SENTINEL")),
        ],
    );
    trace.event(
        &span,
        "approval_requested",
        vec![
            ("blackwall.approval.kind".into(), json!("exec")),
            (
                "blackwall.approval.subject".into(),
                json!("SUBJECT-SENTINEL"),
            ),
        ],
    );
    trace.end(&span, None);
    trace.finish(RunOutcome::Complete);
    telemetry.flush();
    let body = trace_body(&directory);
    for sentinel in [
        "PROMPT",
        "SYSTEM",
        "OUTPUT",
        "ARG",
        "CALLID",
        "REASONING",
        "SUBJECT",
    ] {
        assert!(!body.contains(&format!("{sentinel}-SENTINEL")), "{body}");
    }
    let spans = read(&directory, &list(&directory, 1)[0].id).unwrap();
    let chat = spans.iter().find(|span| span.name == "chat m").unwrap();
    assert_eq!(chat.attributes["gen_ai.operation.name"], "chat");
    assert!(body.contains("blackwall.approval.kind"));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn errors_are_kind_only_when_content_off() {
    for content in [false, true] {
        let directory = folder();
        let telemetry = start(&directory, content);
        let trace = telemetry.run("errors", Mode::Agent);
        let root = trace.root();
        let typed = trace.begin(&root, "execute_tool shell", 1, vec![]);
        trace.set(&typed, vec![("error.type".into(), json!("execution"))]);
        trace.end(&typed, Some("cat /Users/owner/private.txt: denied"));
        let untyped = trace.begin(&root, "chat m", 3, vec![]);
        trace.end(&untyped, Some("upstream said /Users/owner/notes"));
        trace.set(&root, vec![("error.type".into(), json!("model"))]);
        trace.end(&root, Some("The model at /Users/owner failed."));
        trace.finish(RunOutcome::Error);
        telemetry.flush();
        let spans = read(&directory, &list(&directory, 1)[0].id).unwrap();
        let error = |name: &str| {
            spans
                .iter()
                .find(|span| span.name == name)
                .unwrap()
                .error
                .clone()
                .unwrap()
        };
        if content {
            assert!(error("execute_tool shell").contains("private.txt"));
            assert!(error("invoke_agent blackwall").contains("failed"));
        } else {
            assert_eq!(error("execute_tool shell"), "execution");
            assert_eq!(error("chat m"), "_OTHER");
            assert_eq!(error("invoke_agent blackwall"), "model");
            assert!(!trace_body(&directory).contains("/Users/owner"));
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn metrics_file_has_token_usage_and_duration_per_call() {
    let directory = folder();
    let telemetry = start(&directory, false);
    let scope = telemetry.run("metered", Mode::Agent);
    for (input, output) in [(100, 10), (120, 5)] {
        scope.record_model_call(
            "fixture-model",
            "http://127.0.0.1:11434/v1",
            Some(&TokenUsage {
                prompt_tokens: input,
                completion_tokens: output,
                total_tokens: input + output,
            }),
            0.5,
            None,
        );
    }
    scope.record_model_call(
        "fixture-model",
        "127.0.0.1:11434",
        None,
        0.2,
        Some("network"),
    );
    scope.record_tool("read_file", ToolOutcome::Ok);
    scope.record_tool("shell", ToolOutcome::Denied);
    scope.finish(RunOutcome::Complete);
    telemetry.shutdown();
    let files: Vec<_> = std::fs::read_dir(directory.join(METRICS_DIRECTORY))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        files,
        [file_metrics::file_name(file_metrics::day_of(
            SystemTime::now()
        ))]
    );
    let points = metric_points(&directory);
    let tokens = |kind: &str| {
        points
            .iter()
            .filter(|(name, attributes, _)| {
                name == names::GEN_AI_CLIENT_TOKEN_USAGE && attributes["gen_ai.token.type"] == kind
            })
            .map(|(_, attributes, point)| {
                assert_eq!(attributes["gen_ai.request.model"], "fixture-model");
                assert_eq!(attributes["gen_ai.provider.name"], "ollama");
                assert_eq!(attributes["server.address"], "127.0.0.1");
                assert_eq!(attributes["blackwall.mode"], "agent");
                (number(&point["sum"]), number(&point["count"]))
            })
            .fold((0.0, 0.0), |(sum, count), (s, c)| (sum + s, count + c))
    };
    assert_eq!(tokens("input"), (220.0, 2.0));
    assert_eq!(tokens("output"), (15.0, 2.0));
    let durations: Vec<_> = points
        .iter()
        .filter(|(name, ..)| name == names::GEN_AI_CLIENT_OPERATION_DURATION)
        .collect();
    let calls: f64 = durations
        .iter()
        .map(|(_, _, point)| number(&point["count"]))
        .sum();
    assert_eq!(calls, 3.0);
    assert!(durations
        .iter()
        .any(|(_, attributes, _)| attributes.get("error.type") == Some(&json!("network"))));
    let count = |metric: &str, key: &str, value: &str| -> f64 {
        points
            .iter()
            .filter(|(name, attributes, _)| name == metric && attributes[key] == value)
            .map(|(_, _, point)| number(&point["asInt"]))
            .sum()
    };
    assert_eq!(
        count(names::TOOL_INVOCATIONS, "blackwall.tool.outcome", "denied"),
        1.0
    );
    assert_eq!(
        count(names::TOOL_INVOCATIONS, "gen_ai.tool.name", "read_file"),
        1.0
    );
    assert_eq!(
        count(names::RUN_OUTCOMES, "blackwall.run.outcome", "complete"),
        1.0
    );
    let body = std::fs::read_to_string(directory.join(METRICS_DIRECTORY).join(&files[0])).unwrap();
    let first: Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
    assert_eq!(first["resourceMetrics"][0]["schemaUrl"], names::SCHEMA_URL);
    // Delta temporality, as AGGREGATION_TEMPORALITY_DELTA.
    assert!(body.contains(r#""aggregationTemporality":1"#), "{body}");
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn metrics_files_older_than_retention_are_removed() {
    let directory = folder();
    let metrics = directory.join(METRICS_DIRECTORY);
    std::fs::create_dir_all(&metrics).unwrap();
    let today = file_metrics::day_of(SystemTime::now());
    for age in [0, 29, 30, 31, 400, 401] {
        std::fs::write(metrics.join(file_metrics::file_name(today - age)), "{}\n").unwrap();
    }
    std::fs::write(metrics.join("notes.txt"), "keep").unwrap();
    let telemetry = Telemetry::start(
        &directory,
        TelemetryOptions {
            retention: Retention::Days(30),
            ..Default::default()
        },
    );
    telemetry.shutdown();
    let mut kept: Vec<_> = std::fs::read_dir(&metrics)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    kept.sort();
    let mut expected: Vec<_> = [0, 29, 30]
        .iter()
        .map(|age| file_metrics::file_name(today - age))
        .chain(["notes.txt".to_owned()])
        .collect();
    expected.sort();
    assert_eq!(kept, expected);
    // Fewer than 30 days is raised to 30.
    assert_eq!(Retention::Days(7).days(), Some(30));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn retention_forever_keeps_all() {
    let directory = folder();
    let metrics = directory.join(METRICS_DIRECTORY);
    std::fs::create_dir_all(&metrics).unwrap();
    let old = file_metrics::file_name(file_metrics::days_from_civil(1999, 1, 1));
    std::fs::write(metrics.join(&old), "{}\n").unwrap();
    let telemetry = Telemetry::start(
        &directory,
        TelemetryOptions {
            retention: Retention::Forever,
            ..Default::default()
        },
    );
    telemetry.shutdown();
    assert!(metrics.join(old).exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn retention_default_is_400_days() {
    assert_eq!(Retention::default(), Retention::Days(400));
    assert_eq!(TelemetryOptions::default().retention.days(), Some(400));
    let options = TelemetryOptions::from_trace(
        TraceOptions {
            enabled: true,
            content: false,
        },
        Surface::Cli,
    );
    assert_eq!(options.retention, Retention::Days(400));
    assert_eq!(serde_json::to_value(Retention::Forever).unwrap(), "forever");
    let directory = folder();
    let metrics = directory.join(METRICS_DIRECTORY);
    std::fs::create_dir_all(&metrics).unwrap();
    let today = file_metrics::day_of(SystemTime::now());
    for age in [400, 401] {
        std::fs::write(metrics.join(file_metrics::file_name(today - age)), "{}\n").unwrap();
    }
    Telemetry::start(&directory, TelemetryOptions::default()).shutdown();
    assert!(metrics.join(file_metrics::file_name(today - 400)).exists());
    assert!(!metrics.join(file_metrics::file_name(today - 401)).exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn trace_file_is_otlp_json_with_resource_and_schema() {
    let directory = folder();
    let telemetry = Telemetry::start(
        &directory,
        TelemetryOptions {
            traces: true,
            surface: Surface::Cli,
            ..Default::default()
        },
    );
    telemetry
        .run("shape", Mode::Agent)
        .finish(RunOutcome::Complete);
    telemetry.flush();
    let body: Value = serde_json::from_str(&trace_body(&directory)).unwrap();
    let resource = &body["resourceSpans"][0];
    assert_eq!(resource["schemaUrl"], names::SCHEMA_URL);
    assert_eq!(resource["scopeSpans"][0]["scope"]["name"], "blackwall");
    assert_eq!(resource["scopeSpans"][0]["schemaUrl"], names::SCHEMA_URL);
    let attributes = resource["resource"]["attributes"].as_array().unwrap();
    for (key, value) in [("service.name", "blackwall"), ("blackwall.surface", "cli")] {
        assert!(attributes
            .iter()
            .any(|pair| pair["key"] == key && pair["value"]["stringValue"] == value));
    }
    let root = &resource["scopeSpans"][0]["spans"][0];
    assert_eq!(root["traceId"].as_str().unwrap().len(), 32);
    assert_eq!(root["status"]["code"], 1);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn server_address_needs_a_real_host() {
    assert_eq!(
        server_parts("http://localhost:11434/v1"),
        (Some("localhost".into()), Some(11434))
    );
    assert_eq!(
        server_parts("127.0.0.1:8080"),
        (Some("127.0.0.1".into()), Some(8080))
    );
    for endpoint in ["unix:/Users/x/sock", "/Users/x/sock", "", "file:///Users/x"] {
        assert_eq!(server_parts(endpoint), (None, None), "{endpoint}");
    }
    let directory = folder();
    let telemetry = start(&directory, false);
    let scope = telemetry.run("socket", Mode::Agent);
    scope.record_model_call("m", "unix:/Users/x/sock", None, 0.1, None);
    scope.finish(RunOutcome::Complete);
    telemetry.shutdown();
    let points = metric_points(&directory);
    let duration = points
        .iter()
        .find(|(name, ..)| name == names::GEN_AI_CLIENT_OPERATION_DURATION)
        .unwrap();
    assert!(!duration.1.contains_key("server.address"));
    let body = std::fs::read_dir(directory.join(METRICS_DIRECTORY))
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<String>();
    assert!(!body.contains("/Users/x/sock"));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_full_writer_queue_drops_and_counts_runs() {
    let writer = file_spans::Writer::idle(1);
    let run = || Arc::new(RunState::new(PathBuf::from("unused.json"), false));
    let resource = Resource::builder_empty().build();
    writer.submit(run(), vec![], resource.clone());
    assert_eq!(writer.dropped_runs(), 0);
    // The queue is full, so the next run is dropped at once instead of blocking.
    let started = std::time::Instant::now();
    writer.submit(run(), vec![], resource);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(writer.dropped_runs(), 1);
}

const CHILD: &str = "BLACKWALL_OTEL_ENV_CHILD";

/// Runs in a child process with hostile `OTEL_*` variables set, so the parent's environment is
/// never mutated while other tests run.
#[test]
fn otel_env_vars_child() {
    let Some(directory) = std::env::var_os(CHILD) else {
        return;
    };
    let directory = PathBuf::from(directory);
    let telemetry = start(&directory, false);
    telemetry
        .run("env", Mode::Agent)
        .finish(RunOutcome::Complete);
    telemetry.shutdown();
    let body: Value = serde_json::from_str(&trace_body(&directory)).unwrap();
    let attributes = body["resourceSpans"][0]["resource"]["attributes"]
        .as_array()
        .unwrap()
        .clone();
    let service: Vec<_> = attributes
        .iter()
        .filter(|pair| pair["key"] == "service.name")
        .collect();
    assert_eq!(service.len(), 1);
    assert_eq!(service[0]["value"]["stringValue"], "blackwall");
    assert!(!attributes.iter().any(|pair| pair["key"] == "evil.key"));
}

#[test]
fn otel_env_vars_are_ignored() {
    let directory = folder();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "telemetry::tests::otel_env_vars_child",
            "--test-threads=1",
        ])
        .env(CHILD, &directory)
        .env("OTEL_TRACES_SAMPLER", "always_off")
        .env("OTEL_RESOURCE_ATTRIBUTES", "service.name=evil,evil.key=1")
        .env("OTEL_SERVICE_NAME", "evil")
        .env("OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT", "0")
        .env("OTEL_METRIC_EXPORT_INTERVAL", "1")
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    // The child wrote a trace despite the sampler variable.
    assert_eq!(list(&directory, 10).len(), 1);
    std::fs::remove_dir_all(directory).unwrap();
}
