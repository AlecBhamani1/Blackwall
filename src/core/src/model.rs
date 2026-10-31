//! Headless streaming model transport, including fragmented OpenAI-compatible tool calls.
use crate::{
    connection::normalize_endpoint,
    protocol::{Attachment, ChatMessage, ChatRequest, MessageRole, TokenUsage},
};
use futures_util::{future::BoxFuture, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, time::Duration};
use thiserror::Error;

mod text_tools;
mod think;
pub use text_tools::{parse_text_tool_calls, MAX_TEXT_CALLS};
pub use think::ThinkFilter;

/// The most bytes one streamed response may carry, answer and reasoning together.
const MAX_STREAM: usize = 8 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("The model service could not be reached. Check your connection and try again.")]
    Network,
    /// The status, and what the server said about it when it explained, cleaned by
    /// [`server_error_detail`].
    #[error("{}", http_message(*.0, .1.as_deref()))]
    Http(u16, Option<String>),
    #[error("The model returned an incomplete or invalid response.")]
    Invalid,
    #[error(
        "This request or response exceeds Blackwall's size limit. Start a shorter conversation."
    )]
    Limit,
}
impl ModelError {
    /// A stable name for traces.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Http(..) => "http",
            Self::Invalid => "invalid_response",
            Self::Limit => "limit",
        }
    }
}
fn http_message(status: u16, detail: Option<&str>) -> String {
    match detail {
        Some(detail) => format!("The model service returned HTTP {status}. {detail}"),
        None => format!(
            "The model service rejected this request (HTTP {status}). Check the model and access key."
        ),
    }
}

/// The most characters of a server's own error message that are shown.
const MAX_DETAIL: usize = 300;
/// The most bytes of an error response that are read.
pub const MAX_ERROR_BODY: usize = 64 * 1024;

/// What a model server said in an error response, safe to show: the `error` text or
/// `error.message` of Ollama, llama.cpp and OpenAI bodies, vLLM's top-level `message`, a
/// FastAPI `detail`, or short plain text. Control characters are removed, credentials
/// redacted, and the result clipped to 300 characters. HTML pages and empty bodies give `None`.
pub fn server_error_detail(body: &[u8]) -> Option<String> {
    let body = body.get(..MAX_ERROR_BODY).unwrap_or(body);
    let text = String::from_utf8_lossy(body);
    let message = match serde_json::from_str::<Value>(text.trim()) {
        Ok(value) => {
            let error = value.get("error");
            [
                error.and_then(Value::as_str),
                error.and_then(|e| e.get("message")).and_then(Value::as_str),
                value.get("message").and_then(Value::as_str),
                value.get("detail").and_then(Value::as_str),
            ]
            .into_iter()
            .flatten()
            .next()?
            .to_owned()
        }
        Err(_) if text.trim_start().starts_with('<') => return None,
        Err(_) => text.into_owned(),
    };
    clean_detail(&message)
}

/// Replace every copy of the request's own `key` in an error body, so a server that echoes
/// it back cannot get it shown, saved or exported. Done before any cleaning or clipping.
pub fn redact_key(text: String, key: Option<&str>) -> String {
    match key.filter(|key| !key.is_empty()) {
        Some(key) if text.contains(key) => text.replace(key, "[redacted]"),
        _ => text,
    }
}

/// Collapse whitespace and control characters, redact credentials, and clip.
fn clean_detail(text: &str) -> Option<String> {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .map(|word| {
            // Quotes and punctuation around a key must not hide it.
            let bare =
                word.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'));
            // Long unbroken key-like words are access keys far more often than prose.
            let keyish = bare.len() >= 32
                && bare
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !bare.is_empty() && (keyish || bare.starts_with("sk-") || bare.starts_with("bw1_")) {
                return word.replace(bare, "[redacted]");
            }
            // Credentials inside an address: `https://user:pw@host` keeps only the host.
            match word.split_once("://") {
                Some((scheme, rest)) if rest.split('/').next().is_some_and(|a| a.contains('@')) => {
                    let host = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
                    format!("{scheme}://[redacted]@{host}")
                }
                _ => word.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let redacted = crate::policy::redact_text(&flat);
    let redacted = redacted.trim();
    if redacted.is_empty() {
        return None;
    }
    Some(if redacted.chars().count() > MAX_DETAIL {
        let mut clipped: String = redacted.chars().take(MAX_DETAIL - 1).collect();
        clipped.push('…');
        clipped
    } else {
        redacted.to_owned()
    })
}

/// [`server_error_detail`] read from a failed response, named after the server when this
/// session already knows which kind `endpoint` is, such as `Ollama: model requires more memory`.
/// `key`, the request's own credential, is removed first.
pub async fn response_error_detail(
    endpoint: &str,
    response: reqwest::Response,
    key: Option<&str>,
) -> Option<String> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    let read = async {
        while let Some(Ok(chunk)) = stream.next().await {
            let room = MAX_ERROR_BODY.saturating_sub(body.len());
            body.extend_from_slice(chunk.get(..room).unwrap_or(&chunk));
            if body.len() >= MAX_ERROR_BODY {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), read).await;
    let body = redact_key(String::from_utf8_lossy(&body).into_owned(), key);
    let detail = server_error_detail(body.as_bytes())?;
    Some(match crate::connection::known_kind(endpoint) {
        Some(kind) if kind != crate::connection::ServerKind::Unknown => {
            format!("{}: {detail}", kind.display_name())
        }
        _ => detail,
    })
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ToolCall {
    pub id: String,
    pub function: ToolFunction,
}
impl ToolCall {
    pub fn as_json(&self) -> Value {
        json!({"id":self.id,"type":"function","function":self.function})
    }
}
#[derive(Default)]
pub struct ModelTurn {
    pub content: String,
    pub calls: Vec<ToolCall>,
    pub usage: Option<TokenUsage>,
    /// The model's reasoning, kept apart from `content`. It is never sent back to the model.
    pub reasoning: String,
    /// How long the model spent reasoning, in milliseconds.
    pub reasoning_ms: u64,
    /// The calls were written as text, not sent as structured calls. They always ask.
    pub text_calls: bool,
}
/// The most reasoning kept for one run, across all its model calls.
pub const MAX_RUN_REASONING: usize = 256 * 1024;
const TRUNCATED: &str = "\n\n[Reasoning truncated]";

/// Reasoning gathered across the model calls of one run, for `TurnComplete`, up to
/// [`MAX_RUN_REASONING`] bytes.
#[derive(Default)]
pub struct Reasoning {
    text: String,
    ms: u64,
    truncated: bool,
}
impl Reasoning {
    pub fn add(&mut self, turn: &ModelTurn) {
        if turn.reasoning.is_empty() {
            return;
        }
        self.ms = self.ms.saturating_add(turn.reasoning_ms);
        if self.truncated {
            return;
        }
        if !self.text.is_empty() {
            self.text.push_str("\n\n");
        }
        let room = MAX_RUN_REASONING.saturating_sub(self.text.len());
        if turn.reasoning.len() <= room {
            self.text.push_str(&turn.reasoning);
            return;
        }
        let end = (0..=room)
            .rev()
            .find(|&index| turn.reasoning.is_char_boundary(index))
            .unwrap_or(0);
        self.text.push_str(&turn.reasoning[..end]);
        self.text.push_str(TRUNCATED);
        self.truncated = true;
    }
    /// The text and duration, or nothing when the model did not reason.
    pub fn take(&mut self) -> (Option<String>, Option<u64>) {
        let text = std::mem::take(&mut self.text);
        if text.is_empty() {
            (None, None)
        } else {
            (Some(text), Some(self.ms))
        }
    }
}
pub trait ModelBackend: Send + Sync {
    fn context_budget(&self) -> crate::context::ContextBudget {
        Default::default()
    }
    /// The requested model's name, recorded in traces.
    fn model_name(&self) -> Option<&str> {
        None
    }
    /// The model server's endpoint, recorded as `server.address` in usage metrics.
    fn server_address(&self) -> Option<&str> {
        None
    }
    fn generate<'a>(
        &'a self,
        messages: &'a [Value],
        tools: &'a [Value],
        delta: &'a mut (dyn FnMut(String) + Send),
    ) -> BoxFuture<'a, Result<ModelTurn, ModelError>>;
}
pub struct HttpModel {
    client: reqwest::Client,
    endpoint: String,
    key: Option<String>,
    model: String,
    budget: crate::context::ContextBudget,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    text_tool_calls: bool,
}
impl HttpModel {
    pub fn new(endpoint: &str, model: &str, key: Option<String>) -> Result<Self, ModelError> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| ModelError::Network)?,
            endpoint: normalize_endpoint(endpoint).map_err(|_| ModelError::Invalid)?,
            key,
            model: model.into(),
            budget: Default::default(),
            temperature: None,
            max_tokens: None,
            text_tool_calls: false,
        })
    }
}
impl HttpModel {
    pub fn with_sampling(mut self, temperature: Option<f32>, max_tokens: Option<u32>) -> Self {
        self.temperature = temperature;
        self.max_tokens = max_tokens;
        self
    }
    /// Read `<tool_call>` blocks from replies with no structured calls. Off by default.
    pub fn with_text_tool_calls(mut self, enabled: bool) -> Self {
        self.text_tool_calls = enabled;
        self
    }
    pub fn with_budget(
        mut self,
        budget: crate::context::ContextBudget,
    ) -> Result<Self, ModelError> {
        budget.validate().map_err(|_| ModelError::Limit)?;
        self.budget = budget;
        Ok(self)
    }
}
impl ModelBackend for HttpModel {
    fn context_budget(&self) -> crate::context::ContextBudget {
        self.budget
    }
    fn model_name(&self) -> Option<&str> {
        Some(&self.model)
    }
    fn server_address(&self) -> Option<&str> {
        Some(&self.endpoint)
    }
    fn generate<'a>(
        &'a self,
        messages: &'a [Value],
        tools: &'a [Value],
        delta: &'a mut (dyn FnMut(String) + Send),
    ) -> BoxFuture<'a, Result<ModelTurn, ModelError>> {
        Box::pin(async move {
            if self
                .max_tokens
                .is_some_and(|n| n == 0 || n > self.budget.output_tokens)
                || self.temperature.is_some_and(|t| !(0.0..=2.0).contains(&t))
            {
                return Err(ModelError::Invalid);
            }
            self.budget
                .check(messages, tools)
                .map_err(|_| ModelError::Limit)?;
            let mut payload = json!({"model":self.model,"messages":one_system_message(messages),"stream":true,"stream_options":{"include_usage":true},"max_tokens":self.max_tokens.unwrap_or(self.budget.output_tokens)});
            if let Some(temperature) = self.temperature {
                payload["temperature"] = json!(temperature);
            }
            if !tools.is_empty() {
                payload["tools"] = json!(tools);
            }
            let body = serde_json::to_vec(&payload).map_err(|_| ModelError::Invalid)?;
            if body.len() > 48 * 1024 * 1024 {
                return Err(ModelError::Limit);
            }
            let mut request = self
                .client
                .post(format!("{}/chat/completions", self.endpoint))
                .header("Content-Type", "application/json")
                .timeout(Duration::from_secs(30 * 60))
                .body(body);
            if let Some(key) = &self.key {
                request = request.bearer_auth(key);
            }
            let response = request.send().await.map_err(|_| ModelError::Network)?;
            if !response.status().is_success() {
                let status = response.status().as_u16();
                return Err(ModelError::Http(
                    status,
                    response_error_detail(&self.endpoint, response, self.key.as_deref()).await,
                ));
            }
            let mut stream = response.bytes_stream();
            let mut decoder = if self.text_tool_calls && !tools.is_empty() {
                StreamDecoder::with_text_tools(
                    tools
                        .iter()
                        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                        .collect(),
                )
            } else {
                StreamDecoder::default()
            };
            while let Some(chunk) = tokio::time::timeout(Duration::from_secs(180), stream.next())
                .await
                .map_err(|_| ModelError::Network)?
            {
                decoder.push(&chunk.map_err(|_| ModelError::Network)?, delta)?;
                if decoder.done {
                    break;
                }
            }
            decoder.finish(delta)
        })
    }
}
#[derive(Default)]
pub struct StreamDecoder {
    pending: Vec<u8>,
    received: usize,
    turn: ModelTurn,
    calls: BTreeMap<usize, ToolCall>,
    done: bool,
    finished: bool,
    think: ThinkFilter,
    /// When reasoning started and when its latest part arrived.
    thinking: Option<(std::time::Instant, std::time::Instant)>,
    /// Offered tool names, when calls written as text are accepted.
    offered: Option<Vec<String>>,
    scan: text_tools::TagScan,
    /// Bytes of `turn.content` already streamed to the caller.
    emitted: usize,
}
impl StreamDecoder {
    /// Also accept `<tool_call>` blocks for `offered` tools. Text from the first block on is
    /// held back until the reply ends, so a call is never shown as answer text.
    pub fn with_text_tools(offered: Vec<String>) -> Self {
        Self {
            offered: Some(offered),
            ..Default::default()
        }
    }
    pub fn push(
        &mut self,
        bytes: &[u8],
        delta: &mut (dyn FnMut(String) + Send),
    ) -> Result<(), ModelError> {
        self.received += bytes.len();
        if self.received > MAX_STREAM {
            return Err(ModelError::Limit);
        }
        for byte in bytes {
            self.pending.push(*byte);
            if self.pending.len() > 1024 * 1024 {
                return Err(ModelError::Limit);
            }
            if self.pending.ends_with(b"\n\n") || self.pending.ends_with(b"\r\n\r\n") {
                let frame = std::mem::take(&mut self.pending);
                self.frame(&frame, delta)?;
                if self.done {
                    break;
                }
            }
        }
        Ok(())
    }
    fn frame(
        &mut self,
        bytes: &[u8],
        delta: &mut (dyn FnMut(String) + Send),
    ) -> Result<(), ModelError> {
        let frame = std::str::from_utf8(bytes).map_err(|_| ModelError::Invalid)?;
        let data = frame
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            return Ok(());
        }
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let value: Value = serde_json::from_str(&data).map_err(|_| ModelError::Invalid)?;
        if value.get("error").is_some() {
            return Err(ModelError::Invalid);
        }
        if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
            self.turn.usage = serde_json::from_value(usage.clone()).ok();
        }
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
        else {
            return Ok(());
        };
        if choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .is_some()
        {
            self.finished = true;
        }
        let update = &choice["delta"];
        // Servers name the separate reasoning field differently.
        for field in ["reasoning_content", "reasoning"] {
            if let Some(text) = update.get(field).and_then(Value::as_str) {
                self.reason(text)?;
            }
        }
        if let Some(text) = update.get("content").and_then(Value::as_str) {
            let (answer, reasoning) = self.think.push(text);
            self.reason(&reasoning)?;
            self.answer(answer, delta);
        }
        if let Some(calls) = update.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or(ModelError::Invalid)? as usize;
                if index >= 8 {
                    return Err(ModelError::Limit);
                }
                let entry = self.calls.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    entry.id.push_str(id);
                }
                if let Some(name) = call["function"].get("name").and_then(Value::as_str) {
                    entry.function.name.push_str(name);
                }
                if let Some(arguments) = call["function"].get("arguments").and_then(Value::as_str) {
                    entry.function.arguments.push_str(arguments);
                }
                if entry.id.len() > 128
                    || entry.function.name.len() > 80
                    || entry.function.arguments.len() > 256 * 1024
                {
                    return Err(ModelError::Limit);
                }
            }
        }
        Ok(())
    }
    /// Keep reasoning out of the answer, within the same limit as the whole stream.
    fn reason(&mut self, text: &str) -> Result<(), ModelError> {
        if text.is_empty() {
            return Ok(());
        }
        if self.turn.reasoning.len() + text.len() > MAX_STREAM {
            return Err(ModelError::Limit);
        }
        self.turn.reasoning.push_str(text);
        let now = std::time::Instant::now();
        let started = self.thinking.map_or(now, |(started, _)| started);
        self.thinking = Some((started, now));
        Ok(())
    }
    fn answer(&mut self, text: String, delta: &mut (dyn FnMut(String) + Send)) {
        self.turn.content.push_str(&text);
        let shown = match self.offered {
            Some(_) => self.scan.shown(&self.turn.content),
            None => self.turn.content.len(),
        };
        self.show(shown, delta);
    }
    fn show(&mut self, upto: usize, delta: &mut (dyn FnMut(String) + Send)) {
        if let Some(text) = self.turn.content.get(self.emitted..upto) {
            if !text.is_empty() {
                delta(text.to_owned());
                self.emitted = upto;
            }
        }
    }
    pub fn finish(
        mut self,
        delta: &mut (dyn FnMut(String) + Send),
    ) -> Result<ModelTurn, ModelError> {
        if !self.pending.is_empty() && !self.done {
            let frame = std::mem::take(&mut self.pending);
            self.frame(&frame, delta)?;
        }
        if !self.done && !self.finished {
            return Err(ModelError::Invalid);
        }
        let (answer, reasoning) = std::mem::take(&mut self.think).finish();
        self.reason(&reasoning)?;
        self.answer(answer, delta);
        self.turn.reasoning = self.turn.reasoning.trim().to_owned();
        self.turn.reasoning_ms = self.thinking.map_or(0, |(started, last)| {
            u64::try_from(last.duration_since(started).as_millis()).unwrap_or(u64::MAX)
        });
        self.turn.calls = std::mem::take(&mut self.calls).into_values().collect();
        // Text is read for calls only when the reply sent none of its own.
        if let Some(offered) = self.offered.take().filter(|_| self.turn.calls.is_empty()) {
            if let Some((calls, remaining)) = parse_text_tool_calls(&self.turn.content, &offered) {
                self.turn.calls = calls;
                self.turn.content = remaining;
                self.turn.text_calls = true;
            }
        }
        self.show(self.turn.content.len(), delta);
        let mut ids = std::collections::HashSet::new();
        for call in &self.turn.calls {
            if call.id.is_empty()
                || call.function.name.is_empty()
                || !ids.insert(&call.id)
                || serde_json::from_str::<Value>(&call.function.arguments).is_err()
            {
                return Err(ModelError::Invalid);
            }
        }
        Ok(self.turn)
    }
}

/// Send every text system message as one, first. Chat templates such as Qwen's reject a system
/// message anywhere but first, so Blackwall's separate instruction blocks go out as one.
fn one_system_message(messages: &[Value]) -> Vec<Value> {
    let (system, rest): (Vec<_>, Vec<_>) = messages
        .iter()
        .partition(|m| m["role"] == "system" && m["content"].is_string());
    if system.is_empty() {
        return messages.to_vec();
    }
    let content = system
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    std::iter::once(json!({"role":"system","content":content}))
        .chain(rest.into_iter().cloned())
        .collect()
}

/// The one model form of a saved chat message: its text with text and binary attachments
/// inlined, and its images as parts. `extra` are attachments sent with the request rather than
/// the message (`ChatRequest::attachments`, for the newest user message). `messages` sends this
/// for every message, and stored model rows that reference a user message resolve to it.
pub fn message_form(message: &ChatMessage, extra: &[Attachment]) -> Value {
    let mut text = message.content.clone();
    let mut images = vec![];
    for attachment in message.attachments.iter().chain(extra) {
        if let Some(url) = attachment
            .data_url
            .as_ref()
            .filter(|_| attachment.mime_type.starts_with("image/"))
        {
            images.push(json!({"type":"image_url","image_url":{"url":url}}));
        } else if let Some(content) = &attachment.text_content {
            text.push_str(&format!(
                "\n<attachment name={:?}>\n{}\n</attachment>",
                attachment.name, content
            ));
        } else {
            text.push_str(&format!(
                "\n[Attached file: {}. Binary contents are not available.]",
                attachment.name
            ));
        }
    }
    if images.is_empty() {
        json!({"role":message.role,"content":text})
    } else {
        images.insert(0, json!({"type":"text","text":text}));
        json!({"role":message.role,"content":images})
    }
}

pub fn messages(request: &ChatRequest) -> Vec<Value> {
    let last_user = request
        .messages
        .iter()
        .rposition(|m| m.role == MessageRole::User);
    request
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let extra = if Some(index) == last_user {
                request.attachments.as_slice()
            } else {
                &[]
            };
            message_form(message, extra)
        })
        .collect()
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn message_form_is_what_messages_sends() {
        let extra = [Attachment {
            id: "extra".into(),
            name: "extra.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: 1,
            data_url: None,
            text_content: Some("E".into()),
        }];
        let mut seen = 0;
        for session in crate::storage::v5_chats() {
            for row in session["messages"].as_array().unwrap() {
                let message: ChatMessage = serde_json::from_value(row.clone()).unwrap();
                let mut request = ChatRequest::new("r", "m", vec![message.clone()]);
                assert_eq!(message_form(&message, &[]), messages(&request)[0], "{row}");
                if message.role == MessageRole::User {
                    seen += 1;
                    request.attachments = extra.to_vec();
                    assert_eq!(
                        message_form(&message, &extra),
                        messages(&request)[0],
                        "{row}"
                    );
                }
            }
        }
        assert!(seen > 20, "{seen}");
    }
    #[test]
    fn messages_output_is_pinned() {
        let attachment =
            |id: &str, mime: &str, data: Option<&str>, text: Option<&str>| Attachment {
                id: id.into(),
                name: format!("{id}.file"),
                mime_type: mime.into(),
                size_bytes: 3,
                data_url: data.map(Into::into),
                text_content: text.map(Into::into),
            };
        let mut first = ChatMessage::new(MessageRole::User, "First");
        first.attachments = vec![
            attachment("notes", "text/plain", None, Some("a \"quoted\" line")),
            attachment("blob", "application/octet-stream", None, None),
        ];
        let mut last = ChatMessage::new(MessageRole::User, "Last");
        last.attachments = vec![attachment(
            "pic",
            "image/png",
            Some("data:image/png;base64,AA"),
            None,
        )];
        let mut request = ChatRequest::new(
            "r",
            "m",
            vec![
                ChatMessage::new(MessageRole::System, "Sys"),
                first,
                ChatMessage::new(MessageRole::Assistant, "Reply"),
                last,
                ChatMessage::new(MessageRole::Assistant, "Tail"),
            ],
        );
        request.attachments = vec![
            attachment("extra", "text/plain", None, Some("x")),
            attachment("img", "image/jpeg", None, Some("not an image url")),
        ];
        assert_eq!(
            messages(&request),
            [
                json!({"role":"system","content":"Sys"}),
                json!({"role":"user","content":"First\n<attachment name=\"notes.file\">\na \"quoted\" line\n</attachment>\n[Attached file: blob.file. Binary contents are not available.]"}),
                json!({"role":"assistant","content":"Reply"}),
                json!({"role":"user","content":[
                    {"type":"text","text":"Last\n<attachment name=\"extra.file\">\nx\n</attachment>\n<attachment name=\"img.file\">\nnot an image url\n</attachment>"},
                    {"type":"image_url","image_url":{"url":"data:image/png;base64,AA"}},
                ]}),
                json!({"role":"assistant","content":"Tail"}),
            ]
        );
    }
    #[test]
    fn fragmented_tool_calls_and_unicode_survive_chunk_boundaries() {
        let frames = [
            json!({"choices":[{"delta":{"content":"Hi 🦀","tool_calls":[{"index":0,"id":"call1","function":{"name":"read_file","arguments":"{\"pa"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]},"finish_reason":"tool_calls"}]}),
        ];
        let bytes = format!(
            "data: {}\r\n\r\ndata: {}\n\ndata: [DONE]\n\n",
            frames[0], frames[1]
        );
        let mut decoder = StreamDecoder::default();
        let mut text = String::new();
        for chunk in bytes.as_bytes().chunks(3) {
            decoder
                .push(chunk, &mut |delta| text.push_str(&delta))
                .unwrap();
        }
        let turn = decoder.finish(&mut |_| {}).unwrap();
        assert_eq!(text, "Hi 🦀");
        assert_eq!(turn.calls[0].function.arguments, "{\"path\":\"a\"}");
    }
    #[test]
    fn system_messages_are_sent_as_one_first() {
        let messages = [
            json!({"role":"system","content":"Base"}),
            json!({"role":"system","content":"Guidance"}),
            json!({"role":"user","content":"Hi"}),
            json!({"role":"system","content":"Later"}),
        ];
        let sent = one_system_message(&messages);
        assert_eq!(
            sent,
            [
                json!({"role":"system","content":"Base\n\nGuidance\n\nLater"}),
                messages[2].clone()
            ]
        );
        let single = [messages[0].clone(), messages[2].clone()];
        assert_eq!(one_system_message(&single), single);
        assert!(one_system_message(&[]).is_empty());
    }
    fn decode(frames: &[Value]) -> (String, ModelTurn) {
        let mut bytes = String::new();
        for frame in frames {
            bytes.push_str(&format!("data: {frame}\n\n"));
        }
        bytes.push_str("data: [DONE]\n\n");
        let mut decoder = StreamDecoder::default();
        let mut text = String::new();
        for chunk in bytes.as_bytes().chunks(5) {
            decoder
                .push(chunk, &mut |delta| text.push_str(&delta))
                .unwrap();
        }
        let turn = decoder.finish(&mut |delta| text.push_str(&delta)).unwrap();
        (text, turn)
    }
    #[test]
    fn reasoning_content_field_is_separate() {
        let (shown, turn) = decode(&[
            json!({"choices":[{"delta":{"reasoning_content":"llama.cpp thinks. "}}]}),
            json!({"choices":[{"delta":{"reasoning":"Ollama thinks."}}]}),
            json!({"choices":[{"delta":{"content":"<think>inline</think>\n\nAnswer"}}]}),
            json!({"choices":[{"delta":{"content":" here."},"finish_reason":"stop"}]}),
        ]);
        assert_eq!(shown, "Answer here.");
        assert_eq!(turn.content, "Answer here.");
        assert_eq!(turn.reasoning, "llama.cpp thinks. Ollama thinks.inline");
    }
    #[test]
    fn unclosed_think_never_reaches_the_answer() {
        let (shown, turn) = decode(&[
            json!({"choices":[{"delta":{"content":"<think>still going"},"finish_reason":"length"}]}),
        ]);
        assert_eq!(shown, "");
        assert_eq!(turn.content, "");
        assert_eq!(turn.reasoning, "still going");
    }
    #[test]
    fn reasoning_is_bounded_like_content() {
        let mut decoder = StreamDecoder::default();
        let part = "r".repeat(512 * 1024);
        let frame = format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":{"reasoning_content":part}}]})
        );
        let mut result = Ok(());
        for _ in 0..20 {
            result = decoder.push(frame.as_bytes(), &mut |_| {});
            if result.is_err() {
                break;
            }
        }
        assert!(matches!(result, Err(ModelError::Limit)));
    }
    #[test]
    fn reasoning_reaches_traces_only_with_content_capture() {
        use crate::telemetry::{Mode, ModelReply, RunOutcome, Telemetry, TelemetryOptions};
        for content in [false, true] {
            let directory = std::env::temp_dir().join(format!(
                "blackwall-reasoning-trace-{}",
                rand::random::<u64>()
            ));
            std::fs::create_dir_all(&directory).unwrap();
            let telemetry = Telemetry::start(
                &directory,
                TelemetryOptions {
                    traces: true,
                    content,
                    ..Default::default()
                },
            );
            let trace = telemetry.run("thinking", Mode::Agent);
            let call = trace.begin_model_call(&trace.root(), "m");
            let turn = ModelTurn {
                content: "ANSWER-SENTINEL".into(),
                reasoning: "REASONING-SENTINEL".into(),
                ..Default::default()
            };
            trace.end_model_call(call, Mode::Agent, "", Ok(ModelReply::of(&turn)));
            trace.finish(RunOutcome::Complete);
            telemetry.flush();
            drop(telemetry);
            let id = crate::trace::list(&directory, 1).remove(0).id;
            let body = std::fs::read_to_string(
                directory
                    .join(crate::trace::TRACE_DIRECTORY)
                    .join(format!("{id}.json")),
            )
            .unwrap();
            assert_eq!(body.contains("REASONING-SENTINEL"), content, "{body}");
            assert_eq!(body.contains("ANSWER-SENTINEL"), content, "{body}");
            if content {
                assert!(body.contains(r#"\"type\":\"reasoning\""#), "{body}");
            }
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
    fn decode_with(decoder: StreamDecoder, frames: &[Value]) -> (String, ModelTurn) {
        let mut bytes: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
        bytes.push_str("data: [DONE]\n\n");
        let mut decoder = decoder;
        let mut text = String::new();
        for chunk in bytes.as_bytes().chunks(4) {
            decoder
                .push(chunk, &mut |delta| text.push_str(&delta))
                .unwrap();
        }
        let turn = decoder.finish(&mut |delta| text.push_str(&delta)).unwrap();
        (text, turn)
    }
    const BLOCK: &str = r#"<tool_call>{"name":"read_file","arguments":{"path":"a"}}</tool_call>"#;
    #[test]
    fn text_tool_call_off_by_default() {
        let frames = [
            json!({"choices":[{"delta":{"content":format!("Reading.\n{BLOCK}")},"finish_reason":"stop"}]}),
        ];
        let (shown, turn) = decode_with(StreamDecoder::default(), &frames);
        assert!(turn.calls.is_empty() && !turn.text_calls);
        assert_eq!(shown, format!("Reading.\n{BLOCK}"));
        assert!(
            !HttpModel::new("http://127.0.0.1:1/v1", "m", None)
                .unwrap()
                .text_tool_calls
        );
        // Turned on, the same reply is a call and the block is never shown.
        let (shown, turn) = decode_with(
            StreamDecoder::with_text_tools(vec!["read_file".into()]),
            &frames,
        );
        assert!(turn.text_calls);
        assert_eq!(turn.calls[0].function.name, "read_file");
        assert_eq!(
            (shown.as_str(), turn.content.as_str()),
            ("Reading.\n", "Reading.")
        );
    }
    #[test]
    fn structured_tool_calls_disable_text_parsing() {
        let (shown, turn) = decode_with(
            StreamDecoder::with_text_tools(vec!["read_file".into(), "list_files".into()]),
            &[
                json!({"choices":[{"delta":{"content":BLOCK}}]}),
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"list_files","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
            ],
        );
        assert!(!turn.text_calls);
        assert_eq!(turn.calls.len(), 1);
        assert_eq!(turn.calls[0].function.name, "list_files");
        assert_eq!(turn.content, BLOCK);
        assert_eq!(shown, BLOCK);
    }
    #[test]
    fn unmatched_text_tool_call_is_shown_once_at_the_end() {
        let reply = r#"Try <tool_call>{"name":"shell","arguments":{}}</tool_call> later."#;
        let (shown, turn) = decode_with(
            StreamDecoder::with_text_tools(vec!["read_file".into()]),
            &[json!({"choices":[{"delta":{"content":reply},"finish_reason":"stop"}]})],
        );
        assert!(turn.calls.is_empty());
        assert_eq!((shown.as_str(), turn.content.as_str()), (reply, reply));
    }
    #[test]
    fn reasoning_is_bounded_across_a_run() {
        let mut run = Reasoning::default();
        let turn = |text: String| ModelTurn {
            reasoning: text,
            reasoning_ms: 1000,
            ..Default::default()
        };
        // Many calls, each under the per-call limit, add up past the run's limit.
        for _ in 0..10 {
            run.add(&turn("é".repeat(40 * 1024)));
        }
        let (text, ms) = run.take();
        let text = text.unwrap();
        assert!(
            text.len() <= MAX_RUN_REASONING + TRUNCATED.len(),
            "{}",
            text.len()
        );
        assert!(text.ends_with("[Reasoning truncated]"));
        assert_eq!(text.matches("[Reasoning truncated]").count(), 1);
        assert_eq!(ms, Some(10_000));
        let mut short = Reasoning::default();
        short.add(&turn("a".into()));
        short.add(&turn("b".into()));
        assert_eq!(short.take().0.as_deref(), Some("a\n\nb"));
    }
    #[test]
    fn truncated_stream_is_an_error() {
        let mut decoder = StreamDecoder::default();
        decoder
            .push(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"Partial\"}}]}\n\n",
                &mut |_| {},
            )
            .unwrap();
        assert!(decoder.finish(&mut |_| {}).is_err());
    }
    #[test]
    fn server_error_details_read_every_common_shape() {
        let memory = "model requires more system memory (24.1 GiB) than is available (12.0 GiB)";
        for body in [
            json!({ "error": memory }).to_string(),
            json!({"error": {"code": 500, "message": memory, "type": "server_error"}}).to_string(),
            json!({"object": "error", "message": memory, "type": "BadRequestError", "code": 400})
                .to_string(),
            json!({"error": {"message": memory, "type": "invalid_request_error", "param": null}})
                .to_string(),
            json!({ "detail": memory }).to_string(),
            format!("  {memory}\n"),
        ] {
            assert_eq!(
                server_error_detail(body.as_bytes()).as_deref(),
                Some(memory),
                "{body}"
            );
        }
        for silent in [
            "",
            "   ",
            "<html><body>502 Bad Gateway</body></html>",
            "{}",
            r#"{"error": {"code": 1}}"#,
        ] {
            assert_eq!(server_error_detail(silent.as_bytes()), None, "{silent}");
        }
    }
    #[test]
    fn server_error_details_are_clipped_cleaned_and_redacted() {
        let huge = json!({ "error": "x ".repeat(100_000) }).to_string();
        let detail = server_error_detail(huge.as_bytes());
        // Past the 64 KiB read limit the JSON is cut, so the text is shown clipped.
        let detail = detail.unwrap();
        assert!(detail.chars().count() <= 300 && detail.ends_with('…'));
        let long = json!({ "error": "word ".repeat(200) }).to_string();
        let detail = server_error_detail(long.as_bytes()).unwrap();
        assert_eq!(detail.chars().count(), 300);
        assert!(detail.ends_with('…'));
        let controls = json!({"error": "bad\u{1b}[31m model\r\nname\u{0}\t here"}).to_string();
        assert_eq!(
            server_error_detail(controls.as_bytes()).unwrap(),
            "bad [31m model name here"
        );
        let secret = json!({"error": "invalid api key sk-abc123 for https://user:pw@host/v1?api_key=hunter2 Authorization: Bearer abcdef0123456789abcdef0123456789ab"}).to_string();
        let detail = server_error_detail(secret.as_bytes()).unwrap();
        for leaked in ["sk-abc123", "hunter2", "user:pw", "abcdef0123456789"] {
            assert!(!detail.contains(leaked), "{detail}");
        }
        assert!(detail.starts_with("invalid api key [redacted]"), "{detail}");
    }
    #[tokio::test]
    async fn http_errors_never_echo_the_request_key() {
        let key = "review-example-0123456789";
        let fake = crate::test_http::serve(move |method, path, _| match (method, path) {
            ("POST", "/v1/chat/completions") => Some((
                401,
                json!({"error": {"message": format!("Rejected value '{key}'. Also 'sk-quoted-abc'.")}})
                    .to_string(),
            )),
            _ => None,
        })
        .await;
        let endpoint = format!("{}/v1", fake.origin());
        let model = HttpModel::new(&endpoint, "m", Some(key.to_owned())).unwrap();
        let error = model
            .generate(
                &[json!({"role": "user", "content": "hi"})],
                &[],
                &mut |_| {},
            )
            .await
            .err()
            .unwrap();
        let shown = error.to_string();
        assert!(!shown.contains(key), "{shown}");
        assert!(!shown.contains("sk-quoted"), "{shown}");
        assert!(shown.contains("Rejected value '[redacted]'"), "{shown}");
    }
    #[tokio::test]
    async fn http_errors_carry_what_the_server_said() {
        let fake = crate::test_http::serve(|method, path, _| match (method, path) {
            ("POST", "/v1/chat/completions") => Some((
                500,
                json!({"error": "model requires more system memory (24 GiB) than is available (12 GiB)"})
                    .to_string(),
            )),
            ("GET", "/props") => Some((
                200,
                json!({"default_generation_settings": {"n_ctx": 4096}}).to_string(),
            )),
            _ => None,
        })
        .await;
        let endpoint = format!("{}/v1", fake.origin());
        let model = HttpModel::new(&endpoint, "big", None).unwrap();
        let error = model
            .generate(
                &[json!({"role": "user", "content": "hi"})],
                &[],
                &mut |_| {},
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), "http");
        assert!(matches!(error, ModelError::Http(500, Some(_))));
        assert_eq!(
            error.to_string(),
            "The model service returned HTTP 500. model requires more system memory (24 GiB) than is available (12 GiB)"
        );
        // Once the session knows the server, the detail names it.
        crate::connection::probe_endpoint(&endpoint, "big").await;
        let error = model
            .generate(
                &[json!({"role": "user", "content": "hi"})],
                &[],
                &mut |_| {},
            )
            .await
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .ends_with("HTTP 500. llama.cpp: model requires more system memory (24 GiB) than is available (12 GiB)"),
            "{error}"
        );
        assert_eq!(
            ModelError::Http(404, None).to_string(),
            "The model service rejected this request (HTTP 404). Check the model and access key."
        );
    }
}
