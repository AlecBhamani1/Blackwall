//! Streamable HTTP for MCP 2025-06-18: every message is a POST whose reply is JSON or a bounded
//! event stream, inside one `Mcp-Session-Id` session.
use super::{transport::Transport, ServerConfig, MAX_MESSAGE_BYTES};
use crate::tools::ToolError;
use futures_util::future::BoxFuture;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE},
    redirect, StatusCode, Url,
};
use serde_json::Value;
use std::{collections::VecDeque, time::Duration};

const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_SESSION_BYTES: usize = 128;
const MAX_URL_BYTES: usize = 2048;
const MAX_REDIRECTS: usize = 5;
const SESSION: &str = "mcp-session-id";
const VERSION: &str = "mcp-protocol-version";
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Headers the transport sets itself.
const RESERVED: [&str; 9] = [
    "accept",
    "connection",
    "content-length",
    "content-type",
    "host",
    "mcp-protocol-version",
    "mcp-session-id",
    "te",
    "transfer-encoding",
];

fn failed(text: &str) -> ToolError {
    ToolError::Execution(text.into())
}

/// https anywhere, plain http only to a literal loopback host, and never credentials in the URL.
pub(super) fn valid_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let host = parsed.host_str().unwrap_or_default();
    url.len() <= MAX_URL_BYTES
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && match parsed.scheme() {
            "https" => !host.is_empty(),
            "http" => ["127.0.0.1", "[::1]", "localhost"].contains(&host),
            _ => false,
        }
}

/// The secret named by a `{secret:NAME}` header value.
fn secret_reference(value: &str) -> Option<&str> {
    let name = value.strip_prefix("{secret:")?.strip_suffix('}')?;
    ((1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
    .then_some(name)
}

/// A header the owner may set: a token name the transport does not own, and a secret reference
/// that can be saved for the server.
pub(super) fn valid_header(name: &str, value: &str) -> bool {
    (1..=64).contains(&name.len())
        && HeaderName::from_bytes(name.as_bytes()).is_ok()
        && !RESERVED.contains(&name.to_ascii_lowercase().as_str())
        && secret_reference(value).is_some_and(super::valid_secret_name)
}

/// Resolve a server's headers from its secrets, read from the credential store.
pub(super) fn headers(
    config: &ServerConfig,
    secrets: &[(String, String)],
) -> Result<HeaderMap, ToolError> {
    let mut headers = HeaderMap::new();
    for (name, reference) in &config.headers {
        let key = secret_reference(reference).unwrap_or_default();
        let value = secrets
            .iter()
            .find(|(saved, _)| saved == key)
            .map(|(_, value)| value)
            .ok_or_else(|| {
                ToolError::Execution(format!(
                    "{key} is not saved. Add it with bw mcp secret {} {key}.",
                    config.name
                ))
            })?;
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| failed("An MCP server header name is not valid."))?;
        let mut value = HeaderValue::from_str(value).map_err(|_| {
            ToolError::Execution(format!("{key} cannot be sent in an HTTP header."))
        })?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    Ok(headers)
}

/// A session id is 1 to 128 visible ASCII characters.
fn valid_session(id: &[u8]) -> bool {
    (1..=MAX_SESSION_BYTES).contains(&id.len())
        && id.iter().all(|byte| (0x21..=0x7e).contains(byte))
}

/// Follow redirects only within the server's origin, so headers never reach another host.
fn same_origin(attempt: redirect::Attempt) -> redirect::Action {
    let first = attempt.previous().first().map(Url::origin);
    if attempt.previous().len() <= MAX_REDIRECTS && first == Some(attempt.url().origin()) {
        attempt.follow()
    } else {
        attempt.stop()
    }
}

/// Server-sent events from one response, bounded in total and per line.
#[derive(Default)]
struct EventParser {
    line: Vec<u8>,
    data: Vec<u8>,
    total: usize,
}

impl EventParser {
    fn feed(&mut self, chunk: &[u8], out: &mut VecDeque<Value>) -> Result<(), ToolError> {
        self.total += chunk.len();
        if self.total > MAX_MESSAGE_BYTES {
            return Err(failed("The MCP server's reply exceeded 1 MiB."));
        }
        for &byte in chunk {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.field(&line, out);
            } else if self.line.len() == MAX_LINE_BYTES {
                return Err(failed("The MCP server sent an event line over 64 KiB."));
            } else {
                self.line.push(byte);
            }
        }
        Ok(())
    }

    fn field(&mut self, line: &[u8], out: &mut VecDeque<Value>) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return self.dispatch(out);
        }
        // Only data carries messages; event names, ids, retry hints and comments are ignored.
        if let Some(value) = line.strip_prefix(b"data:") {
            if !self.data.is_empty() {
                self.data.push(b'\n');
            }
            self.data
                .extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
        }
    }

    fn dispatch(&mut self, out: &mut VecDeque<Value>) {
        let data = std::mem::take(&mut self.data);
        if let Ok(message) = serde_json::from_slice::<Value>(&data) {
            if message.is_object() {
                out.push_back(message);
            }
        }
    }

    fn finish(&mut self, out: &mut VecDeque<Value>) {
        let line = std::mem::take(&mut self.line);
        self.field(&line, out);
        self.dispatch(out);
    }
}

struct EventStream {
    response: reqwest::Response,
    parser: EventParser,
}

pub(super) struct HttpTransport {
    client: reqwest::Client,
    url: Url,
    headers: HeaderMap,
    session: Option<HeaderValue>,
    version: Option<HeaderValue>,
    pending: VecDeque<Value>,
    streams: VecDeque<EventStream>,
}

impl HttpTransport {
    pub(super) fn new(url: &str, headers: HeaderMap) -> Result<Self, ToolError> {
        let url = Url::parse(url)
            .ok()
            .filter(|_| valid_url(url))
            .ok_or_else(|| failed("The MCP server URL is not allowed."))?;
        // Like the model and web clients, system proxy settings are never used.
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(redirect::Policy::custom(same_origin))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| failed("The MCP connection could not be prepared."))?;
        Ok(Self {
            client,
            url,
            headers,
            session: None,
            version: None,
            pending: VecDeque::new(),
            streams: VecDeque::new(),
        })
    }

    fn session_headers(&self) -> HeaderMap {
        let mut headers = self.headers.clone();
        if let Some(session) = &self.session {
            headers.insert(SESSION, session.clone());
        }
        if let Some(version) = &self.version {
            headers.insert(VERSION, version.clone());
        }
        headers
    }

    fn keep_session(&mut self, response: &reqwest::Response) -> Result<(), ToolError> {
        let Some(session) = response.headers().get(SESSION) else {
            return Ok(());
        };
        if !valid_session(session.as_bytes()) {
            return Err(failed("The MCP server sent an invalid session id."));
        }
        if self.session.is_none() {
            self.session = Some(session.clone());
        }
        Ok(())
    }

    async fn post(&mut self, message: &Value) -> Result<(), ToolError> {
        let body = serde_json::to_vec(message).map_err(|_| ToolError::Arguments)?;
        let response = self
            .client
            .post(self.url.clone())
            .headers(self.session_headers())
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|_| failed("The MCP server could not be reached."))?;
        self.keep_session(&response)?;
        let status = response.status();
        if status.is_redirection() {
            return Err(failed(
                "The MCP server redirected to another address, which Blackwall does not follow.",
            ));
        }
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(ToolError::Execution(format!(
                "The MCP server refused the request (HTTP {}). Check its headers and secrets; OAuth sign-in is not supported yet.",
                status.as_u16()
            )));
        }
        if !status.is_success() {
            return Err(ToolError::Execution(format!(
                "The MCP server answered with HTTP {}.",
                status.as_u16()
            )));
        }
        let kind = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|kind| kind.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if kind.starts_with("text/event-stream") {
            self.streams.push_back(EventStream {
                response,
                parser: EventParser::default(),
            });
        } else if kind.starts_with("application/json") {
            let body = read_bounded(response).await?;
            match serde_json::from_slice::<Value>(&body) {
                Ok(Value::Array(messages)) => self
                    .pending
                    .extend(messages.into_iter().filter(Value::is_object)),
                Ok(message) if message.is_object() => self.pending.push_back(message),
                _ => return Err(failed("The MCP server sent a reply that is not JSON-RPC.")),
            }
        }
        // 202 Accepted, for notifications and replies, carries nothing.
        Ok(())
    }
}

async fn read_bounded(mut response: reqwest::Response) -> Result<Vec<u8>, ToolError> {
    let mut body = vec![];
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| failed("The MCP server stopped responding."))?
    {
        if body.len() + chunk.len() > MAX_MESSAGE_BYTES {
            return Err(failed("The MCP server's reply exceeded 1 MiB."));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

impl Transport for HttpTransport {
    fn send<'a>(&'a mut self, message: &'a Value) -> BoxFuture<'a, Result<(), ToolError>> {
        Box::pin(self.post(message))
    }

    fn recv(&mut self) -> BoxFuture<'_, Result<Value, ToolError>> {
        Box::pin(async move {
            loop {
                if let Some(message) = self.pending.pop_front() {
                    return Ok(message);
                }
                let Some(stream) = self.streams.front_mut() else {
                    return Err(failed("The MCP server stopped responding."));
                };
                match stream.response.chunk().await {
                    Ok(Some(chunk)) => stream.parser.feed(&chunk, &mut self.pending)?,
                    Ok(None) => {
                        stream.parser.finish(&mut self.pending);
                        self.streams.pop_front();
                    }
                    Err(_) => return Err(failed("The MCP server stopped responding.")),
                }
            }
        })
    }

    fn negotiated(&mut self, version: &str) {
        self.version = HeaderValue::from_str(version).ok();
    }

    fn abandon(&mut self) {
        self.streams.clear();
        self.pending.clear();
    }
}

impl Drop for HttpTransport {
    /// End the session with DELETE. A server that does not support it answers 405, which is fine.
    fn drop(&mut self) {
        if self.session.is_none() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let request = self
            .client
            .delete(self.url.clone())
            .headers(self.session_headers());
        runtime.spawn(async move {
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, request.send()).await;
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::{
        parse, read_config, validate, validate_for, Connection, McpServers, Source, MCP_FILE,
        PROTOCOL_VERSION,
    };
    use super::*;
    use crate::{
        approvals::Approvals,
        protocol::{AgentEvent, ApprovalDecision, ResolveApprovalRequest},
        test_http::{response, serve_raw, Seen},
    };
    use serde_json::json;
    use std::{collections::BTreeMap, sync::Mutex};

    const SESSION_ID: &str = "session-1";
    const PROXY_CHILD: &str = "BLACKWALL_MCP_PROXY_CHILD";

    fn json_reply(id: &Value, result: Value) -> String {
        let body = json!({"jsonrpc":"2.0","id":id,"result":result}).to_string();
        response(200, &[("Content-Type", "application/json")], &body)
    }

    fn event_reply(events: &[Value]) -> String {
        let body: String = events
            .iter()
            .map(|event| format!("event: message\r\nid: 1\r\ndata: {event}\r\n\r\n"))
            .collect();
        response(200, &[("Content-Type", "text/event-stream")], &body)
    }

    fn result_of(request: &Value, method: &str) -> Option<Value> {
        Some(match method {
            "initialize" => json!({"protocolVersion":PROTOCOL_VERSION,"capabilities":{}}),
            "tools/list" => json!({"tools":[
                {"name":"search","description":"Search docs","inputSchema":{"type":"object"}},
                {"name":"slow"}
            ]}),
            "tools/call" if request["params"]["name"] == "slow" => return None,
            "tools/call" => {
                json!({"content":[{"type":"text","text":format!("found {}", request["params"]["arguments"])}]})
            }
            _ => return None,
        })
    }

    /// A streamable HTTP MCP server at `/mcp` answering with JSON, or with events when
    /// `events` is set. It starts a session and accepts DELETE with 405.
    fn mcp(events: bool) -> impl Fn(&Seen) -> Option<String> + Send + Sync + 'static {
        move |seen| {
            if seen.method == "DELETE" {
                return Some(response(405, &[], ""));
            }
            let request: Value = serde_json::from_str(&seen.body).unwrap_or_default();
            let Some(method) = request["method"].as_str() else {
                return Some(response(202, &[], ""));
            };
            if request.get("id").is_none() {
                return Some(response(202, &[], ""));
            }
            let result = result_of(&request, method)?;
            let mut reply = if events {
                event_reply(&[json!({"jsonrpc":"2.0","id":request["id"],"result":result})])
            } else {
                json_reply(&request["id"], result)
            };
            if method == "initialize" {
                reply = reply.replacen("\r\n", &format!("\r\nMcp-Session-Id: {SESSION_ID}\r\n"), 1);
            }
            Some(reply)
        }
    }

    fn methods(log: &Mutex<Vec<Seen>>) -> Vec<String> {
        log.lock()
            .unwrap()
            .iter()
            .map(|seen| {
                let body: Value = serde_json::from_str(&seen.body).unwrap_or_default();
                let method = body["method"].as_str().unwrap_or("reply");
                format!("{} {method}", seen.method)
            })
            .collect()
    }

    fn folder(name: &str) -> std::path::PathBuf {
        let directory =
            std::env::temp_dir().join(format!("blackwall-mcp-{name}-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn url_server(name: &str, url: &str) -> ServerConfig {
        ServerConfig {
            name: name.into(),
            command: vec![],
            url: Some(url.into()),
            headers: BTreeMap::new(),
            enabled: true,
            allow_tools: None,
            deny_tools: vec![],
            secrets: vec![],
        }
    }

    async fn connect(url: &str) -> Result<Connection, ToolError> {
        let transport = HttpTransport::new(url, HeaderMap::new())?;
        let mut connection = Connection::new(Box::new(transport), vec![]);
        connection.initialize().await?;
        Ok(connection)
    }

    fn allow(approvals: &Approvals) -> impl Fn(AgentEvent) {
        let approvals = approvals.clone();
        move |event| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } = event
            {
                approvals
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision: ApprovalDecision::Allow,
                    })
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn http_initialize_and_list_tools_json_response() {
        let (origin, log) = serve_raw(mcp(false)).await;
        let directory = folder("http");
        let file = json!({"servers":[url_server("docs", &format!("{origin}/mcp"))]});
        std::fs::write(directory.join(MCP_FILE), file.to_string()).unwrap();
        let servers = McpServers::start(&directory, &[]).await;
        assert!(servers.warnings.is_empty(), "{:?}", servers.warnings);
        let names: Vec<_> = servers
            .definitions()
            .iter()
            .map(|definition| definition["function"]["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["docs__search", "docs__slow"]);
        let approvals = Approvals::default();
        let found = servers
            .call(
                "docs__search",
                r#"{"q":"x"}"#,
                "run",
                &approvals,
                &allow(&approvals),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(found, r#"found {"q":"x"}"#);
        assert_eq!(
            methods(&log),
            [
                "POST initialize",
                "POST notifications/initialized",
                "POST tools/list",
                "POST tools/call"
            ]
        );
        let first = &log.lock().unwrap()[0];
        assert_eq!(first.path, "/mcp");
        assert_eq!(
            first.header("accept"),
            Some("application/json, text/event-stream")
        );
        assert_eq!(first.header("content-type"), Some("application/json"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn http_sse_response_is_parsed() {
        let (origin, _) = serve_raw(mcp(true)).await;
        let mut connection = connect(&format!("{origin}/mcp")).await.unwrap();
        let tools = connection.list_tools().await.unwrap();
        assert_eq!(tools[0]["name"], "search");
    }

    #[tokio::test]
    async fn http_session_id_is_kept_and_deleted_on_close() {
        let (origin, log) = serve_raw(mcp(false)).await;
        let mut connection = connect(&format!("{origin}/mcp")).await.unwrap();
        connection.list_tools().await.unwrap();
        drop(connection);
        for _ in 0..100 {
            if log.lock().unwrap().len() == 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let seen = log.lock().unwrap().clone();
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[0].header(SESSION), None);
        assert_eq!(seen[0].header(VERSION), None);
        for later in &seen[1..] {
            assert_eq!(later.header(SESSION), Some(SESSION_ID));
            assert_eq!(later.header(VERSION), Some(PROTOCOL_VERSION));
        }
        assert_eq!(seen[3].method, "DELETE");
    }

    #[test]
    fn http_requires_https_except_loopback() {
        for allowed in [
            "https://mcp.example.com/mcp",
            "http://127.0.0.1:8080/mcp",
            "http://[::1]:8080/mcp",
            "http://localhost/mcp",
        ] {
            assert!(
                validate(vec![url_server("a", allowed)]).is_ok(),
                "{allowed}"
            );
        }
        for refused in [
            "http://mcp.example.com/mcp",
            "http://192.168.1.2/mcp",
            "http://localhost.example.com/mcp",
            "http://127.0.0.2/mcp",
            "https://user:pass@mcp.example.com/mcp",
            "ftp://mcp.example.com/",
            "mcp.example.com",
        ] {
            assert!(
                validate(vec![url_server("a", refused)]).is_err(),
                "{refused}"
            );
        }
        let mut both = url_server("a", "https://mcp.example.com/mcp");
        both.command = vec!["server".into()];
        assert!(validate(vec![both]).is_err());
        let mut neither = url_server("a", "https://mcp.example.com/mcp");
        neither.url = None;
        assert!(validate(vec![neither]).is_err());
    }

    #[tokio::test]
    async fn http_cross_origin_redirect_refused() {
        let (elsewhere, other) = serve_raw(mcp(false)).await;
        let target = format!("{elsewhere}/mcp");
        let (origin, _) =
            serve_raw(move |_| Some(response(307, &[("Location", target.as_str())], ""))).await;
        let refused = connect(&format!("{origin}/mcp")).await;
        assert!(
            matches!(&refused, Err(ToolError::Execution(message)) if message.contains("redirected")),
            "{:?}",
            refused.err()
        );
        assert!(other.lock().unwrap().is_empty());
        // A redirect within the origin is followed.
        let (origin, _) = serve_raw(|seen| {
            if seen.path == "/old" {
                Some(response(307, &[("Location", "/mcp")], ""))
            } else {
                mcp(false)(seen)
            }
        })
        .await;
        assert!(connect(&format!("{origin}/old")).await.is_ok());
    }

    #[test]
    fn http_header_literal_rejected() {
        let server = |name: &str, value: &str| {
            let mut server = url_server("a", "https://mcp.example.com/mcp");
            server.headers.insert(name.into(), value.into());
            server.secrets = vec!["API_TOKEN".into()];
            server
        };
        assert!(validate(vec![server("Authorization", "{secret:API_TOKEN}")]).is_ok());
        for (name, value) in [
            ("Authorization", "Bearer abc123"),
            ("Authorization", "Bearer {secret:API_TOKEN}"),
            ("Authorization", "{secret:}"),
            ("Authorization", "{secret:api-token}"),
            (
                "X-Key",
                "{secret:A_VERY_LONG_NAME_THAT_GOES_PAST_THE_LIMIT_OF_SIXTY_FOUR_CHARACTERS_X}",
            ),
            ("Mcp-Session-Id", "{secret:API_TOKEN}"),
            ("Host", "{secret:API_TOKEN}"),
            ("Bad Header", "{secret:API_TOKEN}"),
        ] {
            assert!(
                validate(vec![server(name, value)]).is_err(),
                "{name}: {value}"
            );
        }
        // Headers belong to URL servers only.
        let mut local = server("Authorization", "{secret:API_TOKEN}");
        local.url = None;
        local.command = vec!["server".into()];
        assert!(validate(vec![local]).is_err());
    }

    #[tokio::test]
    async fn http_header_from_secret_is_sent_and_redacted() {
        let token = "token-value-123";
        let (origin, log) = serve_raw(move |seen| {
            let request: Value = serde_json::from_str(&seen.body).unwrap_or_default();
            match request["method"].as_str() {
                Some("tools/call") => Some(json_reply(
                    &request["id"],
                    json!({"content":[{"type":"text","text":format!("echo {token}")}]}),
                )),
                _ => mcp(false)(seen),
            }
        })
        .await;
        let mut config = url_server("docs", &format!("{origin}/mcp"));
        config
            .headers
            .insert("Authorization".into(), "{secret:API_TOKEN}".into());
        config.secrets = vec!["API_TOKEN".into()];
        let missing = super::headers(&config, &[]).unwrap_err();
        assert!(
            matches!(&missing, ToolError::Execution(message) if message.contains("bw mcp secret docs API_TOKEN"))
        );
        let environment = vec![("API_TOKEN".to_owned(), token.to_owned())];
        let connection = super::super::launch(&config, &std::env::temp_dir(), environment)
            .await
            .unwrap();
        let mut servers = McpServers::default();
        servers.add(config, connection, &mut vec![]).await;
        let approvals = Approvals::default();
        let found = servers
            .call(
                "docs__search",
                "{}",
                "run",
                &approvals,
                &allow(&approvals),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(found, "echo [redacted]");
        assert_eq!(log.lock().unwrap()[0].header("authorization"), Some(token));
    }

    #[tokio::test]
    async fn http_response_over_1mib_is_error() {
        let (origin, _) = serve_raw(|seen| {
            let request: Value = serde_json::from_str(&seen.body).unwrap_or_default();
            let padding = "x".repeat(MAX_MESSAGE_BYTES);
            Some(json_reply(&request["id"], json!({ "padding": padding })))
        })
        .await;
        let refused = connect(&format!("{origin}/mcp")).await;
        assert!(
            matches!(&refused, Err(ToolError::Execution(message)) if message.contains("1 MiB")),
            "{:?}",
            refused.err()
        );
    }

    #[test]
    fn sse_total_and_line_caps() {
        let mut out = VecDeque::new();
        let mut parser = EventParser::default();
        parser
            .feed(
                b": comment\r\nevent: message\ndata: {\"a\":\ndata: 1}\n\n",
                &mut out,
            )
            .unwrap();
        parser
            .feed(b"data: [1]\n\ndata: {\"b\":2}", &mut out)
            .unwrap();
        parser.finish(&mut out);
        assert_eq!(out, [json!({"a":1}), json!({"b":2})]);

        let mut parser = EventParser::default();
        let line = vec![b'x'; MAX_LINE_BYTES];
        parser.feed(&line, &mut out).unwrap();
        assert!(parser.feed(b"x", &mut out).is_err());

        let mut parser = EventParser::default();
        let mut short = vec![b'x'; 1000];
        short.push(b'\n');
        for _ in 0..MAX_MESSAGE_BYTES / short.len() {
            parser.feed(&short, &mut out).unwrap();
        }
        assert!(parser.feed(&short, &mut out).is_err());
    }

    #[tokio::test]
    async fn session_id_bounds() {
        assert!(valid_session(b"a"));
        assert!(valid_session(&[b'~'; MAX_SESSION_BYTES]));
        assert!(!valid_session(b""));
        assert!(!valid_session(&[b'a'; MAX_SESSION_BYTES + 1]));
        assert!(!valid_session(b"has space"));
        assert!(!valid_session("caf\u{e9}".as_bytes()));
        let long = "a".repeat(MAX_SESSION_BYTES + 1);
        let (origin, _) = serve_raw(move |seen| {
            let request: Value = serde_json::from_str(&seen.body).unwrap_or_default();
            let reply = json_reply(&request["id"], result_of(&request, "initialize")?);
            Some(reply.replacen("\r\n", &format!("\r\nMcp-Session-Id: {long}\r\n"), 1))
        })
        .await;
        let refused = connect(&format!("{origin}/mcp")).await;
        assert!(
            matches!(&refused, Err(ToolError::Execution(message)) if message.contains("session id"))
        );
    }

    #[tokio::test]
    async fn server_requests_get_method_not_found() {
        let (origin, log) = serve_raw(|seen| {
            let request: Value = serde_json::from_str(&seen.body).unwrap_or_default();
            if request["method"] == "tools/list" {
                let ask = json!({"jsonrpc":"2.0","id":"s1","method":"sampling/createMessage","params":{}});
                let ping = json!({"jsonrpc":"2.0","id":"s2","method":"ping"});
                let done = json!({"jsonrpc":"2.0","id":request["id"],"result":{"tools":[]}});
                return Some(event_reply(&[ask, ping, done]));
            }
            mcp(false)(seen)
        })
        .await;
        let mut connection = connect(&format!("{origin}/mcp")).await.unwrap();
        assert!(connection.list_tools().await.unwrap().is_empty());
        let replies: Vec<Value> = log
            .lock()
            .unwrap()
            .iter()
            .filter_map(|seen| serde_json::from_str::<Value>(&seen.body).ok())
            .filter(|body| body.get("method").is_none())
            .collect();
        assert_eq!(replies[0]["id"], "s1");
        assert_eq!(replies[0]["error"]["code"], -32601);
        assert_eq!(replies[1]["id"], "s2");
        assert_eq!(replies[1]["result"], json!({}));
    }

    #[tokio::test]
    async fn http_timeout_marks_broken() {
        let (origin, log) = serve_raw(mcp(false)).await;
        let connection = connect(&format!("{origin}/mcp")).await.unwrap();
        let mut servers = McpServers::default();
        let config = url_server("docs", &format!("{origin}/mcp"));
        servers.add(config, connection, &mut vec![]).await;
        let approvals = Approvals::default();
        let allow = allow(&approvals);
        let slow = servers
            .call(
                "docs__slow",
                "{}",
                "run",
                &approvals,
                &allow,
                Duration::from_millis(100),
            )
            .await;
        assert!(
            matches!(slow, Err(ToolError::Execution(message)) if message.contains("time limit"))
        );
        let sent = log.lock().unwrap().len();
        let after = servers
            .call(
                "docs__search",
                "{}",
                "run",
                &approvals,
                &allow,
                Duration::from_secs(5),
            )
            .await;
        assert!(after.is_err());
        assert_eq!(log.lock().unwrap().len(), sent);
    }

    #[test]
    fn owner_name_with_p_prefix_rejected() {
        let refused = validate(vec![url_server("p-docs", "https://mcp.example.com/mcp")]);
        assert!(refused.unwrap_err().contains("reserved"));
        assert!(validate(vec![url_server("docs-p-1", "https://mcp.example.com/mcp")]).is_ok());
    }

    #[test]
    fn project_server_with_secrets_rejected() {
        let mut server = url_server("p-docs", "https://mcp.example.com/mcp");
        assert!(validate_for(vec![server.clone()], Source::Project).is_ok());
        assert!(validate_for(
            vec![url_server("docs", "https://mcp.example.com/mcp")],
            Source::Project
        )
        .is_err());
        server.secrets = vec!["API_TOKEN".into()];
        assert!(validate_for(vec![server.clone()], Source::Project).is_err());
        server
            .headers
            .insert("Authorization".into(), "{secret:API_TOKEN}".into());
        assert!(validate_for(vec![server.clone()], Source::Project).is_err());
        server.secrets.clear();
        assert!(validate_for(vec![server], Source::Project).is_err());
    }

    #[test]
    fn project_command_server_rejected() {
        let mut server = url_server("p-docs", "https://mcp.example.com/mcp");
        server.url = None;
        server.command = vec!["server".into()];
        assert!(validate_for(vec![server], Source::Project)
            .unwrap_err()
            .contains("by URL"));
    }

    #[test]
    fn legacy_mcp_json_loads_unchanged() {
        let legacy = br#"{"servers":[{"name":"jira","command":["jira-mcp","--read-only"],"enabled":false,"allowTools":["search"],"denyTools":["delete"],"secrets":["JIRA_API_TOKEN"]}]}"#;
        let servers = parse(legacy).unwrap();
        assert_eq!(servers[0].command, ["jira-mcp", "--read-only"]);
        assert!(servers[0].url.is_none() && servers[0].headers.is_empty());
        let directory = folder("legacy");
        std::fs::write(directory.join(MCP_FILE), legacy).unwrap();
        super::super::write_config(&directory, read_config(&directory).unwrap()).unwrap();
        let saved: Value =
            serde_json::from_slice(&std::fs::read(directory.join(MCP_FILE)).unwrap()).unwrap();
        assert_eq!(saved, serde_json::from_slice::<Value>(legacy).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Runs only as the child of `http_ignores_proxy_env`, with proxy variables set.
    #[tokio::test]
    async fn proxy_env_child() {
        let Some(url) = std::env::var_os(PROXY_CHILD) else {
            return;
        };
        connect(&url.to_string_lossy()).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http_ignores_proxy_env() {
        let (origin, seen) = serve_raw(mcp(false)).await;
        let (proxy, proxied) = serve_raw(mcp(false)).await;
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "mcp::http::tests::proxy_env_child",
                "--test-threads=1",
            ])
            .env(PROXY_CHILD, format!("{origin}/mcp"))
            .env("HTTPS_PROXY", &proxy)
            .env("HTTP_PROXY", &proxy)
            .env("ALL_PROXY", &proxy)
            .env("https_proxy", &proxy)
            .env("http_proxy", &proxy)
            .env("all_proxy", &proxy)
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
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
        assert!(!seen.lock().unwrap().is_empty());
        assert!(proxied.lock().unwrap().is_empty());
    }

    #[test]
    fn added_url_servers_save_secret_references() {
        let directory = folder("add");
        let name = super::super::add_http_server(
            &directory,
            "Docs",
            "https://mcp.example.com/mcp",
            vec![("Authorization".into(), "API_TOKEN".into())],
        )
        .unwrap();
        assert_eq!(name, "docs");
        let saved = read_config(&directory).unwrap();
        assert_eq!(saved[0].headers["Authorization"], "{secret:API_TOKEN}");
        assert!(super::super::add_http_server(
            &directory,
            "plain",
            "http://mcp.example.com/mcp",
            vec![]
        )
        .is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
