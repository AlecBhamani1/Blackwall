//! Headless streaming model transport, including fragmented OpenAI-compatible tool calls.
use crate::{
    connection::normalize_endpoint,
    protocol::{ChatRequest, MessageRole, TokenUsage},
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
    #[error("The model service rejected this request (HTTP {0}). Check the model and access key.")]
    Http(u16),
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
            Self::Http(_) => "http",
            Self::Invalid => "invalid_response",
            Self::Limit => "limit",
        }
    }
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
                return Err(ModelError::Http(response.status().as_u16()));
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
            let attachments = message.attachments.iter().chain(
                request
                    .attachments
                    .iter()
                    .filter(|_| Some(index) == last_user),
            );
            let mut text = message.content.clone();
            let mut images = vec![];
            for attachment in attachments {
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
        })
        .collect()
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
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
}
