#![allow(clippy::unwrap_used)]
use super::*;
use crate::telemetry::{Mode, RunOutcome, Telemetry, TelemetryOptions, METRICS_DIRECTORY};
use crate::trace::{list, TRACE_DIRECTORY};
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const OK: &str = "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

struct Captured {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

type Seen = Arc<Mutex<Vec<Captured>>>;

async fn read_request(socket: &mut TcpStream) -> Option<Captured> {
    let mut buffer = vec![];
    let mut chunk = [0u8; 8192];
    let end = loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let path = lines.next()?.split(' ').nth(1)?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(key, _)| key == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[end..].to_vec();
    while body.len() < length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Some(Captured {
        path,
        headers,
        body,
    })
}

/// A fake collector that records every request and answers each with `response`.
async fn serve(response: String) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let seen: Seen = Arc::default();
    let record = seen.clone();
    let response = Arc::new(response);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let record = record.clone();
            let response = response.clone();
            tokio::spawn(async move {
                if let Some(request) = read_request(&mut socket).await {
                    record.lock().unwrap().push(request);
                    let _ = socket.write_all(response.as_bytes()).await;
                }
            });
        }
    });
    (base, seen)
}

/// A collector that accepts connections and never answers.
async fn hang() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        let mut held = vec![];
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    base
}

async fn until(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..250 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    done()
}

fn folder() -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("blackwall-export-unit-{}", rand::random::<u64>()));
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

fn config(endpoint: &str, protocol: Protocol) -> ExportConfig {
    ExportConfig {
        endpoint: Some(endpoint.into()),
        protocol,
        ..Default::default()
    }
}

fn no_secrets(_: &str) -> Result<Option<String>, crate::secrets::SecretError> {
    Ok(None)
}

async fn flush(telemetry: &Telemetry) {
    let telemetry = telemetry.clone();
    tokio::task::spawn_blocking(move || telemetry.flush())
        .await
        .unwrap();
}

fn trace_requests(seen: &Seen) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|request| request.path == "/v1/traces")
        .map(Captured::text)
        .collect()
}

fn run_one(telemetry: &Telemetry, id: &str, output: &str) {
    let trace = telemetry.run(id, Mode::Agent);
    let root = trace.root();
    let span = trace.begin(
        &root,
        "chat m",
        3,
        vec![("gen_ai.output.messages".into(), json!(trace.clip(output)))],
    );
    trace.end(&span, None);
    trace.record_model_call("m", "http://127.0.0.1:11434/v1", None, 0.5, None);
    trace.finish(RunOutcome::Complete);
}

#[test]
fn export_requires_https_except_loopback() {
    for (text, expected) in [
        ("https://otel.example.com", Ok("https://otel.example.com/")),
        (
            "https://cloud.langfuse.com/api/public/otel/",
            Ok("https://cloud.langfuse.com/api/public/otel"),
        ),
        (
            "https://otel.example.com/v1/traces",
            Ok("https://otel.example.com/"),
        ),
        ("http://127.0.0.1:4318", Ok("http://127.0.0.1:4318/")),
        ("http://[::1]:4318", Ok("http://[::1]:4318/")),
        ("http://localhost:4318", Ok("http://localhost:4318/")),
        ("http://LOCALHOST:4318", Ok("http://localhost:4318/")),
        (
            "http://collector.example:4318",
            Err(ExportError::InsecureEndpoint),
        ),
        (
            "http://jupiter.local:4318",
            Err(ExportError::InsecureEndpoint),
        ),
        ("http://127.0.0.2:4318", Err(ExportError::InsecureEndpoint)),
        (
            "http://localhost.example.com",
            Err(ExportError::InsecureEndpoint),
        ),
        ("http://10.0.0.5:4318", Err(ExportError::InsecureEndpoint)),
        (
            "https://user:secret@otel.example.com",
            Err(ExportError::InvalidEndpoint),
        ),
        (
            "https://otel.example.com/?api_key=secret",
            Err(ExportError::InvalidEndpoint),
        ),
        (
            "https://otel.example.com/#secret",
            Err(ExportError::InvalidEndpoint),
        ),
        ("ftp://otel.example.com", Err(ExportError::InvalidEndpoint)),
        ("otel.example.com:4318", Err(ExportError::InvalidEndpoint)),
        ("", Err(ExportError::InvalidEndpoint)),
    ] {
        assert_eq!(
            normalize_endpoint(text).map(String::from),
            expected.map(str::to_owned),
            "{text}"
        );
    }
    let traces = config("https://otel.example.com/base/", Protocol::HttpJson)
        .signal_url(Signal::Traces)
        .unwrap();
    assert_eq!(traces.as_str(), "https://otel.example.com/base/v1/traces");
    let metrics = config("https://otel.example.com", Protocol::HttpJson)
        .signal_url(Signal::Metrics)
        .unwrap();
    assert_eq!(metrics.as_str(), "https://otel.example.com/v1/metrics");
    let error = config("http://collector.example", Protocol::HttpJson)
        .validated()
        .unwrap_err();
    assert_eq!(error.kind(), "insecure_endpoint");
}

#[tokio::test(flavor = "multi_thread")]
async fn export_headers_come_from_credential_store() {
    let (base, seen) = serve(OK.into()).await;
    let directory = folder();
    let telemetry = start(&directory, false);
    let mut settings = config(&base, Protocol::HttpProtobuf);
    settings.headers = vec!["X-Api-Key".into(), "authorization".into()];
    let saved = serde_json::to_string(&settings.validated().unwrap()).unwrap();
    assert!(saved.contains("x-api-key"), "{saved}");
    // A missing value turns export off and says which header, with no request sent.
    telemetry.reload_export(&settings, &no_secrets);
    let status = telemetry.export_status();
    let failure = status.last_error.unwrap();
    assert_eq!((failure.kind, failure.status), ("missing_header", None));
    run_one(&telemetry, "missing", "hello");
    flush(&telemetry).await;
    assert!(seen.lock().unwrap().is_empty());

    let lookup = |name: &str| -> Result<Option<String>, crate::secrets::SecretError> {
        Ok(match name {
            "x-api-key" => Some("key-from-store-123".into()),
            "authorization" => Some("Bearer token-456".into()),
            _ => None,
        })
    };
    telemetry.reload_export(&settings, &lookup);
    run_one(&telemetry, "headers", "hello");
    flush(&telemetry).await;
    assert!(until(|| !trace_requests(&seen).is_empty()).await);
    let requests = seen.lock().unwrap();
    let request = requests
        .iter()
        .find(|request| request.path == "/v1/traces")
        .unwrap();
    assert_eq!(request.header("x-api-key"), Some("key-from-store-123"));
    assert_eq!(request.header("authorization"), Some("Bearer token-456"));
    assert_eq!(
        request.header("content-type"),
        Some("application/x-protobuf")
    );
    let decoded = ExportTraceServiceRequest::decode(request.body.as_slice()).unwrap();
    assert_eq!(decoded.resource_spans[0].scope_spans[0].spans.len(), 2);
    // Neither the settings nor the status hold a value.
    let status = serde_json::to_string(&telemetry.export_status()).unwrap();
    assert!(!status.contains("key-from-store") && !saved.contains("key-from-store"));
    drop(requests);
    telemetry.shutdown();
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn export_failure_never_blocks_run() {
    let base = hang().await;
    let directory = folder();
    let telemetry = start(&directory, false);
    telemetry.reload_export(&config(&base, Protocol::HttpJson), &no_secrets);
    let started = std::time::Instant::now();
    // Three runs of 1,000 spans: the first hangs in flight, the second waits, and the third
    // overflows the 2,048-span queue.
    for run in 0..3 {
        let trace = telemetry.run(&format!("hung-{run}"), Mode::Agent);
        let root = trace.root();
        for _ in 0..999 {
            let span = trace.begin(&root, "execute_tool read_file", 1, vec![]);
            trace.end(&span, None);
        }
        trace.finish(RunOutcome::Complete);
    }
    for run in 0..20 {
        run_one(&telemetry, &format!("small-{run}"), "hello");
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(until(|| telemetry.export_status().dropped_spans >= 1000).await);
    // The local files are written regardless.
    assert!(until(|| list(&directory, 200).len() == 23).await);
    let status = telemetry.export_status();
    assert_eq!(status.sent_spans, 0);
    assert!(status.last_success_unix_ms.is_none());
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn content_attributes_not_exported_when_capture_off() {
    let (base, seen) = serve(OK.into()).await;
    for content in [false, true] {
        let directory = folder();
        let telemetry = start(&directory, content);
        telemetry.reload_export(&config(&base, Protocol::HttpJson), &no_secrets);
        let trace = telemetry.run(&format!("content-{content}"), Mode::Agent);
        let root = trace.root();
        let span = trace.begin(
            &root,
            "chat m",
            3,
            vec![
                (
                    "gen_ai.input.messages".into(),
                    json!(trace.clip("the private prompt text")),
                ),
                (
                    "gen_ai.output.messages".into(),
                    json!(trace.clip("the private answer text")),
                ),
            ],
        );
        trace.end(&span, Some("failed reading private-file.txt"));
        trace.finish(RunOutcome::Complete);
        flush(&telemetry).await;
        assert!(until(|| trace_requests(&seen).len() == usize::from(content) + 1).await);
        let body = trace_requests(&seen).pop().unwrap();
        assert!(body.contains("chat m"), "{body}");
        assert_eq!(body.contains("private prompt"), content, "{body}");
        assert_eq!(body.contains("private answer"), content, "{body}");
        assert_eq!(body.contains("private-file"), content, "{body}");
        telemetry.shutdown();
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn export_off_by_default() {
    let defaults = ExportConfig::default();
    assert!(!defaults.is_on());
    assert_eq!(defaults.endpoint, None);
    assert!(!defaults.include_project);
    assert_eq!(
        serde_json::from_str::<ExportConfig>("{}").unwrap(),
        defaults
    );
    assert!(serde_json::from_str::<ExportConfig>(r#"{"headerValues":{"a":"b"}}"#).is_err());
    let directory = folder();
    let telemetry = start(&directory, true);
    run_one(&telemetry, "default", "hello");
    flush(&telemetry).await;
    assert_eq!(list(&directory, 10).len(), 1);
    let status = telemetry.export_status();
    assert_eq!(status, ExportStatus::default());
    // Applying the default settings starts nothing either.
    telemetry.set_export(&ExportConfig::default());
    assert_eq!(telemetry.export_status(), ExportStatus::default());
    telemetry.shutdown();
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_span_reports_http_status() {
    let (base, seen) =
        serve("HTTP/1.1 401 Unauthorized\r\ncontent-length: 6\r\n\r\nsecret".into()).await;
    let error = send_test_span_with(
        &config(&base, Protocol::HttpProtobuf),
        &HeaderMap::new(),
        "cli",
    )
    .await
    .unwrap_err();
    assert_eq!(error, ExportError::Status(401));
    assert_eq!((error.kind(), error.status()), ("http_status", Some(401)));
    assert!(!error.to_string().contains("secret"));
    let (base, ok) = serve(OK.into()).await;
    let status = send_test_span_with(&config(&base, Protocol::HttpJson), &HeaderMap::new(), "cli")
        .await
        .unwrap();
    assert_eq!(status, 200);
    let first = seen.lock().unwrap().remove(0);
    assert_eq!(first.path, "/v1/traces");
    let decoded = ExportTraceServiceRequest::decode(first.body.as_slice()).unwrap();
    assert_eq!(
        decoded.resource_spans[0].scope_spans[0].spans[0].name,
        "blackwall test span"
    );
    let body = ok.lock().unwrap().remove(0);
    assert_eq!(body.header("content-type"), Some("application/json"));
    let json: serde_json::Value = serde_json::from_slice(&body.body).unwrap();
    assert_eq!(
        json["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["name"],
        "blackwall test span"
    );
    // Unreachable and refused settings report a kind, not a status.
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port())
    };
    let error = send_test_span_with(
        &config(&closed, Protocol::HttpJson),
        &HeaderMap::new(),
        "cli",
    )
    .await
    .unwrap_err();
    assert_eq!((error.kind(), error.status()), ("connect", None));
    let error = send_test_span_with(&ExportConfig::default(), &HeaderMap::new(), "cli")
        .await
        .unwrap_err();
    assert_eq!(error, ExportError::Off);
}

#[tokio::test(flavor = "multi_thread")]
async fn export_does_not_follow_redirects() {
    let (target, followed) = serve(OK.into()).await;
    let (base, seen) = serve(format!(
        "HTTP/1.1 307 Temporary Redirect\r\nlocation: {target}/v1/traces\r\ncontent-length: 0\r\n\r\n"
    ))
    .await;
    let error = send_test_span_with(&config(&base, Protocol::HttpJson), &HeaderMap::new(), "cli")
        .await
        .unwrap_err();
    assert_eq!(error, ExportError::Redirect(307));
    assert_eq!(error.kind(), "redirect");
    assert_eq!(seen.lock().unwrap().len(), 1);
    // The run exporter does not follow one either.
    let directory = folder();
    let telemetry = start(&directory, false);
    telemetry.reload_export(&config(&base, Protocol::HttpJson), &no_secrets);
    run_one(&telemetry, "redirect", "hello");
    flush(&telemetry).await;
    assert!(until(|| telemetry.export_status().last_error.is_some()).await);
    let failure = telemetry.export_status().last_error.unwrap();
    assert_eq!((failure.kind, failure.status), ("redirect", Some(307)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(followed.lock().unwrap().is_empty());
    telemetry.shutdown();
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn secret_seen_late_is_scrubbed_from_earlier_exported_spans() {
    let (base, seen) = serve(OK.into()).await;
    let directory = folder();
    let telemetry = start(&directory, true);
    telemetry.reload_export(&config(&base, Protocol::HttpJson), &no_secrets);
    let trace = telemetry.run("late", Mode::Agent);
    let root = trace.root();
    // This span ends, and reaches the processor, before the secret is known.
    let early = trace.begin(
        &root,
        "chat m",
        3,
        vec![(
            "gen_ai.output.messages".into(),
            json!(trace.clip("Use latesecret77 here.")),
        )],
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
    flush(&telemetry).await;
    assert!(until(|| trace_requests(&seen).len() == 1).await);
    let exported = trace_requests(&seen).remove(0);
    assert!(!exported.contains("latesecret77"), "{exported}");
    assert!(exported.contains("Use [redacted] here."), "{exported}");
    // OTLP received exactly what the trace file holds.
    let id = list(&directory, 1).remove(0).id;
    let file = std::fs::read_to_string(directory.join(TRACE_DIRECTORY).join(format!("{id}.json")))
        .unwrap();
    assert_eq!(file.trim_end(), exported);
    telemetry.shutdown();
    std::fs::remove_dir_all(directory).unwrap();
}

fn metric_bodies(seen: &Seen) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|request| request.path == "/v1/metrics")
        .map(Captured::text)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn project_labels_not_exported_unless_allowed() {
    for include in [false, true] {
        let (base, seen) = serve(OK.into()).await;
        let directory = folder();
        let telemetry = start(&directory, false);
        let mut settings = config(&base, Protocol::HttpJson);
        settings.include_project = include;
        telemetry.reload_export(&settings, &no_secrets);
        for session in ["sess-a", "sess-b"] {
            let trace = telemetry
                .run(session, Mode::Agent)
                .labelled(Some(Path::new("/work/secret-project")), Some(session));
            trace.record_model_call(
                "m",
                "http://127.0.0.1:11434/v1",
                Some(&crate::protocol::TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    ..Default::default()
                }),
                0.5,
                None,
            );
            trace.finish(RunOutcome::Complete);
        }
        flush(&telemetry).await;
        assert!(until(|| !metric_bodies(&seen).is_empty()).await);
        let body = metric_bodies(&seen).join("\n");
        assert_eq!(body.contains("secret-project"), include, "{body}");
        assert_eq!(body.contains("sess-a"), include, "{body}");
        assert_eq!(body.contains(names::PROJECT), include, "{body}");
        // The local file keeps the labels the owner approved.
        let local = std::fs::read_dir(directory.join(METRICS_DIRECTORY))
            .unwrap()
            .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect::<String>();
        assert!(local.contains("secret-project"));
        if !include {
            // Stripped series are merged: one run-outcome point counting both runs.
            let request: ExportMetricsServiceRequest =
                serde_json::from_str(&metric_bodies(&seen)[0]).unwrap();
            let runs = request.resource_metrics[0].scope_metrics[0]
                .metrics
                .iter()
                .find(|metric| metric.name == names::RUN_OUTCOMES)
                .unwrap();
            let Some(Data::Sum(sum)) = &runs.data else {
                panic!("{runs:?}");
            };
            assert_eq!(sum.data_points.len(), 1);
            assert_eq!(
                sum.data_points[0].value,
                Some(number_data_point::Value::AsInt(2))
            );
            let tokens = request.resource_metrics[0].scope_metrics[0]
                .metrics
                .iter()
                .find(|metric| metric.name == names::GEN_AI_CLIENT_TOKEN_USAGE)
                .unwrap();
            let Some(Data::Histogram(histogram)) = &tokens.data else {
                panic!("{tokens:?}");
            };
            // One input and one output series, each counting both calls.
            assert_eq!(histogram.data_points.len(), 2);
            assert!(histogram.data_points.iter().all(|point| point.count == 2));
        }
        telemetry.shutdown();
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn retention_setting_round_trips() {
    use crate::telemetry::Retention;
    assert_eq!(serde_json::to_string(&Retention::default()).unwrap(), "400");
    assert_eq!(
        serde_json::to_string(&Retention::Forever).unwrap(),
        r#""forever""#
    );
    for retention in [
        Retention::Days(30),
        Retention::Days(3650),
        Retention::Forever,
    ] {
        let text = serde_json::to_string(&retention).unwrap();
        assert_eq!(serde_json::from_str::<Retention>(&text).unwrap(), retention);
    }
    for bad in ["29", "3651", r#""never""#, "-1", "true"] {
        assert!(serde_json::from_str::<Retention>(bad).is_err(), "{bad}");
    }
    assert_eq!("forever".parse::<Retention>(), Ok(Retention::Forever));
    assert_eq!(" 90 ".parse::<Retention>(), Ok(Retention::Days(90)));
    assert!("10".parse::<Retention>().is_err());
    assert!("lots".parse::<Retention>().is_err());
}

const CHILD: &str = "BLACKWALL_OTEL_EXPORT_ENV_CHILD";

/// Runs in a child process with hostile `OTEL_*` variables set; the parent checks what arrived.
#[test]
fn otel_export_env_child() {
    let Some(endpoint) = std::env::var_os(CHILD) else {
        return;
    };
    let directory = folder();
    let telemetry = start(&directory, false);
    telemetry.reload_export(
        &config(&endpoint.to_string_lossy(), Protocol::HttpJson),
        &no_secrets,
    );
    run_one(&telemetry, "env", "hello");
    telemetry.shutdown();
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn otel_env_vars_are_ignored() {
    let (base, seen) = serve(OK.into()).await;
    let (evil, evil_seen) = serve(OK.into()).await;
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "telemetry::export::tests::otel_export_env_child",
            "--test-threads=1",
        ])
        .env(CHILD, &base)
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", &evil)
        .env(
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            format!("{evil}/v1/traces"),
        )
        .env(
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
            format!("{evil}/v1/metrics"),
        )
        .env("OTEL_EXPORTER_OTLP_HEADERS", "x-evil=1")
        .env("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc")
        .env("OTEL_EXPORTER_OTLP_TIMEOUT", "1")
        .env("OTEL_SDK_DISABLED", "true")
        .env("OTEL_TRACES_EXPORTER", "none")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let status = tokio::task::spawn_blocking(move || {
        let mut child = child;
        child.wait()
    })
    .await
    .unwrap()
    .unwrap();
    assert!(status.success());
    assert!(until(|| !trace_requests(&seen).is_empty() && !metric_bodies(&seen).is_empty()).await);
    let requests = seen.lock().unwrap();
    for request in requests.iter() {
        assert_eq!(request.header("x-evil"), None);
        assert_eq!(request.header("content-type"), Some("application/json"));
    }
    assert!(evil_seen.lock().unwrap().is_empty());
}

#[test]
fn header_names_are_checked_and_values_never_print() {
    assert_eq!(header_name(" X-Api-Key ").unwrap(), "x-api-key");
    for bad in ["", "content-type", "Host", "x api", "x:y", &"a".repeat(65)] {
        assert_eq!(header_name(bad), Err(ExportError::InvalidHeader), "{bad}");
    }
    let mut settings = ExportConfig {
        headers: (0..17).map(|n| format!("x-{n}")).collect(),
        ..Default::default()
    };
    assert_eq!(settings.validated(), Err(ExportError::TooManyHeaders));
    settings.headers = vec!["X-A".into(), "x-a".into()];
    assert_eq!(settings.validated().unwrap().headers, vec!["x-a"]);
    let lookup = |_: &str| -> Result<Option<String>, crate::secrets::SecretError> {
        Ok(Some("bad\nvalue".into()))
    };
    assert_eq!(
        resolve_headers(&settings.validated().unwrap(), &lookup),
        Err(ExportError::InvalidHeaderValue)
    );
    let headers = resolve_headers(&settings.validated().unwrap(), &|_: &str| -> Result<
        Option<String>,
        crate::secrets::SecretError,
    > {
        Ok(Some("visible-value".into()))
    })
    .unwrap();
    assert!(!format!("{headers:?}").contains("visible-value"));
}
