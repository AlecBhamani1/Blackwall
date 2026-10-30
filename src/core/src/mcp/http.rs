//! Streamable HTTP for MCP 2025-06-18: every message is a POST whose reply is JSON or a bounded
//! event stream, inside one `Mcp-Session-Id` session.
use super::{transport::Transport, MAX_MESSAGE_BYTES};
use crate::tools::ToolError;
use futures_util::future::BoxFuture;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE},
    redirect, StatusCode, Url,
};
use serde_json::Value;
use std::{
    collections::VecDeque,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

const MAX_SESSION_BYTES: usize = 128;
const MAX_URL_BYTES: usize = 2048;
const MAX_REDIRECTS: usize = 5;
/// Messages read from one reply but not yet handled.
const MAX_PENDING: usize = 256;
const SESSION: &str = "mcp-session-id";
const VERSION: &str = "mcp-protocol-version";
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Headers the transport or HTTP itself owns, and credentials meant for a proxy or a browser.
const RESERVED: [&str; 15] = [
    "accept",
    "connection",
    "content-length",
    "content-type",
    "cookie",
    "expect",
    "host",
    "keep-alive",
    "mcp-protocol-version",
    "mcp-session-id",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn failed(text: &str) -> ToolError {
    ToolError::Execution(text.into())
}

fn stopped() -> ToolError {
    failed("The MCP server stopped responding.")
}

/// https anywhere, plain http only to literal loopback addresses, never credentials in the URL.
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
            "http" => ["127.0.0.1", "[::1]"].contains(&host),
            _ => false,
        }
}

/// A host name and the checked addresses a connection to it must use.
pub(super) type Pinned = (String, Vec<SocketAddr>);

/// Addresses a project's server may not use: loopback, private, link-local, shared, multicast
/// and unspecified ranges, including IPv4 addresses written as IPv6.
fn local_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [first, second, ..] = ip.octets();
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || first == 0
                || (first == 100 && (64..128).contains(&second))
        }
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || first & 0xfe00 == 0xfc00
                || first & 0xffc0 == 0xfe80
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|ip| local_address(IpAddr::V4(ip)))
        }
    }
}

/// Check that a project server's host is public. A name is resolved once, every address must be
/// public, and the result is pinned so the connection cannot be pointed elsewhere afterwards.
pub(super) async fn public_addresses(url: &str) -> Result<Option<Pinned>, ToolError> {
    let refused = || {
        failed("Its address is on this computer or a private network, which a project cannot use.")
    };
    let url = Url::parse(url).map_err(|_| failed("The MCP server URL is not allowed."))?;
    let host = url.host_str().unwrap_or_default();
    let port = url.port_or_known_default().unwrap_or(443);
    if let Ok(ip) = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
    {
        return if local_address(ip) {
            Err(refused())
        } else {
            Ok(None)
        };
    }
    let lookup = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| failed("Its address could not be looked up in time."))?
    .map_err(|_| failed("Its address could not be looked up."))?;
    let addresses: Vec<SocketAddr> = lookup.collect();
    if addresses.is_empty() {
        return Err(failed("Its address could not be looked up."));
    }
    if addresses.iter().any(|address| local_address(address.ip())) {
        return Err(refused());
    }
    Ok(Some((host.to_owned(), addresses)))
}

/// `scheme://host[:port]`, which header secrets are bound to.
pub(super) fn origin(url: &str) -> Option<String> {
    Url::parse(url)
        .ok()
        .map(|url| url.origin().ascii_serialization())
}

/// The secret named by a `{secret:NAME}` header value.
pub(super) fn secret_reference(value: &str) -> Option<&str> {
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

/// Build headers from `(header name, value)` pairs read for the server's current origin.
pub(super) fn headers(values: &[(String, String)]) -> Result<HeaderMap, ToolError> {
    let mut headers = HeaderMap::new();
    for (name, value) in values {
        let mut value = HeaderValue::from_str(value).map_err(|_| {
            ToolError::Execution(format!(
                "The {name} secret cannot be sent in an HTTP header."
            ))
        })?;
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| failed("An MCP server header name is not valid."))?;
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

/// Follow only 307 and 308, which keep the POST, and only within the server's origin, so headers
/// never reach another host. Other redirects come back as errors.
fn same_origin(attempt: redirect::Attempt) -> redirect::Action {
    let first = attempt.previous().first().map(Url::origin);
    let keeps_post = matches!(
        attempt.status(),
        StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT
    );
    if keeps_post
        && attempt.previous().len() <= MAX_REDIRECTS
        && first == Some(attempt.url().origin())
    {
        attempt.follow()
    } else {
        attempt.stop()
    }
}

/// Queue a JSON-RPC message or batch, keeping only objects.
fn queue(message: Value, out: &mut VecDeque<Value>) -> bool {
    match message {
        Value::Array(messages) => {
            out.extend(messages.into_iter().filter(Value::is_object));
            true
        }
        message if message.is_object() => {
            out.push_back(message);
            true
        }
        _ => false,
    }
}

/// Server-sent events from one response, bounded in total, which also bounds each line.
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
            queue(message, out);
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
    /// Set when the server forgot the session, so the connection can initialize again.
    expired: bool,
    pending: VecDeque<Value>,
    /// The event stream of the request in flight. Only requests keep a reply stream open, and
    /// each new request replaces it, so there is never more than one.
    stream: Option<EventStream>,
}

impl HttpTransport {
    pub(super) fn new(
        url: &str,
        headers: HeaderMap,
        pinned: Option<Pinned>,
    ) -> Result<Self, ToolError> {
        let url = Url::parse(url)
            .ok()
            .filter(|_| valid_url(url))
            .ok_or_else(|| failed("The MCP server URL is not allowed."))?;
        // Like the model and web clients, system proxy settings are never used.
        let mut client = reqwest::Client::builder()
            .no_proxy()
            .redirect(redirect::Policy::custom(same_origin))
            .connect_timeout(Duration::from_secs(10));
        if let Some((host, addresses)) = &pinned {
            client = client.resolve_to_addrs(host, addresses);
        }
        let client = client
            .build()
            .map_err(|_| failed("The MCP connection could not be prepared."))?;
        Ok(Self {
            client,
            url,
            headers,
            session: None,
            version: None,
            expired: false,
            pending: VecDeque::new(),
            stream: None,
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
        let request = message.get("method").is_some() && message.get("id").is_some();
        if request {
            // Anything left from an earlier request is stale and must not block this one.
            self.stream = None;
            self.pending.clear();
        }
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
        if status == StatusCode::NOT_FOUND && self.session.is_some() {
            self.session = None;
            self.version = None;
            self.expired = true;
            return Err(failed("The MCP server ended the session."));
        }
        if status.is_redirection() {
            return Err(failed(
                "The MCP server redirected to another address or with a redirect that drops the request, which Blackwall does not follow.",
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
        // Notifications and replies expect 202 Accepted. Whatever else comes back is closed
        // unread, so it can never pile up.
        if !request || status == StatusCode::ACCEPTED {
            return Ok(());
        }
        let kind = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|kind| kind.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if kind.starts_with("text/event-stream") {
            self.stream = Some(EventStream {
                response,
                parser: EventParser::default(),
            });
            return Ok(());
        }
        let body = read_bounded(response).await?;
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let queued = serde_json::from_slice::<Value>(&body)
            .is_ok_and(|message| queue(message, &mut self.pending));
        if !queued {
            return Err(failed("The MCP server sent a reply that is not JSON-RPC."));
        }
        self.bounded()
    }

    fn bounded(&self) -> Result<(), ToolError> {
        if self.pending.len() > MAX_PENDING {
            return Err(failed("The MCP server sent too many messages at once."));
        }
        Ok(())
    }
}

async fn read_bounded(mut response: reqwest::Response) -> Result<Vec<u8>, ToolError> {
    let mut body = vec![];
    while let Some(chunk) = response.chunk().await.map_err(|_| stopped())? {
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
                let Some(stream) = self.stream.as_mut() else {
                    return Err(stopped());
                };
                match stream.response.chunk().await {
                    Ok(Some(chunk)) => {
                        stream.parser.feed(&chunk, &mut self.pending)?;
                        self.bounded()?;
                    }
                    Ok(None) => {
                        stream.parser.finish(&mut self.pending);
                        self.stream = None;
                        self.bounded()?;
                    }
                    Err(_) => return Err(stopped()),
                }
            }
        })
    }

    fn negotiated(&mut self, version: &str) {
        self.version = HeaderValue::from_str(version).ok();
    }

    fn expired(&mut self) -> bool {
        std::mem::take(&mut self.expired)
    }

    fn abandon(&mut self) {
        self.stream = None;
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
        header_accounts, parse, read_config, validate, validate_for, write_config, Connection,
        McpServers, ServerConfig, Source, MAX_SERVER_REQUESTS, MCP_FILE, PROTOCOL_VERSION,
    };
    use super::*;
    use crate::{
        approvals::Approvals,
        protocol::{AgentEvent, ApprovalDecision, ResolveApprovalRequest},
        test_http::{open_events, response, serve_raw, Seen},
    };
    use serde_json::json;
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

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
            secret_origins: BTreeMap::new(),
            env: vec![],
        }
    }

    async fn connect(url: &str) -> Result<Connection, ToolError> {
        let transport = HttpTransport::new(url, HeaderMap::new(), None)?;
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
        let servers = McpServers::start(&directory, None, &[]).await;
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
        ] {
            assert!(
                validate(vec![url_server("a", allowed)]).is_ok(),
                "{allowed}"
            );
        }
        for refused in [
            "http://mcp.example.com/mcp",
            "http://192.168.1.2/mcp",
            "http://localhost/mcp",
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
        // 307 and 308 within the origin are followed; redirects that turn a POST into a GET are not.
        for (status, followed) in [
            (307, true),
            (308, true),
            (301, false),
            (302, false),
            (303, false),
        ] {
            let (origin, _) = serve_raw(move |seen| {
                if seen.path == "/old" {
                    Some(response(status, &[("Location", "/mcp")], ""))
                } else {
                    mcp(false)(seen)
                }
            })
            .await;
            assert_eq!(
                connect(&format!("{origin}/old")).await.is_ok(),
                followed,
                "{status}"
            );
        }
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
            ("Upgrade", "{secret:API_TOKEN}"),
            ("Expect", "{secret:API_TOKEN}"),
            ("Keep-Alive", "{secret:API_TOKEN}"),
            ("Trailer", "{secret:API_TOKEN}"),
            ("Proxy-Authorization", "{secret:API_TOKEN}"),
            ("Cookie", "{secret:API_TOKEN}"),
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
        let missing = header_accounts(&config).unwrap_err();
        assert!(
            matches!(&missing, ToolError::Execution(message) if message.contains("bw mcp secret docs API_TOKEN"))
        );
        config.secrets = vec!["API_TOKEN".into()];
        config
            .secret_origins
            .insert("API_TOKEN".into(), origin.clone());
        let accounts = header_accounts(&config).unwrap();
        assert_eq!(accounts[0].2, format!("docs/{origin}/API_TOKEN"));
        let environment = vec![("Authorization".to_owned(), token.to_owned())];
        let env = crate::child_env::ChildEnv::default();
        let connection =
            super::super::launch(&config, &std::env::temp_dir(), &env, environment, None)
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
            .feed(b"data: [{\"c\":3},1]\n\ndata: {\"b\":2}", &mut out)
            .unwrap();
        parser.finish(&mut out);
        assert_eq!(out, [json!({"a":1}), json!({"c":3}), json!({"b":2})]);

        // One line may be as large as a whole message, as real tool lists are.
        let mut out = VecDeque::new();
        let mut parser = EventParser::default();
        let big = json!({ "tools": "x".repeat(200 * 1024) });
        parser
            .feed(format!("data: {big}\n\n").as_bytes(), &mut out)
            .unwrap();
        assert_eq!(out.len(), 1);

        let mut parser = EventParser::default();
        let mut short = vec![b'x'; 1000];
        short.push(b'\n');
        for _ in 0..MAX_MESSAGE_BYTES / short.len() {
            parser.feed(&short, &mut out).unwrap();
        }
        assert!(parser.feed(&short, &mut out).is_err());
        let mut parser = EventParser::default();
        assert!(parser
            .feed(&vec![b'x'; MAX_MESSAGE_BYTES + 1], &mut out)
            .is_err());
    }

    #[test]
    fn sse_parser_fuzz_is_panic_free_and_bounded() {
        fn next(state: &mut u64) -> u8 {
            *state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (*state >> 32) as u8
        }

        let mut seed = 0x5eed_f00d_cafe_babe;
        for _ in 0..512 {
            let len = usize::from(next(&mut seed)) * 8;
            let mut input = (0..len).map(|_| next(&mut seed)).collect::<Vec<_>>();
            // Regularly mix in SSE delimiters and JSON-looking bytes.
            for (at, byte) in b"data: {\"x\":1}\n\n\r:{}[]".iter().enumerate() {
                if !input.is_empty() {
                    let index = (at * 97 + usize::from(next(&mut seed))) % input.len();
                    input[index] = *byte;
                }
            }
            let mut parser = EventParser::default();
            let mut out = VecDeque::new();
            let mut at = 0;
            while at < input.len() {
                let width = usize::from(next(&mut seed) % 31) + 1;
                let end = (at + width).min(input.len());
                parser.feed(&input[at..end], &mut out).unwrap();
                at = end;
            }
            parser.finish(&mut out);
            assert!(parser.line.len() + parser.data.len() <= input.len());
            assert!(out.len() <= input.len());
        }

        for input in [
            vec![b'x'; MAX_MESSAGE_BYTES + 1],
            [b"data: ".as_slice(), &vec![b'['; MAX_MESSAGE_BYTES]].concat(),
            b"data:\ndata:\ndata:\n\n".repeat(32 * 1024),
        ] {
            let mut parser = EventParser::default();
            let mut out = VecDeque::new();
            let _ = parser.feed(&input, &mut out);
            assert!(parser.line.len() + parser.data.len() <= MAX_MESSAGE_BYTES);
            assert!(out.len() <= MAX_MESSAGE_BYTES);
        }
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

    #[tokio::test]
    async fn owner_name_with_p_prefix_rejected() {
        let directory = folder("prefix");
        let refused = super::super::add_http_server(
            &directory,
            "P docs",
            "https://mcp.example.com/mcp",
            vec![],
        );
        assert!(refused.unwrap_err().contains("reserved"));
        // An existing file with such a name still loads; only that server is skipped.
        let (origin, _) = serve_raw(mcp(false)).await;
        let file = json!({"servers":[
            url_server("p-docs", &format!("{origin}/mcp")),
            url_server("docs", &format!("{origin}/mcp"))
        ]});
        std::fs::write(directory.join(MCP_FILE), file.to_string()).unwrap();
        assert_eq!(read_config(&directory).unwrap().len(), 2);
        let servers = McpServers::start(&directory, None, &[]).await;
        assert_eq!(servers.warnings.len(), 1);
        assert!(servers.warnings[0].contains("p-docs") && servers.warnings[0].contains("rename"));
        assert!(servers.handles("docs__search") && !servers.handles("p-docs__search"));
        std::fs::remove_dir_all(directory).unwrap();
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

    fn secret_server(url: &str, saved_for: Option<&str>) -> ServerConfig {
        let mut config = url_server("docs", url);
        config
            .headers
            .insert("Authorization".into(), "{secret:API_TOKEN}".into());
        config.secrets = vec!["API_TOKEN".into()];
        if let Some(origin) = saved_for {
            config
                .secret_origins
                .insert("API_TOKEN".into(), origin.into());
        }
        config
    }

    #[tokio::test]
    async fn editing_the_url_sends_no_header() {
        let (old, _) = serve_raw(mcp(false)).await;
        let (new, log) = serve_raw(mcp(false)).await;
        let directory = folder("moved");
        let moved = secret_server(&format!("{new}/mcp"), Some(&old));
        assert_eq!(
            moved.secrets_saved_elsewhere(),
            [("API_TOKEN", old.as_str())]
        );
        let file = json!({ "servers": [moved] });
        std::fs::write(directory.join(MCP_FILE), file.to_string()).unwrap();
        // The secret is refused before the credential store or the new address is contacted.
        let servers = McpServers::start(&directory, None, &[]).await;
        assert!(
            servers.warnings[0].contains(&format!("was saved for {old}, not {new}")),
            "{:?}",
            servers.warnings
        );
        assert!(log.lock().unwrap().is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stdio_secret_is_not_sent_as_header() {
        let config = secret_server("https://mcp.example.com/mcp", None);
        assert_eq!(config.account("API_TOKEN"), "docs/API_TOKEN");
        let refused = header_accounts(&config).unwrap_err();
        assert!(
            matches!(&refused, ToolError::Execution(message) if message.contains("local command"))
        );
    }

    #[test]
    fn moving_a_server_drops_its_approval_rules() {
        let directory = folder("rules");
        let keep = || url_server("keep", "https://k.example.com/mcp");
        write_config(
            &directory,
            vec![url_server("docs", "https://a.example.com/mcp"), keep()],
        )
        .unwrap();
        crate::policy::allow_tool(&directory, "docs", None).unwrap();
        crate::policy::allow_tool(&directory, "keep__search", None).unwrap();
        // A new path on the same origin keeps them.
        write_config(
            &directory,
            vec![url_server("docs", "https://a.example.com/v2"), keep()],
        )
        .unwrap();
        assert_eq!(crate::policy::tool_rules(&directory).unwrap().len(), 2);
        write_config(
            &directory,
            vec![url_server("docs", "https://b.example.com/mcp"), keep()],
        )
        .unwrap();
        let rules = crate::policy::tool_rules(&directory).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].tool, "keep__search");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn descriptions_and_schemas_are_redacted() {
        let (origin, _) = serve_raw(|seen| {
            let request: Value = serde_json::from_str(&seen.body).unwrap_or_default();
            if request["method"] == "tools/list" {
                return Some(json_reply(
                    &request["id"],
                    json!({"tools":[{
                        "name":"search",
                        "description":"uses token-123",
                        "inputSchema":{"type":"object","properties":{"q":{"description":"not token-123"}}}
                    }]}),
                ));
            }
            mcp(false)(seen)
        })
        .await;
        let url = format!("{origin}/mcp");
        let transport = HttpTransport::new(&url, HeaderMap::new(), None).unwrap();
        let mut connection = Connection::new(Box::new(transport), vec!["token-123".into()]);
        connection.initialize().await.unwrap();
        let mut servers = McpServers::default();
        servers
            .add(url_server("docs", &url), connection, &mut vec![])
            .await;
        let definition = servers.definitions()[0].to_string();
        assert!(!definition.contains("token-123"), "{definition}");
        assert!(definition.contains("uses [redacted]") && definition.contains("not [redacted]"));
    }

    #[tokio::test]
    async fn server_request_flood_breaks_the_connection() {
        let (origin, _) = serve_raw(|seen| {
            let request: Value = serde_json::from_str(&seen.body).unwrap_or_default();
            if request["method"] == "tools/list" {
                let asks: Vec<Value> = (0..=MAX_SERVER_REQUESTS)
                    .map(|index| json!({"jsonrpc":"2.0","id":index,"method":"ping"}))
                    .collect();
                return Some(event_reply(&asks));
            }
            mcp(false)(seen)
        })
        .await;
        let mut connection = connect(&format!("{origin}/mcp")).await.unwrap();
        let flooded = connection.list_tools().await;
        assert!(
            matches!(flooded, Err(ToolError::Execution(message)) if message.contains("too many requests"))
        );
        assert!(connection.broken);
    }

    #[tokio::test]
    async fn pending_messages_are_capped() {
        let (origin, _) = serve_raw(|seen| {
            let request: Value = serde_json::from_str(&seen.body).unwrap_or_default();
            if request["method"] == "tools/list" {
                let many: Vec<Value> = (0..=MAX_PENDING)
                    .map(|_| json!({"jsonrpc":"2.0","method":"notifications/message"}))
                    .collect();
                let body = Value::Array(many).to_string();
                return Some(response(
                    200,
                    &[("Content-Type", "application/json")],
                    &body,
                ));
            }
            mcp(false)(seen)
        })
        .await;
        let mut connection = connect(&format!("{origin}/mcp")).await.unwrap();
        let flooded = connection.list_tools().await;
        assert!(
            matches!(flooded, Err(ToolError::Execution(message)) if message.contains("too many messages"))
        );
    }

    /// Every reply is a live event stream that never ends, and notifications get one too.
    #[tokio::test]
    async fn open_event_streams_never_block_later_requests() {
        let (origin, _) = serve_raw(|seen| {
            let request: Value = serde_json::from_str(&seen.body).unwrap_or_default();
            let method = request["method"].as_str().unwrap_or_default();
            let Some(result) = result_of(&request, method) else {
                return Some(open_events(": nothing to say\n\n"));
            };
            let reply = json!({"jsonrpc":"2.0","id":request["id"],"result":result});
            Some(open_events(&format!("data: {reply}\n\n")))
        })
        .await;
        let transport =
            HttpTransport::new(&format!("{origin}/mcp"), HeaderMap::new(), None).unwrap();
        let mut connection = Connection::new(Box::new(transport), vec![]);
        tokio::time::timeout(Duration::from_secs(5), async {
            connection.initialize().await.unwrap();
            assert_eq!(connection.list_tools().await.unwrap().len(), 2);
            assert_eq!(connection.list_tools().await.unwrap().len(), 2);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn accepted_or_empty_replies_to_notifications_are_fine() {
        for reply in [
            response(202, &[("Content-Type", "text/event-stream")], ""),
            response(200, &[("Content-Type", "application/json")], ""),
            response(200, &[("Content-Type", "text/event-stream")], ""),
        ] {
            let (origin, _) = serve_raw(move |seen| {
                let request: Value = serde_json::from_str(&seen.body).unwrap_or_default();
                if request.get("id").is_none() {
                    return Some(reply.clone());
                }
                mcp(false)(seen)
            })
            .await;
            assert!(connect(&format!("{origin}/mcp")).await.is_ok());
        }
    }

    #[tokio::test]
    async fn lost_session_is_initialized_again_once() {
        let sessions = Arc::new(Mutex::new(0_u32));
        let (origin, log) = serve_raw(move |seen| {
            let request: Value = serde_json::from_str(&seen.body).unwrap_or_default();
            let method = request["method"].as_str().unwrap_or_default();
            if method == "initialize" {
                let mut count = sessions.lock().unwrap();
                *count += 1;
                let reply = json_reply(&request["id"], result_of(&request, method)?);
                let header = format!("\r\nMcp-Session-Id: s{count}\r\n");
                return Some(reply.replacen("\r\n", &header, 1));
            }
            // The first session is forgotten after initialization.
            if method == "tools/list" && seen.header(SESSION) == Some("s1") {
                return Some(response(404, &[], ""));
            }
            mcp(false)(seen)
        })
        .await;
        let mut connection = connect(&format!("{origin}/mcp")).await.unwrap();
        assert_eq!(connection.list_tools().await.unwrap().len(), 2);
        assert_eq!(
            methods(&log),
            [
                "POST initialize",
                "POST notifications/initialized",
                "POST tools/list",
                "POST initialize",
                "POST notifications/initialized",
                "POST tools/list"
            ]
        );
        let seen = log.lock().unwrap().clone();
        assert_eq!(seen[3].header(SESSION), None);
        assert_eq!(seen[3].header(VERSION), None);
        assert_eq!(seen[5].header(SESSION), Some("s2"));
    }

    /// A body sent back for a client reply is closed unread, so it cannot answer a request.
    #[tokio::test]
    async fn replies_to_server_requests_cannot_supply_answers() {
        let (origin, _) = serve_raw(|seen| {
            let request: Value = serde_json::from_str(&seen.body).unwrap_or_default();
            if request["method"] == "tools/list" {
                let ping = json!({"jsonrpc":"2.0","id":"s1","method":"ping"});
                return Some(open_events(&format!("data: {ping}\n\n")));
            }
            if request["id"] == "s1" {
                let forged = json!({"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"forged"}]}});
                return Some(event_reply(&[forged]));
            }
            mcp(false)(seen)
        })
        .await;
        let mut connection = connect(&format!("{origin}/mcp")).await.unwrap();
        let listed =
            tokio::time::timeout(Duration::from_millis(500), connection.list_tools()).await;
        assert!(listed.is_err(), "{listed:?}");
    }

    #[tokio::test]
    async fn project_servers_prefixed_and_need_trust() {
        let (origin, log) = serve_raw(mcp(false)).await;
        let root = folder("project");
        let (data, workspace) = (root.join("data"), root.join("work"));
        std::fs::create_dir_all(workspace.join(".blackwall")).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let file = json!({"mcp":{"servers":[{"name":"docs","url":format!("{origin}/mcp")}]}});
        std::fs::write(workspace.join(".blackwall/project.json"), file.to_string()).unwrap();
        let untrusted = McpServers::start(&data, Some(&workspace), &[]).await;
        assert!(untrusted.warnings.is_empty(), "{:?}", untrusted.warnings);
        assert!(untrusted.definitions().is_empty());
        let sha = crate::project::status(&workspace, &data).sha256.unwrap();
        crate::project::trust(&workspace, &data, &sha).unwrap();
        // Trusted, it is started as p-docs, and refused because its address is this computer.
        let trusted = McpServers::start(&data, Some(&workspace), &[]).await;
        assert_eq!(trusted.warnings.len(), 1, "{:?}", trusted.warnings);
        assert!(trusted.warnings[0].starts_with("Project MCP server p-docs did not start"));
        assert!(trusted.warnings[0].contains("private network"));
        assert!(log.lock().unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn project_server_addresses_must_be_public() {
        for local in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "::ffff:192.168.1.1",
            "ff02::1",
        ] {
            assert!(local_address(local.parse().unwrap()), "{local}");
        }
        for public in ["1.1.1.1", "172.32.0.1", "8.8.8.8", "2606:4700::1111"] {
            assert!(!local_address(public.parse().unwrap()), "{public}");
        }
        assert!(public_addresses("https://10.0.0.1/mcp").await.is_err());
        assert!(public_addresses("https://[fe80::1]/mcp").await.is_err());
        // A name is resolved and refused when it points at this computer.
        assert!(public_addresses("https://localhost/mcp").await.is_err());
        assert_eq!(public_addresses("https://1.1.1.1/mcp").await.unwrap(), None);
    }

    #[tokio::test]
    async fn project_server_approvals_say_who_defined_them() {
        let (origin, _) = serve_raw(mcp(false)).await;
        let url = format!("{origin}/mcp");
        let mut servers = McpServers::default();
        servers
            .add(
                url_server("p-docs", &url),
                connect(&url).await.unwrap(),
                &mut vec![],
            )
            .await;
        let approvals = Approvals::default();
        let details = Arc::new(Mutex::new(vec![]));
        let seen = details.clone();
        let allow = allow(&approvals);
        let emit = move |event: AgentEvent| {
            if let AgentEvent::ApprovalRequest { detail, .. } = &event {
                seen.lock().unwrap().push(detail.clone());
            }
            allow(event);
        };
        servers
            .call(
                "p-docs__search",
                "{}",
                "run",
                &approvals,
                &emit,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert!(details.lock().unwrap()[0].contains("p-docs (127.0.0.1), defined by this project"));
    }
}
