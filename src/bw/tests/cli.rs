#![allow(clippy::expect_used, clippy::unwrap_used)]

use serde_json::{json, Value};
use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    project: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "blackwall-cli-{}-{timestamp}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        Self { root, project }
    }

    fn run(&self, arguments: &[&str], input: &str) -> Output {
        self.run_from(arguments, input, &self.project, &[])
    }

    fn run_from(
        &self,
        arguments: &[&str],
        input: &str,
        project: &Path,
        environment: &[(&str, &str)],
    ) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bw"));
        command
            .args(arguments)
            .current_dir(project)
            .env("BLACKWALL_DATA_DIR", self.root.join("data"))
            .env("NO_PROXY", "127.0.0.1,localhost")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, _) in env::vars_os() {
            if key
                .to_str()
                .is_some_and(|key| key.starts_with("BLACKWALL_MODEL"))
            {
                command.env_remove(key);
            }
        }
        for key in [
            "OLLAMA_HOST",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env_remove(key);
        }
        for (key, value) in environment {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn success(&self, arguments: &[&str], input: &str) -> String {
        let output = self.run(arguments, input);
        assert!(
            output.status.success(),
            "bw {arguments:?} failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr)
                .lines()
                .any(|line| line.starts_with("bw:")),
            "bw {arguments:?} reported an error: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn configure_model(&self) {
        self.success(&["config", "set", "model", "fixture-model"], "");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A rejecting HTTP fixture proves settings commands never contact a model.
struct OfflineGuard {
    endpoint: String,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<usize>>,
}

impl OfflineGuard {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = stopped.clone();
        let worker = thread::spawn(move || {
            let mut calls = 0;
            while !worker_stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        calls += 1;
                        let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("Offline fixture accept failed: {error}"),
                }
            }
            calls
        });
        Self {
            endpoint,
            stopped,
            worker: Some(worker),
        }
    }

    fn assert_unused(mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        assert_eq!(
            self.worker.take().unwrap().join().unwrap(),
            0,
            "A settings command called a model"
        );
    }
}

impl Drop for OfflineGuard {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct ModelServer {
    endpoint: String,
    worker: JoinHandle<Vec<Value>>,
}

impl ModelServer {
    fn start(responses: Vec<Vec<Value>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            for frames in responses {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "The CLI did not call the fixture"
                            );
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("Fixture accept failed: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                requests.push(read_request(&mut stream));
                let frames = frames
                    .into_iter()
                    .map(|frame| format!("data: {frame}\n\n"))
                    .chain(std::iter::once("data: [DONE]\n\n".to_owned()))
                    .collect::<Vec<_>>();
                let length: usize = frames.iter().map(String::len).sum();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n").unwrap();
                for frame in frames {
                    stream.write_all(frame.as_bytes()).unwrap();
                    stream.flush().unwrap();
                }
            }
            requests
        });
        Self { endpoint, worker }
    }

    fn finish(self) -> Vec<Value> {
        self.worker.join().expect("The mock model failed")
    }
}

fn read_request(stream: &mut TcpStream) -> Value {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("POST /v1/chat/completions HTTP/1.1"));
    let mut length = None;
    let mut headers = serde_json::Map::new();
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.to_ascii_lowercase(), json!(value.trim()));
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
    }
    let length = length.expect("Fixture requests must have a content length");
    assert!(length < 1024 * 1024);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let mut request: Value = serde_json::from_slice(&body).unwrap();
    request["_fixtureHeaders"] = Value::Object(headers);
    request
}

fn answer(text: &str) -> Vec<Value> {
    vec![
        json!({"choices":[{"delta":{"content":"Fixture "},"finish_reason":null}]}),
        json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]}),
    ]
}

fn proposed_write() -> Vec<Value> {
    vec![json!({"choices":[{
        "delta":{"tool_calls":[{
            "index":0,
            "id":"fixture-write",
            "type":"function",
            "function":{
                "name":"write_file",
                "arguments":json!({"path":"protected.txt","content":"changed\n"}).to_string()
            }
        }]},
        "finish_reason":"tool_calls"
    }]})]
}

fn message_contents(request: &Value, role: &str) -> Vec<String> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == role)
        .map(|message| message["content"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn help_and_command_catalog_are_available_without_a_model() {
    let fixture = Fixture::new();
    let help = fixture.success(&["--help"], "");
    assert!(help.contains("bw chat"));
    assert!(help.contains("bw config"));
    let commands = fixture.success(&["commands"], "");
    for name in ["/commands", "/model", "/endpoint", "/settings"] {
        assert!(
            commands.contains(name),
            "Missing command {name}: {commands}"
        );
    }
}

#[test]
fn saved_model_is_used_by_later_processes_and_request_preview() {
    let fixture = Fixture::new();
    let defaults = fixture.success(&["config", "show"], "");
    assert!(!defaults.trim().is_empty());
    fixture.configure_model();
    let settings = fixture.success(&["config", "show"], "");
    assert!(settings.contains("fixture-model"));
    let preview = fixture.success(&["request", "hello"], "");
    let request: Value = serde_json::from_str(&preview).unwrap();
    assert_eq!(request["model"], "fixture-model");
    assert!(request["endpoint"].as_str().is_some());
    assert_eq!(request["messages"][0]["content"], "hello");
}

#[test]
fn secret_bearing_endpoint_is_rejected_without_changing_saved_configuration() {
    let fixture = Fixture::new();
    fixture.success(
        &[
            "config",
            "set",
            "endpoint",
            "http://fixture.invalid:11434/v1",
        ],
        "",
    );
    let before = fixture.success(&["config", "show"], "");
    for endpoint in [
        "http://user:fixture-secret@fixture.invalid:11434/v1",
        "http://fixture.invalid:11434/v1?key=fixture-secret",
        "http://fixture.invalid:11434/v1#fixture-secret",
    ] {
        let rejected = fixture.run(&["config", "set", "endpoint", endpoint], "");
        assert!(!rejected.status.success());
        assert!(!String::from_utf8_lossy(&rejected.stdout).contains("fixture-secret"));
        assert!(!String::from_utf8_lossy(&rejected.stderr).contains("fixture-secret"));
        assert_eq!(fixture.success(&["config", "show"], ""), before);
    }
}

#[test]
fn bare_cli_and_chat_change_settings_without_calling_a_model() {
    for arguments in [&[][..], &["chat"][..]] {
        let fixture = Fixture::new();
        let model = OfflineGuard::start();
        fixture.success(&["config", "set", "endpoint", &model.endpoint], "");
        let transcript = fixture.success(
            arguments,
            "/model fixture-model\n/web on\n/memory on\n/settings\n/quit\n",
        );
        assert!(transcript.contains("fixture-model"));
        let settings = fixture.success(&["config", "show"], "");
        assert!(settings.contains("fixture-model"));
        assert!(settings.contains("web: on"));
        assert!(settings.contains("memory: on"));
        model.assert_unused();
    }
}

#[test]
fn unknown_slash_command_does_not_end_the_interactive_session() {
    let fixture = Fixture::new();
    let model = OfflineGuard::start();
    fixture.success(&["config", "set", "endpoint", &model.endpoint], "");
    let output = fixture.run(
        &["chat"],
        "/fixture-unknown-command\n/model fixture-model\n/settings\n/quit\n",
    );
    assert!(output.status.success());
    let transcript = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(transcript.to_lowercase().contains("unknown"));
    assert!(fixture
        .success(&["config", "show"], "")
        .contains("fixture-model"));
    model.assert_unused();
}

#[test]
fn setup_commits_all_settings_together_and_cancels_without_partial_changes() {
    let fixture = Fixture::new();
    let model = OfflineGuard::start();
    fixture.success(&["config", "set", "endpoint", &model.endpoint], "");
    fixture.configure_model();
    let before = fixture.success(&["config", "show"], "");
    for input in [
        "http://changed.invalid:11434/v1\n",
        "http://changed.invalid:11434/v1\nchanged-model\ninvalid-switch\n",
        "http://user:fixture-secret@changed.invalid/v1\nchanged-model\non\non\n",
    ] {
        let cancelled = fixture.run(&["setup"], input);
        assert!(!cancelled.status.success());
        assert!(!String::from_utf8_lossy(&cancelled.stderr).contains("fixture-secret"));
        assert_eq!(fixture.success(&["config", "show"], ""), before);
    }
    fixture.success(&["setup"], "\nconfigured-model\non\non\n");
    let after = fixture.success(&["config", "show"], "");
    assert!(after.contains("model: configured-model"));
    assert!(after.contains(&format!("endpoint: {}", model.endpoint)));
    assert!(after.contains("web: on"));
    assert!(after.contains("memory: on"));
    model.assert_unused();
}

#[test]
fn interactive_chat_streams_multiple_turns_and_retains_history() {
    let fixture = Fixture::new();
    fixture.configure_model();
    let model = ModelServer::start(vec![answer("first reply"), answer("second reply")]);
    fixture.success(&["config", "set", "endpoint", &model.endpoint], "");
    let transcript = fixture.success(&["chat"], "hello\nfollow up\n/quit\n");
    assert!(transcript.contains("Fixture first reply"), "{transcript}");
    assert!(transcript.contains("Fixture second reply"), "{transcript}");
    let requests = model.finish();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["model"], "fixture-model");
    assert_eq!(requests[1]["model"], "fixture-model");
    assert_eq!(message_contents(&requests[0], "user"), ["hello"]);
    assert_eq!(
        message_contents(&requests[1], "user"),
        ["hello", "follow up"]
    );
    assert_eq!(
        message_contents(&requests[1], "assistant"),
        ["Fixture first reply"]
    );
    let sessions = fixture.success(&["sessions"], "");
    assert!(sessions.contains("hello"));
}

#[test]
fn declining_a_proposed_file_write_keeps_the_file_unchanged() {
    let fixture = Fixture::new();
    fixture.configure_model();
    let protected = fixture.project.join("protected.txt");
    fs::write(&protected, "original\n").unwrap();
    let model = ModelServer::start(vec![proposed_write(), answer("write was denied")]);
    fixture.success(&["config", "set", "endpoint", &model.endpoint], "");
    let transcript = fixture.success(&["chat"], "change protected.txt\nno\n/quit\n");
    assert!(
        transcript.contains("Fixture write was denied"),
        "{transcript}"
    );
    assert_eq!(fs::read_to_string(&protected).unwrap(), "original\n");
    let requests = model.finish();
    let results = message_contents(&requests[1], "tool");
    assert_eq!(results.len(), 1);
    assert!(results[0].to_lowercase().contains("denied"));
    assert_eq!(
        message_contents(&requests[1], "user"),
        ["change protected.txt"]
    );
}

#[test]
fn a_write_requires_explicit_yes_and_is_denied_when_stdin_ends() {
    for (decision, expected) in [("yes\n", "changed\n"), ("", "original\n")] {
        let fixture = Fixture::new();
        fixture.configure_model();
        let protected = fixture.project.join("protected.txt");
        fs::write(&protected, "original\n").unwrap();
        let model = ModelServer::start(vec![proposed_write(), answer("approval handled")]);
        fixture.success(&["config", "set", "endpoint", &model.endpoint], "");
        let transcript = fixture.success(&["run", "change protected.txt"], decision);
        assert!(
            transcript.contains("Fixture approval handled"),
            "{transcript}"
        );
        assert_eq!(fs::read_to_string(&protected).unwrap(), expected);
        let requests = model.finish();
        let results = message_contents(&requests[1], "tool");
        assert_eq!(results.len(), 1);
        if decision.is_empty() {
            assert!(results[0].contains("denied"));
        } else {
            assert!(results[0].contains("Saved protected.txt"));
        }
    }
}

#[test]
fn resuming_from_another_directory_reads_files_from_the_saved_project() {
    let fixture = Fixture::new();
    fixture.configure_model();
    fs::write(fixture.project.join("marker.txt"), "saved project marker").unwrap();
    let other = fixture.root.join("other-project");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("marker.txt"), "different project marker").unwrap();
    let read = vec![json!({"choices":[{
        "delta":{"tool_calls":[{
            "index":0,
            "id":"fixture-read",
            "type":"function",
            "function":{
                "name":"read_file",
                "arguments":json!({"path":"marker.txt"}).to_string()
            }
        }]},
        "finish_reason":"tool_calls"
    }]})];
    let model = ModelServer::start(vec![
        answer("initial answer"),
        read,
        answer("resumed answer"),
    ]);
    fixture.success(&["config", "set", "endpoint", &model.endpoint], "");
    fixture.success(&["run", "initial task"], "");
    let sessions = fixture.success(&["sessions"], "");
    let id = sessions.split_whitespace().next().unwrap();
    let output = fixture.run_from(&["resume", id, "read marker.txt"], "", &other, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Fixture resumed answer"),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = model.finish();
    assert_eq!(
        message_contents(&requests[1], "user"),
        ["initial task", "read marker.txt"]
    );
    assert_eq!(
        message_contents(&requests[1], "assistant"),
        ["Fixture initial answer"]
    );
    let reads = message_contents(&requests[2], "tool");
    assert_eq!(reads.len(), 1);
    assert!(reads[0].contains("saved project marker"));
    assert!(!reads[0].contains("different project marker"));
}

#[test]
fn resume_rejects_desktop_chat_and_invalid_modern_workspaces_but_accepts_legacy_cli() {
    let fixture = Fixture::new();
    let model = OfflineGuard::start();
    fixture.success(&["config", "set", "endpoint", &model.endpoint], "");
    let store = blackwall_core::storage::LocalStore::open(&fixture.root.join("data")).unwrap();
    for (id, mode, workspace) in [
        ("desktop-chat", Some("chat"), Some(fixture.project.clone())),
        ("agent-missing-path", Some("agent"), None),
        (
            "agent-invalid-path",
            Some("agent"),
            Some(fixture.root.join("missing-project")),
        ),
        ("cli_legacy", None, None),
    ] {
        let mut session = json!({"id":id,"title":"fixture session","updatedAt":1,"messages":[]});
        if let Some(mode) = mode {
            session["mode"] = json!(mode);
        }
        if let Some(workspace) = workspace {
            session["workspace"] = json!(workspace);
        }
        store.save_session(&session).unwrap();
    }
    drop(store);
    for id in ["desktop-chat", "agent-missing-path", "agent-invalid-path"] {
        let output = fixture.run(&["resume", id], "/quit\n");
        assert!(
            !output.status.success(),
            "Invalid session {id} was resumed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    let transcript = fixture.success(&["resume", "cli_legacy"], "/settings\n/quit\n");
    assert!(transcript.contains(
        fs::canonicalize(&fixture.project)
            .unwrap()
            .to_str()
            .unwrap()
    ));
    model.assert_unused();
}

#[test]
fn environment_access_keys_are_sent_only_to_the_configured_origin() {
    let fixture = Fixture::new();
    fixture.configure_model();
    let model = ModelServer::start(vec![
        answer("same origin"),
        answer("different origin"),
        answer("fallback origin"),
    ]);
    fixture.success(&["config", "set", "endpoint", &model.endpoint], "");
    for environment in [
        vec![
            ("BLACKWALL_MODEL_ENDPOINT", model.endpoint.as_str()),
            ("BLACKWALL_MODEL_API_KEY", "fixture-access-key"),
        ],
        vec![
            ("BLACKWALL_MODEL_ENDPOINT", "http://other.invalid:11434/v1"),
            ("BLACKWALL_MODEL_API_KEY", "fixture-access-key"),
        ],
        vec![
            ("BLACKWALL_MODEL_ENDPOINT", ""),
            ("OLLAMA_HOST", model.endpoint.as_str()),
            ("BLACKWALL_MODEL_API_KEY", "fixture-access-key"),
        ],
    ] {
        let output = fixture.run_from(
            &["run", "check credentials"],
            "",
            &fixture.project,
            &environment,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-access-key"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-access-key"));
    }
    let requests = model.finish();
    assert_eq!(
        requests[0]["_fixtureHeaders"]["authorization"],
        "Bearer fixture-access-key"
    );
    assert!(requests[1]["_fixtureHeaders"]
        .get("authorization")
        .is_none());
    assert_eq!(
        requests[2]["_fixtureHeaders"]["authorization"],
        "Bearer fixture-access-key"
    );
}
