//! Golden JSON-lines streams for `bw run --json`, one case per headless exit code.
//! Set `BLACKWALL_BLESS=1` to rewrite the files in `tests/golden/` from the current output.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use serde_json::{json, Value};
use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
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
            "blackwall-golden-{}-{timestamp}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        Self { root, project }
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bw"));
        command
            .args(arguments)
            .current_dir(&self.project)
            .env("BLACKWALL_DATA_DIR", self.root.join("data"))
            .env("NO_PROXY", "127.0.0.1,localhost")
            .stdin(Stdio::null())
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
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command(arguments).output().unwrap()
    }

    fn spawn(&self, arguments: &[&str]) -> Child {
        self.command(arguments).spawn().unwrap()
    }

    fn set(&self, key: &str, value: &str) {
        let output = self.run(&["config", "set", key, value]);
        assert!(output.status.success(), "config set {key} failed");
    }

    fn connect(&self, server: &Server) {
        self.set("model", "fixture-model");
        self.set("endpoint", &server.endpoint);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// One scripted reply to a chat request.
enum Reply {
    /// Server-sent event frames followed by `[DONE]`.
    Stream(Vec<Value>),
    /// An HTTP error status with an empty body.
    Status(u16),
    /// Read the request, then never answer.
    Stall,
}

/// A fake OpenAI-compatible endpoint that answers chat requests in order and reports each one.
struct Server {
    endpoint: String,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<usize>>,
    received: Receiver<()>,
}

impl Server {
    fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = stopped.clone();
        let (sender, received) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut served = 0;
            for reply in replies {
                let mut stream = loop {
                    if worker_stopped.load(Ordering::Relaxed) {
                        return served;
                    }
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            if !answered_probe(&mut stream) {
                                break stream;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("Fixture accept failed: {error}"),
                    }
                };
                read_request(&mut stream);
                served += 1;
                let _ = sender.send(());
                match reply {
                    Reply::Stream(frames) => {
                        let body = frames
                            .into_iter()
                            .map(|frame| format!("data: {frame}\n\n"))
                            .chain(std::iter::once("data: [DONE]\n\n".to_owned()))
                            .collect::<String>();
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    }
                    Reply::Status(code) => {
                        let _ = write!(stream, "HTTP/1.1 {code} Fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                    Reply::Stall => {
                        while !worker_stopped.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(20));
                        }
                    }
                }
            }
            served
        });
        Self {
            endpoint,
            stopped,
            worker: Some(worker),
            received,
        }
    }

    fn finish(mut self) -> usize {
        self.stopped.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Answer credential-free server probes (`GET`) with 404 so only chat requests are scripted.
fn answered_probe(stream: &mut TcpStream) -> bool {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut start = [0_u8; 4];
    let mut seen = 0;
    while seen < start.len() {
        match stream.peek(&mut start) {
            Ok(0) | Err(_) => return false,
            Ok(n) => seen = n,
        }
    }
    if &start != b"GET " {
        return false;
    }
    let _ = stream
        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    true
}

fn read_request(stream: &mut TcpStream) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("POST /v1/chat/completions HTTP/1.1"));
    let mut length = 0;
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
}

fn answer(text: &str) -> Reply {
    Reply::Stream(vec![
        json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":30,"total_tokens":150}}),
    ])
}

fn tool_call(name: &str, arguments: Value) -> Reply {
    Reply::Stream(vec![
        json!({"choices":[{
            "delta":{"tool_calls":[{
                "index":0,
                "id":"fixture-call",
                "type":"function",
                "function":{"name":name,"arguments":arguments.to_string()}
            }]},
            "finish_reason":"tool_calls"
        }]}),
        json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}}),
    ])
}

/// Replace values that change between runs: identifiers, timings, paths, and size estimates.
fn normalize(line: &str) -> Value {
    let mut event: Value = serde_json::from_str(line).unwrap_or_else(|_| json!({"raw": line}));
    if let Some(object) = event.as_object_mut() {
        for key in ["sessionId", "requestId", "approvalId", "reasoningMs"] {
            if object.get(key).is_some_and(|value| !value.is_null()) {
                object.insert(key.into(), json!(format!("<{key}>")));
            }
        }
        if object.contains_key("report") {
            object.insert("report".into(), json!("<report>"));
        }
        // The project path is spelled differently per platform (`/private` on macOS, `\\?\` on
        // Windows), so the approval's project line is replaced whole.
        if let Some(Value::String(detail)) = object.get_mut("detail") {
            if let Some(rest) = detail.strip_prefix("Project: ") {
                let rest = rest.split_once('\n').map_or("", |(_, rest)| rest);
                *detail = format!("Project: <project>\n{rest}");
            }
        }
    }
    let text = event.to_string();
    assert!(
        !text.contains("blackwall-golden-"),
        "Unnormalized fixture path in {text}"
    );
    event
}

fn assert_golden(name: &str, output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let actual = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| normalize(line).to_string())
        .collect::<Vec<_>>();
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.jsonl"));
    if env::var_os("BLACKWALL_BLESS").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("{}\n", actual.join("\n"))).unwrap();
    }
    let expected = fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("Missing golden file {}", path.display()));
    // `lines` also accepts a CRLF checkout.
    assert_eq!(
        actual,
        expected.lines().collect::<Vec<_>>(),
        "Golden {name} differs.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn result_line(output: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().last().expect("A result line");
    let result: Value = serde_json::from_str(line).unwrap();
    assert_eq!(result["type"], "result");
    result
}

fn assert_exit(output: &Output, code: i32, reason: &str) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(result_line(output)["reason"], reason);
}

#[test]
fn exit_0_complete() {
    let fixture = Fixture::new();
    let server = Server::start(vec![answer("All done.")]);
    fixture.connect(&server);
    let output = fixture.run(&["run", "--json", "say done"]);
    assert_exit(&output, 0, "complete");
    assert_golden("complete", &output);
    assert_eq!(server.finish(), 1);
}

#[test]
fn exit_1_error() {
    let fixture = Fixture::new();
    let server = Server::start(vec![]);
    fixture.connect(&server);
    let output = fixture.run(&["run", "--json", "/review --commit HEAD --base main"]);
    assert_exit(&output, 1, "error");
    assert_golden("error", &output);
    assert_eq!(server.finish(), 0);
}

#[test]
fn exit_2_usage() {
    let fixture = Fixture::new();
    let server = Server::start(vec![]);
    fixture.connect(&server);
    let output = fixture.run(&["run", "--json", "--timeout", "5", "say done"]);
    assert_exit(&output, 2, "usage");
    assert_golden("usage", &output);
    assert_eq!(server.finish(), 0);
}

#[test]
fn exit_3_denied() {
    let fixture = Fixture::new();
    fs::write(fixture.project.join("protected.txt"), "original\n").unwrap();
    let server = Server::start(vec![
        tool_call(
            "write_file",
            json!({"path":"protected.txt","content":"changed\n"}),
        ),
        answer("The write was denied."),
    ]);
    fixture.connect(&server);
    let output = fixture.run(&["run", "--json", "change protected.txt"]);
    assert_exit(&output, 3, "denied");
    assert_golden("denied", &output);
    assert_eq!(
        fs::read_to_string(fixture.project.join("protected.txt")).unwrap(),
        "original\n"
    );
    assert_eq!(server.finish(), 2);
}

#[test]
fn exit_4_iterations() {
    let fixture = Fixture::new();
    fs::write(fixture.project.join("notes.txt"), "notes\n").unwrap();
    let server = Server::start(vec![tool_call("read_file", json!({"path":"notes.txt"}))]);
    fixture.connect(&server);
    fixture.set("iterations", "1");
    let output = fixture.run(&["run", "--json", "read notes.txt"]);
    assert_exit(&output, 4, "iterations");
    // Tokens spent before the limit are still reported.
    assert_eq!(
        result_line(&output)["usage"],
        json!({"input":100,"output":10})
    );
    assert_golden("iterations", &output);
    assert_eq!(server.finish(), 1);
}

#[test]
fn exit_5_model() {
    let fixture = Fixture::new();
    let server = Server::start(vec![Reply::Status(500)]);
    fixture.connect(&server);
    let output = fixture.run(&["run", "--json", "say done"]);
    assert_exit(&output, 5, "model");
    assert_golden("model", &output);
    assert_eq!(server.finish(), 1);
}

#[test]
fn exit_6_budget() {
    let fixture = Fixture::new();
    let server = Server::start(vec![]);
    fixture.connect(&server);
    fixture.set("output", "512");
    fixture.set("context", "2048");
    let prompt = "word ".repeat(4000);
    let output = fixture.run(&["run", "--json", &prompt]);
    assert_exit(&output, 6, "budget");
    assert_golden("budget", &output);
    assert_eq!(server.finish(), 0);
}

#[test]
fn exit_7_timeout() {
    let fixture = Fixture::new();
    let server = Server::start(vec![Reply::Stall]);
    fixture.connect(&server);
    let output = fixture.run(&["run", "--json", "--timeout", "10", "think forever"]);
    assert_exit(&output, 7, "timeout");
    assert_golden("timeout", &output);
    assert_eq!(server.finish(), 1);
}

#[cfg(unix)]
#[test]
fn exit_130_interrupted() {
    let fixture = Fixture::new();
    let server = Server::start(vec![Reply::Stall]);
    fixture.connect(&server);
    let child = fixture.spawn(&["run", "--json", "think forever"]);
    server
        .received
        .recv_timeout(Duration::from_secs(30))
        .expect("The run never reached the model");
    assert!(Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap()
        .success());
    let output = child.wait_with_output().unwrap();
    assert_exit(&output, 130, "interrupted");
    assert_golden("interrupted", &output);
    assert_eq!(server.finish(), 1);
}

#[test]
fn result_line_has_reason_denied_and_usage() {
    let fixture = Fixture::new();
    let server = Server::start(vec![
        tool_call("shell", json!({"command":"echo hi"})),
        answer("The command was denied."),
    ]);
    fixture.connect(&server);
    let output = fixture.run(&["run", "--json", "run echo hi"]);
    assert_eq!(output.status.code(), Some(3));
    let result = result_line(&output);
    assert_eq!(result["status"], "complete");
    assert_eq!(result["reason"], "denied");
    assert_eq!(result["denied"], 1);
    // Both model calls count: the tool call (100 + 10) and the answer (120 + 30).
    assert_eq!(result["usage"], json!({"input":220,"output":40}));
    assert!(result["sessionId"].as_str().is_some());
    assert_eq!(result["answer"], "The command was denied.");
    assert_eq!(server.finish(), 2);
}

#[test]
fn timeout_flag_out_of_range_is_usage_error() {
    let fixture = Fixture::new();
    let server = Server::start(vec![]);
    fixture.connect(&server);
    for value in ["9", "86401", "0", "ten", "-5"] {
        let output = fixture.run(&["run", "--json", "--timeout", value, "say done"]);
        assert_exit(&output, 2, "usage");
    }
    let missing = fixture.run(&["run", "--json", "--timeout"]);
    assert_exit(&missing, 2, "usage");
    let no_prompt = fixture.run(&["run", "--json"]);
    assert_exit(&no_prompt, 2, "usage");
    // Without --json there is no result line, but the exit code is the same.
    let plain = fixture.run(&["run", "--timeout", "86401", "say done"]);
    assert_eq!(plain.status.code(), Some(2));
    assert_eq!(server.finish(), 0);
}
