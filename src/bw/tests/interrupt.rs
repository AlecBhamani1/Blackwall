#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use blackwall_core::storage::LocalStore;
use serde_json::{json, Value};
use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const WAIT_LIMIT: Duration = Duration::from_secs(10);
const PARTIAL_ANSWER: &str = "Fixture partial answer before approval.";

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "blackwall-cli-interrupt-{}-{timestamp}",
            std::process::id(),
        ));
        fs::create_dir_all(root.join("project")).unwrap();
        Self(root)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Kill a stalled CLI even if a bounded assertion fails while stdin is still open.
struct RunningCli(Child);

impl RunningCli {
    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + WAIT_LIMIT;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "The CLI did not exit after /quit"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for RunningCli {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_line(lines: &Receiver<String>, expected: &str) {
    let deadline = Instant::now() + WAIT_LIMIT;
    let mut observed = String::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = lines
            .recv_timeout(remaining)
            .unwrap_or_else(|error| panic!("Did not see {expected:?}: {error}\n{observed}"));
        observed.push_str(&line);
        if line.contains(expected) {
            return;
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Value {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("POST /v1/chat/completions HTTP/1.1"));
    let mut length = None;
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
    }
    let length = length.expect("Fixture requests must have a content length");
    assert!(length < 1024 * 1024);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[test]
fn interrupted_approval_preserves_partial_answer_and_later_slash_commands() {
    let fixture = Fixture::new();
    let project = fixture.0.join("project");
    let data = fixture.0.join("data");
    let protected = project.join("protected.txt");
    fs::write(&protected, "original\n").unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + WAIT_LIMIT;
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
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let request = read_request(&mut stream);
        let proposal = json!({"choices":[{
            "delta":{
                "content":PARTIAL_ANSWER,
                "tool_calls":[{
                    "index":0,
                    "id":"fixture-write",
                    "type":"function",
                    "function":{
                        "name":"write_file",
                        "arguments":json!({"path":"protected.txt","content":"changed\n"}).to_string()
                    }
                }]
            },
            "finish_reason":"tool_calls"
        }]});
        let body = format!("data: {proposal}\n\ndata: [DONE]\n\n");
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        stream.flush().unwrap();
        request
    });

    let mut command = Command::new(env!("CARGO_BIN_EXE_bw"));
    command
        .arg("chat")
        .current_dir(&project)
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
    command
        .env("BLACKWALL_DATA_DIR", &data)
        .env("BLACKWALL_MODEL_ENDPOINT", &endpoint)
        .env("BLACKWALL_MODEL", "fixture-model")
        .env("NO_PROXY", "127.0.0.1,localhost");

    let mut cli = RunningCli(command.spawn().unwrap());
    let mut stdin = cli.0.stdin.take().unwrap();
    let mut stdout = cli.0.stdout.take().unwrap();
    let output_reader = thread::spawn(move || {
        let mut output = String::new();
        stdout.read_to_string(&mut output).unwrap();
        output
    });
    let stderr = cli.0.stderr.take().unwrap();
    let (sender, lines) = mpsc::channel();
    let error_reader = thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut errors = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            errors.push_str(&line);
            let _ = sender.send(line);
        }
        errors
    });

    stdin.write_all(b"change protected.txt\n").unwrap();
    stdin.flush().unwrap();
    wait_for_line(&lines, "Allow this action once?");
    let interrupt = Command::new("kill")
        .args(["-INT", &cli.0.id().to_string()])
        .status()
        .unwrap();
    assert!(interrupt.success());
    wait_for_line(&lines, "Saved conversation");
    stdin
        .write_all(b"/model after-stop\n/settings\n/quit\n")
        .unwrap();
    stdin.flush().unwrap();
    drop(stdin);

    let status = cli.wait();
    let output = output_reader.join().unwrap();
    let errors = error_reader.join().unwrap();
    let request = server.join().expect("The mock model failed");
    assert!(status.success(), "The CLI failed:\n{output}\n{errors}");
    assert!(output.contains(PARTIAL_ANSWER), "{output}");
    assert!(output.contains("model: after-stop"), "{output}");
    assert!(errors.contains("Task stopped."), "{errors}");
    assert_eq!(fs::read_to_string(&protected).unwrap(), "original\n");
    assert_eq!(request["model"], "fixture-model");

    let store = LocalStore::open(&data).unwrap();
    let sessions = store.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    let session = store
        .load_session(sessions[0]["id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    let messages = session["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["content"], "change protected.txt");
    assert_eq!(messages[1]["content"], PARTIAL_ANSWER);
    assert_eq!(messages[1]["status"], "error");
    let settings: Value = serde_json::from_str(
        &store
            .setting("cli-settings")
            .unwrap()
            .expect("Saved CLI settings"),
    )
    .unwrap();
    assert_eq!(settings["model"], "after-stop");
}
