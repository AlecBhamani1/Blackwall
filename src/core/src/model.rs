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
}
pub trait ModelBackend: Send + Sync {
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
        })
    }
}
impl ModelBackend for HttpModel {
    fn generate<'a>(
        &'a self,
        messages: &'a [Value],
        tools: &'a [Value],
        delta: &'a mut (dyn FnMut(String) + Send),
    ) -> BoxFuture<'a, Result<ModelTurn, ModelError>> {
        Box::pin(async move {
            let mut payload = json!({"model":self.model,"messages":messages,"stream":true,"stream_options":{"include_usage":true}});
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
            let mut decoder = StreamDecoder::default();
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
}
impl StreamDecoder {
    pub fn push(
        &mut self,
        bytes: &[u8],
        delta: &mut (dyn FnMut(String) + Send),
    ) -> Result<(), ModelError> {
        self.received += bytes.len();
        if self.received > 8 * 1024 * 1024 {
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
        if let Some(text) = update.get("content").and_then(Value::as_str) {
            self.turn.content.push_str(text);
            delta(text.into());
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
        self.turn.calls = self.calls.into_values().collect();
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
