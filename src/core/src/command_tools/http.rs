//! Owner HTTP tools. Each sends one request to the fixed origin in `tools.json`: model values only
//! fill placeholders in the path or the query, percent-encoded, and header values come only from
//! the credential store, saved for that origin. Redirects, proxies, and compressed responses are
//! refused, and the response is capped at 64 KiB as it streams.
use super::{
    shown_bytes, valid_name, BodyMode, CommandTool, Method, StdinMode, ToolKind,
    COMMAND_TOOLS_FILE, MAX_FILE_BYTES,
};
use crate::{approvals::Approvals, protocol::AgentEvent, secrets::SecretError, tools::ToolError};
use futures_util::StreamExt;
use reqwest::{
    header::{HeaderName, HeaderValue, ACCEPT_ENCODING, CONTENT_ENCODING, USER_AGENT},
    redirect::Policy,
    Url,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path, sync::Arc, time::Duration};

/// Credential store service for HTTP tool headers; each account is `{origin}|{NAME}`, so a value
/// is only ever sent to the origin it was saved for.
pub const SECRET_SERVICE: &str = "com.blackwall.tools";
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_URL: usize = 2048;
/// The longest request URL once values are filled in.
const MAX_RESOLVED_URL: usize = 8 * 1024;
const MAX_HEADERS: usize = 8;
const MAX_SECRET_BYTES: usize = 16 * 1024;
/// How much of an error response is shown, after redaction.
const MAX_ERROR_EXCERPT: usize = 200;
const SECRET_TIMEOUT: Duration = Duration::from_secs(30);
/// Headers Blackwall sets or that change how the request is framed.
const RESERVED_HEADERS: [&str; 13] = [
    "host",
    "content-length",
    "content-type",
    "content-encoding",
    "transfer-encoding",
    "accept-encoding",
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "upgrade",
    "proxy-authorization",
    "proxy-connection",
];
/// Query names whose values are hidden in the approval prompt, matched exactly (ignoring case),
/// as are names ending in `_key` or `_token`.
const SECRET_QUERY_NAMES: [&str; 12] = [
    "token",
    "secret",
    "password",
    "passwd",
    "key",
    "apikey",
    "auth",
    "signature",
    "sig",
    "credential",
    "credentials",
    "client_secret",
];
/// Value prefixes of common access tokens, hidden in the approval prompt whatever their name.
const SECRET_VALUE_PREFIXES: [&str; 8] = [
    "ghp_",
    "gho_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "sk-",
    "AKIA",
];
const PLACEMENT: &str =
    "A tool URL may use placeholders only in its path or query values, never in its scheme, host, port, or sign-in.";
const INVALID_URL: &str = "A tool has an invalid URL.";

/// Reads one header value by its credential-store account. Blocking.
pub(super) type Lookup =
    Arc<dyn Fn(&str) -> Result<Option<String>, SecretError> + Send + Sync + 'static>;

enum Piece<'a> {
    Text(&'a str),
    Name(&'a str),
}

/// Literal text and `{name}` placeholders; a stray or unnamed brace is refused.
fn pieces(text: &str) -> Option<Vec<Piece<'_>>> {
    let mut out = vec![];
    let mut rest = text;
    while let Some(start) = rest.find(['{', '}']) {
        if rest.as_bytes()[start] == b'}' {
            return None;
        }
        out.push(Piece::Text(&rest[..start]));
        let after = &rest[start + 1..];
        let end = after.find('}')?;
        let name = &after[..end];
        if !valid_name(name) {
            return None;
        }
        out.push(Piece::Name(name));
        rest = &after[end + 1..];
    }
    out.push(Piece::Text(rest));
    Some(out)
}

/// One query pair: its literal name and, after `=`, its value pieces.
type QueryPair<'a> = (&'a str, Option<Vec<Piece<'a>>>);

/// A parsed URL template: the fixed `scheme://authority`, then path and query pieces.
pub(super) struct Template<'a> {
    prefix: &'a str,
    path: Vec<Piece<'a>>,
    query: Option<Vec<QueryPair<'a>>>,
    /// `scheme://host:port`, with the port always written out.
    pub origin: String,
    pub names: Vec<&'a str>,
}

impl<'a> Template<'a> {
    pub fn parse(text: &'a str) -> Result<Self, &'static str> {
        if text.is_empty()
            || text.len() > MAX_URL
            || text.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(INVALID_URL);
        }
        let start = text.find("://").ok_or(INVALID_URL)? + 3;
        let end = text[start..]
            .find(['/', '?', '#'])
            .map_or(text.len(), |offset| start + offset);
        let prefix = &text[..end];
        if prefix.contains(['{', '}']) {
            return Err(PLACEMENT);
        }
        if prefix.contains('@') || text.contains('#') {
            return Err(INVALID_URL);
        }
        let rest = &text[end..];
        let (path, query) = match rest.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (rest, None),
        };
        let path = pieces(path).ok_or(INVALID_URL)?;
        let query = query
            .map(|query| {
                query
                    .split('&')
                    .map(|pair| {
                        let (name, value) = match pair.split_once('=') {
                            Some((name, value)) => (name, Some(value)),
                            None => (pair, None),
                        };
                        if name.contains(['{', '}']) {
                            return Err(PLACEMENT);
                        }
                        let value = value
                            .map(|value| pieces(value).ok_or(INVALID_URL))
                            .transpose()?;
                        Ok((name, value))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let mut template = Self {
            prefix,
            path,
            query,
            origin: String::new(),
            names: vec![],
        };
        template.names = template
            .all_pieces()
            .filter_map(|piece| match piece {
                Piece::Name(name) => Some(*name),
                Piece::Text(_) => None,
            })
            .collect();
        let sample = template
            .build(&mut |_| Ok("x".into()))
            .map_err(|_| INVALID_URL)?;
        let url = Url::parse(&sample).map_err(|_| INVALID_URL)?;
        template.origin = checked_origin(&url)?;
        Ok(template)
    }

    fn all_pieces(&self) -> impl Iterator<Item = &Piece<'a>> {
        self.path.iter().chain(
            self.query
                .iter()
                .flatten()
                .filter_map(|(_, value)| value.as_ref())
                .flatten(),
        )
    }

    /// The URL text with each placeholder replaced by `fill(name)`, already encoded.
    fn build(
        &self,
        fill: &mut dyn FnMut(&str) -> Result<String, ToolError>,
    ) -> Result<String, ToolError> {
        let mut text = self.prefix.to_owned();
        let mut write = |pieces: &[Piece], text: &mut String| -> Result<(), ToolError> {
            for piece in pieces {
                match piece {
                    Piece::Text(literal) => text.push_str(literal),
                    Piece::Name(name) => text.push_str(&fill(name)?),
                }
            }
            Ok(())
        };
        write(&self.path, &mut text)?;
        if let Some(query) = &self.query {
            text.push('?');
            for (index, (name, value)) in query.iter().enumerate() {
                if index > 0 {
                    text.push('&');
                }
                text.push_str(name);
                if let Some(value) = value {
                    text.push('=');
                    write(value, &mut text)?;
                }
            }
        }
        Ok(text)
    }

    /// The request URL for these arguments. Its origin must be the template's, and no path
    /// segment may be `.` or `..`, so a value can never reach another host or folder.
    fn resolve(&self, tool: &CommandTool, values: &Value) -> Result<Url, ToolError> {
        let text = self.build(&mut |name| {
            let value = tool.value_text(values, name)?;
            if value.is_empty() {
                return Err(ToolError::Arguments);
            }
            Ok(encode(&value))
        })?;
        let path_end = text[self.prefix.len()..]
            .find('?')
            .map_or(text.len(), |offset| self.prefix.len() + offset);
        if text[self.prefix.len()..path_end]
            .split('/')
            .any(|segment| segment == "." || segment == "..")
        {
            return Err(ToolError::Arguments);
        }
        if text.len() > MAX_RESOLVED_URL {
            return Err(ToolError::Arguments);
        }
        let url = Url::parse(&text).map_err(|_| ToolError::Arguments)?;
        if checked_origin(&url).ok().as_deref() != Some(self.origin.as_str()) {
            return Err(ToolError::Arguments);
        }
        Ok(url)
    }
}

/// `https`, or plain `http` only to the literal loopback addresses, with no sign-in part.
fn checked_origin(url: &Url) -> Result<String, &'static str> {
    let host = url.host_str().filter(|host| !host.is_empty());
    let Some(host) = host else {
        return Err(INVALID_URL);
    };
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(INVALID_URL);
    }
    match url.scheme() {
        "https" => {}
        "http" if matches!(host, "127.0.0.1" | "[::1]") => {}
        _ => return Err("HTTP tools use https, or plain http only to 127.0.0.1 or ::1."),
    }
    let port = url.port_or_known_default().ok_or(INVALID_URL)?;
    Ok(format!("{}://{host}:{port}", url.scheme()))
}

/// Every byte but the unreserved ones is percent-encoded, so a value stays one path segment or
/// one query value.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn secret_reference(value: &str) -> Option<&str> {
    value
        .strip_prefix("{secret:")?
        .strip_suffix('}')
        .filter(|name| crate::mcp::valid_secret_name(name))
}

pub(super) fn validate(tool: &CommandTool) -> Result<(), &'static str> {
    if !tool.command.is_empty() || tool.stdin != StdinMode::None {
        return Err("HTTP tools take a url, not a command or standard input.");
    }
    let url = tool.url.as_deref().ok_or(INVALID_URL)?;
    Template::parse(url)?;
    if tool.body == BodyMode::Json && tool.method != Some(Method::Post) {
        return Err("A JSON body needs method POST.");
    }
    let mut names: Vec<String> = tool
        .headers
        .keys()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    names.sort();
    names.dedup();
    if names.len() != tool.headers.len() {
        return Err("A tool lists the same header twice.");
    }
    if tool.headers.len() > MAX_HEADERS
        || tool.headers.iter().any(|(name, value)| {
            let plain = !name.is_empty()
                && name.len() <= 64
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
            !plain
                || RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str())
                || secret_reference(value).is_none()
        })
    {
        return Err("Tool headers take only {secret:NAME} values, saved with bw tools secret.");
    }
    Ok(())
}

/// The URL as the approval prompt shows it, with secret-looking query values hidden.
fn shown(url: &Url) -> String {
    let text = url.as_str();
    let Some((base, query)) = text.split_once('?') else {
        return text.to_owned();
    };
    let pairs: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => {
                let lower = name.to_ascii_lowercase();
                let hidden = SECRET_QUERY_NAMES.contains(&lower.as_str())
                    || lower.ends_with("_key")
                    || lower.ends_with("_token")
                    || SECRET_VALUE_PREFIXES
                        .iter()
                        .any(|prefix| value.starts_with(prefix));
                if hidden {
                    format!("{name}=[redacted]")
                } else {
                    pair.to_owned()
                }
            }
            None => pair.to_owned(),
        })
        .collect();
    format!("{base}?{}", pairs.join("&"))
}

fn account(origin: &str, name: &str) -> String {
    format!("{origin}|{name}")
}

fn redact(text: String, secrets: &[String]) -> String {
    secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .fold(text, |text, secret| {
            text.replace(secret.as_str(), "[redacted]")
        })
}

async fn read_secret(
    lookup: &Lookup,
    origin: &str,
    tool: &str,
    name: &str,
) -> Result<String, ToolError> {
    let lookup = lookup.clone();
    let key = account(origin, name);
    let read = tokio::task::spawn_blocking(move || lookup(&key));
    match tokio::time::timeout(SECRET_TIMEOUT, read).await {
        Ok(Ok(Ok(Some(value)))) => Ok(value),
        Ok(Ok(Ok(None))) => Err(ToolError::Execution(format!(
            "{name} is not saved for {origin}. Add it with bw tools secret {tool} {name}."
        ))),
        Ok(Ok(Err(error))) => Err(ToolError::Execution(error.explain(format!(
            "{name} could not be read from {}. {}, then try again.",
            crate::secrets::STORE,
            crate::secrets::RECOVER
        )))),
        _ => Err(ToolError::Execution(format!(
            "{} did not answer in time. {}, then try again.",
            crate::secrets::STORE,
            crate::secrets::RECOVER
        ))),
    }
}

fn request_failed(error: &reqwest::Error) -> ToolError {
    // reqwest's own message can include the URL, so it is never shown.
    ToolError::Execution(
        if error.is_timeout() {
            "The service did not answer in time."
        } else if error.is_connect() {
            "The service could not be reached."
        } else {
            "The request failed."
        }
        .into(),
    )
}

pub(super) async fn run(
    tool: &CommandTool,
    values: &Value,
    request_id: &str,
    approvals: &Approvals,
    emit: &(dyn Fn(AgentEvent) + Send + Sync),
    timeout: Duration,
    lookup: Lookup,
) -> Result<String, ToolError> {
    let invalid = || ToolError::Execution(INVALID_URL.into());
    let template = Template::parse(tool.url.as_deref().ok_or_else(invalid)?)
        .map_err(|error| ToolError::Execution(error.into()))?;
    let url = template.resolve(tool, values)?;
    let method = tool.method.unwrap_or_default();
    let body = match tool.body {
        BodyMode::Json => Some(serde_json::to_vec(values).map_err(|_| ToolError::Arguments)?),
        BodyMode::None => None,
    };
    if body
        .as_ref()
        .is_some_and(|body| body.len() > super::MAX_INPUT_BYTES)
    {
        return Err(ToolError::Arguments);
    }
    let method_name = match method {
        Method::Get => "GET",
        Method::Post => "POST",
    };
    let mut digest = Sha256::new();
    digest.update(format!("{method_name} {url}\n"));
    digest.update(body.as_deref().unwrap_or_default());
    let digest: String = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut detail = format!(
        "Tool: {}\nRequest: {method_name} {}",
        tool.name,
        shown(&url)
    );
    if !tool.headers.is_empty() {
        let names: Vec<&str> = tool.headers.keys().map(String::as_str).collect();
        detail.push_str(&format!(
            "\nHeaders from {}: {}",
            crate::secrets::STORE,
            names.join(", ")
        ));
    }
    if let Some(body) = &body {
        detail.push_str(&format!("\nBody: JSON, {}", shown_bytes(body)));
    }
    detail.push_str(&format!(
        "\nRequest sha256: {digest}\n\nThe service receives this request and your network address. Its response is treated as untrusted information."
    ));
    let host = url.host_str().unwrap_or_default().to_owned();
    // A request that carries saved credentials or model-written data is allowed only by a rule
    // naming this tool; a host allow alone still asks. Deny and ask rules for the host apply.
    let credentialed = !tool.headers.is_empty() || tool.body == BodyMode::Json;
    if !approvals
        .request_http_tool(request_id, &tool.name, &host, credentialed, detail, emit)
        .await
        .map_err(ToolError::Execution)?
    {
        return Err(ToolError::Denied);
    }
    let mut headers = vec![];
    let mut secrets = vec![];
    for (name, value) in &tool.headers {
        let key = secret_reference(value).ok_or_else(invalid)?;
        let secret = read_secret(&lookup, &template.origin, &tool.name, key).await?;
        let header =
            HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()).map_err(|_| invalid())?;
        let mut value = HeaderValue::from_str(secret.trim()).map_err(|_| {
            ToolError::Execution(format!(
                "The saved value of {key} is not a valid header value."
            ))
        })?;
        value.set_sensitive(true);
        secrets.push(secret.trim().to_owned());
        headers.push((header, value));
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(timeout.min(Duration::from_secs(10)))
        .timeout(timeout)
        .build()
        .map_err(|_| ToolError::Execution("The HTTP client could not start.".into()))?;
    let mut request = client
        .request(
            match method {
                Method::Get => reqwest::Method::GET,
                Method::Post => reqwest::Method::POST,
            },
            url,
        )
        .header(USER_AGENT, concat!("Blackwall/", env!("CARGO_PKG_VERSION")))
        // Compressed responses are refused below, so ask for none.
        .header(ACCEPT_ENCODING, "identity");
    for (name, value) in headers {
        request = request.header(name, value);
    }
    if let Some(body) = body {
        request = request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
    }
    let response = request
        .send()
        .await
        .map_err(|error| request_failed(&error))?;
    let status = response.status();
    if status.is_redirection() {
        return Err(ToolError::Execution(format!(
            "The service answered with a redirect (HTTP {}). Redirects are not followed; put the final URL in tools.json.",
            status.as_u16()
        )));
    }
    // Without a decoder the streamed bytes are the response bytes, so the cap counts what the
    // model would read. A compressed body could inflate past it, so it is refused. If reqwest's
    // gzip, brotli, deflate or zstd feature is ever enabled, `http_tool_cap_counts_decompressed_bytes`
    // in `http/tests.rs` is the guard: it must keep passing with the cap counted after decoding.
    if response
        .headers()
        .get(CONTENT_ENCODING)
        .is_some_and(|encoding| !encoding.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return Err(ToolError::Execution(
            "The service sent a compressed response, which HTTP tools do not accept.".into(),
        ));
    }
    let limit = if status.is_success() {
        MAX_RESPONSE_BYTES
    } else {
        MAX_ERROR_EXCERPT * 4
    };
    let mut bytes = vec![];
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| request_failed(&error))?;
        if bytes.len() + chunk.len() > limit {
            if !status.is_success() {
                bytes.extend_from_slice(&chunk[..limit - bytes.len()]);
                break;
            }
            return Err(ToolError::Execution(
                "The response exceeded the 64 KiB limit.".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let text = redact(String::from_utf8_lossy(&bytes).into_owned(), &secrets);
        let excerpt: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(MAX_ERROR_EXCERPT)
            .collect();
        let excerpt = excerpt.trim();
        return Err(ToolError::Execution(if excerpt.is_empty() {
            format!("The service returned HTTP {}.", status.as_u16())
        } else {
            format!("The service returned HTTP {}: {excerpt}", status.as_u16())
        }));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ToolError::Execution("The response is not UTF-8 text.".into()))?;
    Ok(format!(
        "HTTP {}\n{}",
        status.as_u16(),
        redact(text, &secrets)
    ))
}

/// The origin of the owner's HTTP tool `name`, from `tools.json` in `directory`.
fn tool_origin(directory: &Path, name: &str) -> Result<String, String> {
    let mut body = vec![];
    std::fs::File::open(directory.join(COMMAND_TOOLS_FILE))
        .and_then(|file| file.take(MAX_FILE_BYTES as u64 + 1).read_to_end(&mut body))
        .map_err(|_| "The tools file could not be read.")?;
    let tools = super::parse_file(&body)?;
    let tool = tools
        .iter()
        .find(|tool| tool.name == name && tool.kind == ToolKind::Http)
        .ok_or_else(|| format!("No HTTP tool is named {name:?} in {COMMAND_TOOLS_FILE}."))?;
    let url = tool.url.as_deref().ok_or(INVALID_URL)?;
    Ok(Template::parse(url)?.origin)
}

fn store_error(error: SecretError) -> String {
    error.explain(format!(
        "The credential store could not be updated. {}, then try again.",
        crate::secrets::RECOVER
    ))
}

/// Save a header value for the origin of HTTP tool `tool`, which is returned. Blocking.
pub fn save_secret(directory: &Path, tool: &str, key: &str, value: &str) -> Result<String, String> {
    save_secret_with(directory, tool, key, value, crate::secrets::set)
}

/// [`save_secret`] with the secret write supplied by the caller.
pub fn save_secret_with(
    directory: &Path,
    tool: &str,
    key: &str,
    value: &str,
    set: crate::secrets::SetSecret,
) -> Result<String, String> {
    if !crate::mcp::valid_secret_name(key) {
        return Err("Use a name of uppercase letters, digits, and _, such as GITLAB_TOKEN.".into());
    }
    if value.trim().is_empty() || value.len() > MAX_SECRET_BYTES {
        return Err("Provide a value of up to 16 KiB.".into());
    }
    let origin = tool_origin(directory, tool)?;
    set(SECRET_SERVICE, &account(&origin, key), value).map_err(|error| {
        error.explain_save(format!(
            "The credential store could not be updated. {}, then try again.",
            crate::secrets::RECOVER
        ))
    })?;
    Ok(origin)
}

/// Remove the header value saved for the origin of HTTP tool `tool`. Blocking. Returns the
/// origin and whether a value was saved.
pub fn delete_secret(directory: &Path, tool: &str, key: &str) -> Result<(String, bool), String> {
    if !crate::mcp::valid_secret_name(key) {
        return Err("Use a name of uppercase letters, digits, and _, such as GITLAB_TOKEN.".into());
    }
    let origin = tool_origin(directory, tool)?;
    let account = account(&origin, key);
    let saved = crate::secrets::get(SECRET_SERVICE, &account)
        .map_err(store_error)?
        .is_some();
    if saved {
        crate::secrets::delete(SECRET_SERVICE, &account).map_err(store_error)?;
    }
    Ok((origin, saved))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
