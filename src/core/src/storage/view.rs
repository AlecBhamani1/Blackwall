//! Rebuilds a v6 chat from its `chat_events` rows.
//!
//! Each value is stored once. A row that repeats another holds a reference instead:
//! - a `model` row `{"ref": seq}` is the model form of the UI message at `seq`;
//! - an assistant's `content` `{"turns": [seq…], "tail"?}` is its model turns' text, joined the
//!   way the agent joins them, then `tail`;
//! - an assistant tool `{…, "call": [seq, index], "result": seq|null}` takes its `name` and
//!   `arguments` from that model call, and its `output` from the result row.

use super::StorageError;
use crate::{model::message_form, protocol::ChatMessage};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

const UNREADABLE: &str =
    "This conversation's saved history can't be read. Your existing data has been left in place.";

/// A `chat_events.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    UiUser,
    UiAssistant,
    Model,
}

impl Kind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::UiUser => "ui_user",
            Self::UiAssistant => "ui_assistant",
            Self::Model => "model",
        }
    }

    pub(crate) fn parse(name: &str) -> Result<Self, StorageError> {
        match name {
            "ui_user" => Ok(Self::UiUser),
            "ui_assistant" => Ok(Self::UiAssistant),
            "model" => Ok(Self::Model),
            _ => Err(unreadable()),
        }
    }
}

/// One `chat_events` row, its body parsed.
pub(crate) struct EventRow {
    pub seq: i64,
    pub kind: Kind,
    pub body: Value,
}

/// The `chat_context` row: the transcript is the first `model_len` model rows from seq
/// `model_from`, and `state` is the rest of the `contextState`.
pub(crate) struct ContextRow {
    pub model_from: i64,
    pub model_len: usize,
    pub state: Map<String, Value>,
}

fn unreadable() -> StorageError {
    StorageError::Invalid(UNREADABLE)
}

/// The UI messages in seq order, each with its seq, and the `contextState`.
pub(crate) type ChatView = (Vec<(i64, Value)>, Option<Value>);

pub(crate) fn build_view(
    rows: &[EventRow],
    context: Option<ContextRow>,
) -> Result<ChatView, StorageError> {
    let by_seq: BTreeMap<i64, &EventRow> = rows.iter().map(|row| (row.seq, row)).collect();
    let mut messages = vec![];
    for row in rows {
        match row.kind {
            Kind::UiUser => messages.push((row.seq, row.body.clone())),
            Kind::UiAssistant => messages.push((row.seq, assistant(&by_seq, &row.body)?)),
            Kind::Model => {}
        }
    }
    let context = context
        .map(|context| {
            let transcript = by_seq
                .range(context.model_from..)
                .filter(|(_, row)| row.kind == Kind::Model)
                .take(context.model_len)
                .map(|(_, row)| model(&by_seq, &row.body))
                .collect::<Result<Vec<_>, _>>()?;
            if transcript.len() < context.model_len {
                return Err(unreadable());
            }
            let mut state = context.state;
            state.insert("transcript".into(), Value::Array(transcript));
            Ok(Value::Object(state))
        })
        .transpose()?;
    Ok((messages, context))
}

/// The body of the row at `seq`, which must be of `kind`.
fn target<'a>(
    rows: &BTreeMap<i64, &'a EventRow>,
    seq: &Value,
    kind: Kind,
) -> Result<&'a Value, StorageError> {
    seq.as_i64()
        .and_then(|seq| rows.get(&seq))
        .filter(|row| row.kind == kind)
        .map(|row| &row.body)
        .ok_or_else(unreadable)
}

/// A `model` row as a transcript entry.
fn model(rows: &BTreeMap<i64, &EventRow>, body: &Value) -> Result<Value, StorageError> {
    let Some(seq) = body.get("ref") else {
        return Ok(body.clone());
    };
    if let Ok(user) = target(rows, seq, Kind::UiUser) {
        let message: ChatMessage =
            serde_json::from_value(user.clone()).map_err(|_| unreadable())?;
        return Ok(message_form(&message, &[]));
    }
    let message = assistant(rows, target(rows, seq, Kind::UiAssistant)?)?;
    Ok(json!({"role": "assistant", "content": message["content"]}))
}

/// A `ui_assistant` row as the UI message.
fn assistant(rows: &BTreeMap<i64, &EventRow>, body: &Value) -> Result<Value, StorageError> {
    let mut message = body.clone();
    if let Some(form) = body.get("content").filter(|content| content.is_object()) {
        let turns = form["turns"].as_array().ok_or_else(unreadable)?;
        let texts = turns
            .iter()
            .map(|seq| {
                target(rows, seq, Kind::Model)?["content"]
                    .as_str()
                    .ok_or_else(unreadable)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let tail = form.get("tail").and_then(Value::as_str).unwrap_or_default();
        message["content"] = json!(accumulate(texts) + tail);
    }
    if let Some(Value::Array(tools)) = message.get_mut("tools") {
        for tool in tools.iter_mut().filter(|tool| tool.get("call").is_some()) {
            *tool = resolve_tool(rows, tool)?;
        }
    }
    Ok(message)
}

/// A tool `{…, "call": [seq, index], "result": seq|null}` with its name, arguments and output.
fn resolve_tool(rows: &BTreeMap<i64, &EventRow>, residual: &Value) -> Result<Value, StorageError> {
    let mut tool = residual.as_object().cloned().ok_or_else(unreadable)?;
    let call = tool.remove("call").unwrap_or_default();
    let result = tool.remove("result").unwrap_or_default();
    let index = call[1].as_u64().ok_or_else(unreadable)?;
    let called = target(rows, &call[0], Kind::Model)?["tool_calls"]
        .get(usize::try_from(index).map_err(|_| unreadable())?)
        .ok_or_else(unreadable)?;
    tool.insert("name".into(), called["function"]["name"].clone());
    tool.insert("arguments".into(), called["function"]["arguments"].clone());
    if !result.is_null() {
        let output = target(rows, &result, Kind::Model)?;
        tool.insert("output".into(), output["content"].clone());
    }
    Ok(Value::Object(tool))
}

/// The screen text of a run's turns: the agent and chat loops add `"\n\n"` before every later
/// turn while the text so far is nonempty, empty turns included (`agent.rs`, `context.rs`).
pub(crate) fn accumulate<'a>(turns: impl IntoIterator<Item = &'a str>) -> String {
    let mut text = String::new();
    for (i, turn) in turns.into_iter().enumerate() {
        if i > 0 && !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(turn);
    }
    text
}
