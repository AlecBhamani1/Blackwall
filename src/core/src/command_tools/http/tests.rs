use super::*;
use crate::{
    command_tools::{CommandTools, COMMAND_TOOLS_FILE},
    protocol::{ApprovalDecision, ApprovalKind, ResolveApprovalRequest},
};
use serde_json::json;
use std::sync::Mutex;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

/// A fake server that records each raw request and answers every one with `response`.
async fn serve(response: Vec<u8>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(vec![]));
    let requests = log.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let response = response.clone();
            let requests = requests.clone();
            tokio::spawn(async move {
                let mut head = vec![];
                let mut byte = [0_u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match socket.read(&mut byte).await {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let text = String::from_utf8_lossy(&head).into_owned();
                let length = text
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                let mut body = vec![0_u8; length];
                if socket.read_exact(&mut body).await.is_err() {
                    return;
                }
                requests
                    .lock()
                    .unwrap()
                    .push(format!("{text}{}", String::from_utf8_lossy(&body)));
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (origin, log)
}

fn reply(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn ok(body: &str) -> Vec<u8> {
    reply(
        "200 OK",
        "Content-Type: application/json\r\n",
        body.as_bytes(),
    )
}

async fn load_file(file: Value) -> (CommandTools, std::path::PathBuf) {
    let directory =
        std::env::temp_dir().join(format!("blackwall-http-tools-{}", rand::random::<u64>()));
    tokio::fs::create_dir_all(&directory).await.unwrap();
    tokio::fs::write(directory.join(COMMAND_TOOLS_FILE), file.to_string())
        .await
        .unwrap();
    (CommandTools::load(&directory).await, directory)
}

async fn tool(definition: Value) -> (CommandTools, std::path::PathBuf) {
    let (tools, directory) = load_file(json!({"tools":[definition]})).await;
    assert_eq!(tools.error, None, "{definition}");
    (tools, directory)
}

type Asked = Arc<Mutex<Vec<(ApprovalKind, String)>>>;

/// Allows every approval, recording each kind and detail.
fn allow_all(approvals: &Approvals) -> (impl Fn(AgentEvent) + Send + Sync, Asked) {
    let decisions = approvals.clone();
    let asked = Arc::new(Mutex::new(vec![]));
    let seen = asked.clone();
    let emit = move |event| {
        if let AgentEvent::ApprovalRequest {
            request_id,
            approval_id,
            kind,
            detail,
            ..
        } = event
        {
            seen.lock().unwrap().push((kind, detail));
            decisions
                .resolve(ResolveApprovalRequest {
                    request_id,
                    approval_id,
                    decision: ApprovalDecision::Allow,
                })
                .unwrap();
        }
    };
    (emit, asked)
}

fn no_secrets() -> Lookup {
    Arc::new(|_: &str| Ok(None))
}

async fn call(
    tools: &CommandTools,
    directory: &Path,
    name: &str,
    arguments: Value,
    approvals: &Approvals,
    emit: &(dyn Fn(AgentEvent) + Send + Sync),
    secrets: Lookup,
) -> Result<String, ToolError> {
    let workspace = crate::tools::Workspace::open(directory).unwrap();
    let result = tools
        .get(name)
        .unwrap()
        .run_with(
            &workspace,
            &arguments.to_string(),
            "run",
            approvals,
            emit,
            Duration::from_secs(10),
            secrets,
        )
        .await;
    drop(workspace);
    result
}

fn requests(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    log.lock().unwrap().clone()
}

#[tokio::test]
async fn http_tool_posts_json_and_caps_response() {
    let (origin, log) = serve(ok(r#"{"id":7}"#)).await;
    let (tools, directory) = tool(json!({
        "name":"create_issue","description":"Create an issue","kind":"http",
        "url":format!("{origin}/issues"),"method":"POST","body":"json",
        "parameters":{"type":"object","properties":{"title":{"type":"string"},"urgent":{"type":"boolean"}},"required":["title"]}
    }))
    .await;
    let approvals = Approvals::default();
    let (emit, _) = allow_all(&approvals);
    let output = call(
        &tools,
        &directory,
        "create_issue",
        json!({"title":"Broken","urgent":true}),
        &approvals,
        &emit,
        no_secrets(),
    )
    .await
    .unwrap();
    assert_eq!(output, "HTTP 200\n{\"id\":7}");
    let request = requests(&log).remove(0);
    assert!(request.starts_with("POST /issues HTTP/1.1"), "{request}");
    assert!(request
        .to_ascii_lowercase()
        .contains("content-type: application/json"));
    assert!(
        request.ends_with(r#"{"title":"Broken","urgent":true}"#),
        "{request}"
    );
    // A response over 64 KiB is refused rather than passed to the model.
    let (big, _) = serve(ok(&"x".repeat(MAX_RESPONSE_BYTES + 1))).await;
    let (tools_big, directory_big) = tool(json!({
        "name":"big","description":"Big","kind":"http","url":format!("{big}/big")
    }))
    .await;
    let error = call(
        &tools_big,
        &directory_big,
        "big",
        json!({}),
        &approvals,
        &emit,
        no_secrets(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("64 KiB"), "{error}");
    // A failure shows the status and a short excerpt, never the whole body.
    let (failing, _) = serve(reply(
        "500 Internal Server Error",
        "",
        "e".repeat(5000).as_bytes(),
    ))
    .await;
    let (tools_failing, directory_failing) = tool(json!({
        "name":"failing","description":"Fails","kind":"http","url":format!("{failing}/x")
    }))
    .await;
    let error = call(
        &tools_failing,
        &directory_failing,
        "failing",
        json!({}),
        &approvals,
        &emit,
        no_secrets(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.starts_with("The service returned HTTP 500"),
        "{error}"
    );
    assert!(error.len() < 300, "{error}");
    // Redirects are not followed.
    let (moved, _) = serve(reply(
        "302 Found",
        "Location: https://example.com/\r\n",
        b"",
    ))
    .await;
    let (tools_moved, directory_moved) = tool(json!({
        "name":"moved","description":"Moved","kind":"http","url":format!("{moved}/x")
    }))
    .await;
    let error = call(
        &tools_moved,
        &directory_moved,
        "moved",
        json!({}),
        &approvals,
        &emit,
        no_secrets(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("redirect"), "{error}");
    for directory in [directory, directory_big, directory_failing, directory_moved] {
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[tokio::test]
async fn http_tool_https_only_except_loopback() {
    for (url, valid) in [
        ("https://gitlab.example.com/api/v4/projects", true),
        ("http://127.0.0.1:8080/x", true),
        ("http://[::1]:8080/x", true),
        ("http://gitlab.example.com/api", false),
        ("http://localhost:8080/x", false),
        ("http://127.0.0.2/x", false),
        ("ftp://example.com/x", false),
        ("https://user:pass@example.com/x", false),
        ("https://example.com/x#part", false),
        ("example.com/x", false),
    ] {
        let (tools, directory) = load_file(json!({"tools":[
            {"name":"fetch","description":"Fetch","kind":"http","url":url}
        ]}))
        .await;
        assert_eq!(tools.error.is_none(), valid, "{url}: {:?}", tools.error);
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[tokio::test]
async fn http_tool_approval_is_network_kind() {
    let (origin, _) = serve(ok("{}")).await;
    let (tools, directory) = tool(json!({
        "name":"lookup","description":"Look up","kind":"http",
        "url":format!("{origin}/items/{{id}}?api_key=fixed&q={{query}}")
    }))
    .await;
    let approvals = Approvals::default();
    let (emit, asked) = allow_all(&approvals);
    call(
        &tools,
        &directory,
        "lookup",
        json!({"id":"a b","query":"glpat-abc"}),
        &approvals,
        &emit,
        no_secrets(),
    )
    .await
    .unwrap();
    let asked = asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].0, ApprovalKind::Network);
    // The resolved URL is shown, with secret-looking query values hidden.
    assert!(
        asked[0].1.contains(&format!(
            "Request: GET {origin}/items/a%20b?api_key=[redacted]&q=[redacted]"
        )),
        "{}",
        asked[0].1
    );
    assert!(!asked[0].1.contains("fixed") && !asked[0].1.contains("glpat-abc"));
    // A denied request is never sent.
    let (refused, log) = serve(ok("{}")).await;
    let (tools_refused, directory_refused) = tool(json!({
        "name":"refused","description":"Refused","kind":"http","url":format!("{refused}/x")
    }))
    .await;
    let deny = {
        let decisions = approvals.clone();
        move |event| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } = event
            {
                decisions
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision: ApprovalDecision::Deny,
                    })
                    .unwrap();
            }
        }
    };
    assert!(matches!(
        call(
            &tools_refused,
            &directory_refused,
            "refused",
            json!({}),
            &approvals,
            &deny,
            no_secrets()
        )
        .await,
        Err(ToolError::Denied)
    ));
    assert!(requests(&log).is_empty());
    std::fs::remove_dir_all(directory).unwrap();
    std::fs::remove_dir_all(directory_refused).unwrap();
}

#[tokio::test]
async fn http_tool_placeholder_in_host_rejected() {
    for url in [
        "https://{host}/x",
        "https://api.{name}.example.com/x",
        "https://example.com:{port}/x",
        "{scheme}://example.com/x",
        "https://{user}@example.com/x",
        "https://example.com/x?{name}=value",
        "https://example.com/{unclosed",
        "https://example.com/x}",
    ] {
        let (tools, directory) = load_file(json!({"tools":[
            {"name":"fetch","description":"Fetch","kind":"http","url":url}
        ]}))
        .await;
        assert!(tools.error.is_some(), "{url}");
        std::fs::remove_dir_all(directory).unwrap();
    }
    // Placeholders in path segments and query values are accepted.
    let (tools, directory) = tool(json!({
        "name":"fetch","description":"Fetch","kind":"http",
        "url":"https://example.com/repos/{owner}/{repo}.json?ref={branch}"
    }))
    .await;
    assert_eq!(
        tools.definitions()[0]["function"]["parameters"]["required"],
        json!(["owner", "repo", "branch"])
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn http_tool_values_percent_encoded() {
    let (origin, log) = serve(ok("{}")).await;
    let (tools, directory) = tool(json!({
        "name":"lookup","description":"Look up","kind":"http",
        "url":format!("{origin}/items/{{id}}?q={{query}}")
    }))
    .await;
    let approvals = Approvals::default();
    let (emit, _) = allow_all(&approvals);
    call(
        &tools,
        &directory,
        "lookup",
        json!({"id":"../a/b?c#d","query":"x&admin=1 é"}),
        &approvals,
        &emit,
        no_secrets(),
    )
    .await
    .unwrap();
    let request = requests(&log).remove(0);
    assert!(
        request.starts_with("GET /items/..%2Fa%2Fb%3Fc%23d?q=x%26admin%3D1%20%C3%A9 HTTP/1.1"),
        "{request}"
    );
    // A whole segment of `.` or `..` would leave the configured folder.
    for id in ["..", "."] {
        assert!(matches!(
            call(
                &tools,
                &directory,
                "lookup",
                json!({"id":id,"query":"x"}),
                &approvals,
                &emit,
                no_secrets()
            )
            .await,
            Err(ToolError::Arguments)
        ));
    }
    // Values must be strings unless the owner declared other types.
    assert!(matches!(
        call(
            &tools,
            &directory,
            "lookup",
            json!({"id":5,"query":"x"}),
            &approvals,
            &emit,
            no_secrets()
        )
        .await,
        Err(ToolError::Arguments)
    ));
    assert_eq!(requests(&log).len(), 1);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn http_tool_network_rule_matches_resolved_host() {
    let (origin, log) = serve(ok("{}")).await;
    let (tools, directory) = tool(json!({
        "name":"lookup","description":"Look up","kind":"http","url":format!("{origin}/items/{{id}}")
    }))
    .await;
    let owner =
        std::env::temp_dir().join(format!("blackwall-http-policy-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&owner).unwrap();
    let never = |_: AgentEvent| panic!("an allow rule for the host answers without a prompt");
    std::fs::write(
        owner.join(crate::policy::POLICY_FILE),
        json!({"rules":[{"decision":"allow","kind":"network","host":"127.0.0.1"}]}).to_string(),
    )
    .unwrap();
    let approvals = Approvals::default();
    approvals.load_global_policy("run", &owner).await.unwrap();
    call(
        &tools,
        &directory,
        "lookup",
        json!({"id":"one"}),
        &approvals,
        &never,
        no_secrets(),
    )
    .await
    .unwrap();
    assert_eq!(requests(&log).len(), 1);
    // A deny rule for the host refuses the call before any request.
    std::fs::write(
        owner.join(crate::policy::POLICY_FILE),
        json!({"rules":[{"decision":"deny","kind":"network","host":"127.0.0.1"}]}).to_string(),
    )
    .unwrap();
    let approvals = Approvals::default();
    approvals.load_global_policy("run", &owner).await.unwrap();
    assert!(matches!(
        call(
            &tools,
            &directory,
            "lookup",
            json!({"id":"two"}),
            &approvals,
            &never,
            no_secrets()
        )
        .await,
        Err(ToolError::Denied)
    ));
    assert_eq!(requests(&log).len(), 1);
    std::fs::remove_dir_all(directory).unwrap();
    std::fs::remove_dir_all(owner).unwrap();
}

/// The workspace reqwest has no decoders, so bytes as streamed are bytes as decoded. A compressed
/// body could inflate past the cap, so it is refused outright.
#[tokio::test]
async fn http_tool_cap_counts_decompressed_bytes() {
    // 134 bytes of gzip that inflate to 100 KiB.
    let compressed: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xed, 0xc1, 0x81, 0x00, 0x00,
        0x00, 0x00, 0xc3, 0x20, 0xd6, 0xf9, 0x4b, 0x1c, 0xe4, 0x55, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x7c, 0x18, 0x2a, 0x3d, 0x9a, 0xf0, 0x00, 0x90, 0x01, 0x00,
    ];
    let (origin, log) = serve(reply("200 OK", "Content-Encoding: gzip\r\n", compressed)).await;
    let (tools, directory) = tool(json!({
        "name":"zipped","description":"Zipped","kind":"http","url":format!("{origin}/x")
    }))
    .await;
    let approvals = Approvals::default();
    let (emit, _) = allow_all(&approvals);
    let error = call(
        &tools,
        &directory,
        "zipped",
        json!({}),
        &approvals,
        &emit,
        no_secrets(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("compressed"), "{error}");
    assert!(requests(&log)[0]
        .to_ascii_lowercase()
        .contains("accept-encoding: identity"));
    // A chunked body has no length to trust; the cap counts the streamed bytes.
    let chunk = "y".repeat(16 * 1024);
    let mut chunked =
        String::from("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
    for _ in 0..5 {
        chunked.push_str(&format!("{:x}\r\n{chunk}\r\n", chunk.len()));
    }
    chunked.push_str("0\r\n\r\n");
    let (streamed, _) = serve(chunked.into_bytes()).await;
    let (tools_streamed, directory_streamed) = tool(json!({
        "name":"streamed","description":"Streamed","kind":"http","url":format!("{streamed}/x")
    }))
    .await;
    let error = call(
        &tools_streamed,
        &directory_streamed,
        "streamed",
        json!({}),
        &approvals,
        &emit,
        no_secrets(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("64 KiB"), "{error}");
    std::fs::remove_dir_all(directory).unwrap();
    std::fs::remove_dir_all(directory_streamed).unwrap();
}

#[tokio::test]
async fn http_tool_header_secret_bound_to_origin() {
    let (saved, saved_log) = serve(ok(r#"{"echo":"tok-123"}"#)).await;
    let (other, other_log) = serve(ok("{}")).await;
    let header = json!({"PRIVATE-TOKEN":"{secret:GITLAB_TOKEN}"});
    let (tools, directory) = load_file(json!({"tools":[
        {"name":"saved","description":"Saved","kind":"http","url":format!("{saved}/api"),"headers":header},
        {"name":"other","description":"Other","kind":"http","url":format!("{other}/api"),"headers":header}
    ]}))
    .await;
    assert_eq!(tools.error, None);
    let asked = Arc::new(Mutex::new(vec![]));
    let record = asked.clone();
    let saved_account = format!("{saved}|GITLAB_TOKEN");
    let only = saved_account.clone();
    let lookup: Lookup = Arc::new(move |account: &str| {
        record.lock().unwrap().push(account.to_owned());
        Ok((account == only).then(|| "tok-123".to_owned()))
    });
    let approvals = Approvals::default();
    let (emit, prompts) = allow_all(&approvals);
    let output = call(
        &tools,
        &directory,
        "saved",
        json!({}),
        &approvals,
        &emit,
        lookup.clone(),
    )
    .await
    .unwrap();
    assert!(requests(&saved_log)[0]
        .to_ascii_lowercase()
        .contains("private-token: tok-123"));
    // The value never reaches the model or the prompt.
    assert!(
        !output.contains("tok-123") && output.contains("[redacted]"),
        "{output}"
    );
    assert!(!prompts.lock().unwrap()[0].1.contains("tok-123"));
    // The same name on another origin reads that origin's account, which is empty.
    let error = call(
        &tools,
        &directory,
        "other",
        json!({}),
        &approvals,
        &emit,
        lookup,
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("bw tools secret other GITLAB_TOKEN"),
        "{error}"
    );
    assert!(requests(&other_log).is_empty());
    assert_eq!(
        *asked.lock().unwrap(),
        vec![saved_account, format!("{other}|GITLAB_TOKEN")]
    );
    // Literal header values are refused at load.
    for value in ["Bearer abc", "{secret:lower}", "Bearer {secret:TOKEN}"] {
        let (invalid, invalid_directory) = load_file(json!({"tools":[
            {"name":"x","description":"X","kind":"http","url":"https://example.com/","headers":{"authorization":value}}
        ]}))
        .await;
        assert!(invalid.error.is_some(), "{value}");
        std::fs::remove_dir_all(invalid_directory).unwrap();
    }
    std::fs::remove_dir_all(directory).unwrap();
}

const PROXY_CHILD: &str = "BLACKWALL_HTTP_TOOL_PROXY_CHILD";

/// Runs in a child process with proxy variables pointing at a listener.
#[tokio::test]
async fn http_tool_proxy_env_child() {
    let Some(origin) = std::env::var_os(PROXY_CHILD) else {
        return;
    };
    let (tools, directory) = tool(json!({
        "name":"direct","description":"Direct","kind":"http","url":format!("{}/x", origin.to_string_lossy())
    }))
    .await;
    let approvals = Approvals::default();
    let (emit, _) = allow_all(&approvals);
    let result = call(
        &tools,
        &directory,
        "direct",
        json!({}),
        &approvals,
        &emit,
        no_secrets(),
    )
    .await;
    std::fs::remove_dir_all(directory).unwrap();
    result.unwrap();
}

#[tokio::test]
async fn http_tool_ignores_proxy_env() {
    let (origin, seen) = serve(ok("{}")).await;
    let (proxy, proxied) = serve(ok("{}")).await;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "command_tools::http::tests::http_tool_proxy_env_child",
            "--test-threads=1",
        ])
        .env(PROXY_CHILD, &origin)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .stdout(std::process::Stdio::null());
    for name in [
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "ALL_PROXY",
        "https_proxy",
        "http_proxy",
        "all_proxy",
    ] {
        child.env(name, &proxy);
    }
    let status = tokio::task::spawn_blocking(move || child.status())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    assert_eq!(requests(&seen).len(), 1);
    assert!(requests(&proxied).is_empty());
}

#[tokio::test]
async fn http_tool_body_json_without_parameters_takes_only_placeholders() {
    let (origin, log) = serve(ok("{}")).await;
    let (tools, directory) = tool(json!({
        "name":"note","description":"Note","kind":"http","method":"POST","body":"json",
        "url":format!("{origin}/notes/{{id}}")
    }))
    .await;
    let approvals = Approvals::default();
    let (emit, _) = allow_all(&approvals);
    for arguments in [
        json!({"id":"a","extra":"b"}),
        json!({"id":{"nested":"a"}}),
        json!({"id":["a"]}),
        json!({}),
    ] {
        assert!(
            matches!(
                call(
                    &tools,
                    &directory,
                    "note",
                    arguments.clone(),
                    &approvals,
                    &emit,
                    no_secrets()
                )
                .await,
                Err(ToolError::Arguments)
            ),
            "{arguments}"
        );
    }
    assert!(requests(&log).is_empty());
    call(
        &tools,
        &directory,
        "note",
        json!({"id":"a"}),
        &approvals,
        &emit,
        no_secrets(),
    )
    .await
    .unwrap();
    assert!(requests(&log)[0].ends_with(r#"{"id":"a"}"#));
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn http_tool_text_call_confirms_as_network_and_still_shows_the_request() {
    let (origin, log) = serve(ok("{}")).await;
    let (tools, directory) = tool(json!({
        "name":"lookup","description":"Look up","kind":"http","url":format!("{origin}/items/{{id}}")
    }))
    .await;
    let tool = tools.get("lookup").unwrap();
    assert_eq!(tool.approval_kind(), ApprovalKind::Network);
    let approvals = Approvals::default();
    let (emit, asked) = allow_all(&approvals);
    let arguments = r#"{"id":"one"}"#;
    assert!(approvals
        .confirm_text_call(
            "run",
            tool.approval_kind(),
            false,
            "lookup",
            arguments,
            &emit
        )
        .await
        .unwrap());
    call(
        &tools,
        &directory,
        "lookup",
        json!({"id":"one"}),
        &approvals,
        &emit,
        no_secrets(),
    )
    .await
    .unwrap();
    approvals.end_text_call("run");
    let asked = asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0].0, ApprovalKind::Network);
    assert!(asked[0].1.contains("wrote this tool call as text"));
    assert_eq!(asked[1].0, ApprovalKind::Network);
    assert!(asked[1]
        .1
        .contains(&format!("Request: GET {origin}/items/one")));
    assert_eq!(requests(&log).len(), 1);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn http_tool_credentialed_request_needs_a_tool_rule() {
    let (origin, log) = serve(ok("{}")).await;
    let (tools, directory) = tool(json!({
        "name":"post_note","description":"Post","kind":"http","method":"POST","body":"json",
        "url":format!("{origin}/notes"),"headers":{"authorization":"{secret:NOTES_TOKEN}"}
    }))
    .await;
    let secret: Lookup = Arc::new(|_: &str| Ok(Some("value".into())));
    let owner =
        std::env::temp_dir().join(format!("blackwall-http-rules-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&owner).unwrap();
    let rules = |rules: Value| {
        std::fs::write(
            owner.join(crate::policy::POLICY_FILE),
            json!({"rules":rules}).to_string(),
        )
        .unwrap();
    };
    // A host allow does not authorize a credentialed POST: it still asks.
    rules(json!([{"decision":"allow","kind":"network","host":"127.0.0.1"}]));
    let approvals = Approvals::default();
    approvals.load_global_policy("run", &owner).await.unwrap();
    let (emit, asked) = allow_all(&approvals);
    call(
        &tools,
        &directory,
        "post_note",
        json!({}),
        &approvals,
        &emit,
        secret.clone(),
    )
    .await
    .unwrap();
    assert_eq!(asked.lock().unwrap().len(), 1);
    // An allow rule naming the tool does.
    rules(json!([{"decision":"allow","kind":"network","tool":"post_note"}]));
    let approvals = Approvals::default();
    approvals.load_global_policy("run", &owner).await.unwrap();
    let never = |_: AgentEvent| panic!("a tool rule answers without a prompt");
    call(
        &tools,
        &directory,
        "post_note",
        json!({}),
        &approvals,
        &never,
        secret.clone(),
    )
    .await
    .unwrap();
    // A host deny still blocks it.
    rules(json!([
        {"decision":"allow","kind":"network","tool":"post_note"},
        {"decision":"deny","kind":"network","host":"127.0.0.1"}
    ]));
    let approvals = Approvals::default();
    approvals.load_global_policy("run", &owner).await.unwrap();
    assert!(matches!(
        call(
            &tools,
            &directory,
            "post_note",
            json!({}),
            &approvals,
            &never,
            secret
        )
        .await,
        Err(ToolError::Denied)
    ));
    assert_eq!(requests(&log).len(), 2);
    // A tool rule for another tool, or an invalid tool name, never matches.
    rules(json!([{"decision":"allow","kind":"network","tool":"Bad-Name"}]));
    assert!(crate::policy::Policy::load_global(&owner)
        .await
        .error
        .is_some());
    std::fs::remove_dir_all(directory).unwrap();
    std::fs::remove_dir_all(owner).unwrap();
}

#[tokio::test]
async fn http_tool_resolved_url_is_capped() {
    let (tools, directory) = tool(json!({
        "name":"lookup","description":"Look up","kind":"http","url":"https://example.com/items/{id}"
    }))
    .await;
    let approvals = Approvals::default();
    let never = |_: AgentEvent| panic!("an oversized URL never asks");
    // 3,000 slashes encode to 9,000 bytes, over the 8 KiB limit.
    assert!(matches!(
        call(
            &tools,
            &directory,
            "lookup",
            json!({"id":"/".repeat(3000)}),
            &approvals,
            &never,
            no_secrets()
        )
        .await,
        Err(ToolError::Arguments)
    ));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn secret_query_names_match_exactly_or_by_suffix() {
    let url = Url::parse(
        "https://example.com/?monkey=1&keyboard=2&Key=3&api_key=4&private_token=5&tokens=6&sig=7",
    )
    .unwrap();
    assert_eq!(
        shown(&url),
        "https://example.com/?monkey=1&keyboard=2&Key=[redacted]&api_key=[redacted]&private_token=[redacted]&tokens=6&sig=[redacted]"
    );
}

#[tokio::test]
async fn http_tool_header_names_unique_ignoring_case() {
    let (tools, directory) = load_file(json!({"tools":[{
        "name":"x","description":"X","kind":"http","url":"https://example.com/",
        "headers":{"X-Token":"{secret:A}","x-token":"{secret:B}"}
    }]}))
    .await;
    assert!(tools.error.is_some());
    std::fs::remove_dir_all(directory).unwrap();
}
