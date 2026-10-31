//! The v6 load path: rebuilds the chat screen and the model's `ContextState` from a chat's
//! add-only `chat_events` log.
#![cfg_attr(not(test), allow(dead_code))] // the session API that calls it lands later

use crate::{
    model::message_form,
    protocol::ChatMessage,
    run_fold::{accumulate, AssistantShape, RunFold},
};
use serde_json::{json, Map, Value};
use std::{cell::Cell, collections::BTreeMap};

/// Most bytes one load may build by following references (security L5): assistant content
/// (inline or from turns), tool names, arguments and outputs, and the model forms behind `model`
/// refs. Stored bytes are
/// capped at [`super::MAX_SESSION_BYTES`] when written, but a reference costs a few bytes and can
/// name a large row any number of times, so a tampered `{"turns":[s,s,s,…]}` could otherwise ask
/// for terabytes. A valid chat builds each stored byte at most twice (once for the screen, and
/// once more for an assistant echo ref in the transcript, or for a tool's arguments as its call
/// and then its result arrive), so twice the session cap never refuses one.
const MAX_RESOLVED_BYTES: u64 = 2 * super::MAX_SESSION_BYTES;

/// A `chat_events.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    UiUser,
    Model,
    UiProgress,
    UiAssistant,
}

/// One `chat_events` row, its body parsed.
#[derive(Clone, Debug)]
pub(crate) struct EventRow {
    pub seq: i64,
    pub kind: Kind,
    pub body: Value,
}

/// The `chat_context` row.
#[derive(Clone, Debug)]
pub(crate) struct ContextRow {
    pub model_from: i64,
    pub model_len: u64,
    pub covered_messages: u64,
    pub source_hash: String,
    pub server_usage: Option<String>,
}

/// A run with no terminal row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpenRun {
    pub start: i64,
    pub assistant_id: String,
    pub user_seq: i64,
    pub message_index: u64,
    pub turn_seqs: Vec<i64>,
}

/// The loaded chat.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChatView {
    pub messages: Vec<Value>,
    /// The `contextState` JSON.
    pub context: Option<Value>,
    pub open_run: Option<OpenRun>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ViewError {
    #[error("A saved chat entry ({seq}) refers to an entry that is missing or of the wrong kind.")]
    BadRef { seq: i64 },
    #[error("A saved chat entry ({seq}) is not in a form Blackwall can read.")]
    BadBody { seq: i64 },
    #[error("The saved model history is shorter than its context says.")]
    ContextLength,
    #[error("The saved context usage is not in a form Blackwall can read.")]
    BadContext,
    /// Its references would build more than [`MAX_RESOLVED_BYTES`].
    #[error("A saved chat refers to more data than one conversation can hold.")]
    TooLarge,
}

/// One run: its `ui_progress` start, deltas and terminal, and the verbatim assistant turns
/// written while it was open.
struct Run {
    id: String,
    created_at: i64,
    user: i64,
    deltas: Vec<i64>,
    terminal: Option<i64>,
    turns: Vec<i64>,
}

/// The session's rows by seq, with each run's rows gathered in one pass.
pub(crate) struct Index<'a> {
    rows: BTreeMap<i64, &'a EventRow>,
    runs: BTreeMap<i64, Run>,
    /// The bytes built from references so far, against [`MAX_RESOLVED_BYTES`].
    resolved: Cell<u64>,
}

impl<'a> Index<'a> {
    pub(crate) fn new(rows: &'a [EventRow]) -> Result<Self, ViewError> {
        let mut by_seq = BTreeMap::new();
        for row in rows {
            if by_seq.insert(row.seq, row).is_some() {
                return Err(ViewError::BadBody { seq: row.seq });
            }
        }
        let mut runs: BTreeMap<i64, Run> = BTreeMap::new();
        // Started and not yet terminated, oldest first. A model turn belongs to the newest.
        let mut open: Vec<i64> = vec![];
        for (&seq, row) in &by_seq {
            match row.kind {
                Kind::UiProgress if row.body.get("start").is_some() => {
                    let start = only(&row.body, &["start"], seq)?
                        .get("start")
                        .and_then(Value::as_object)
                        .filter(|start| within(start, &["id", "createdAt", "user"]))
                        .ok_or(ViewError::BadBody { seq })?;
                    let (Some(id), Some(created_at), Some(user)) = (
                        start.get("id").and_then(Value::as_str),
                        start.get("createdAt").and_then(Value::as_i64),
                        start.get("user").and_then(Value::as_i64),
                    ) else {
                        return Err(ViewError::BadBody { seq });
                    };
                    if by_seq.get(&user).map(|row| row.kind) != Some(Kind::UiUser) {
                        return Err(ViewError::BadRef { seq });
                    }
                    runs.insert(
                        seq,
                        Run {
                            id: id.to_owned(),
                            created_at,
                            user,
                            deltas: vec![],
                            terminal: None,
                            turns: vec![],
                        },
                    );
                    open.push(seq);
                }
                Kind::UiProgress => open_run(&mut runs, &row.body, seq)?.deltas.push(seq),
                Kind::UiAssistant if row.body.get("run").is_some() => {
                    let run = open_run(&mut runs, &row.body, seq)?;
                    run.terminal = Some(seq);
                    let start = row.body["run"].as_i64();
                    open.retain(|&open| Some(open) != start);
                }
                Kind::Model if verbatim_role(&row.body) == Some("assistant") => {
                    if let Some(run) = open.last().and_then(|start| runs.get_mut(start)) {
                        run.turns.push(seq);
                    }
                }
                Kind::UiUser | Kind::UiAssistant | Kind::Model => {}
            }
        }
        Ok(Self {
            rows: by_seq,
            runs,
            resolved: Cell::new(0),
        })
    }

    /// Counts `bytes` about to be built from a reference, before they are: `TooLarge` past the cap.
    fn charge(&self, bytes: u64) -> Result<(), ViewError> {
        let total = self.resolved.get().saturating_add(bytes);
        if total > MAX_RESOLVED_BYTES {
            return Err(ViewError::TooLarge);
        }
        self.resolved.set(total);
        Ok(())
    }

    fn row(&self, seq: i64, kind: Kind) -> Option<&'a EventRow> {
        self.rows.get(&seq).copied().filter(|row| row.kind == kind)
    }

    /// A verbatim `model` row with `role`, for the row at `referrer` that points at it.
    fn verbatim(&self, seq: i64, role: &str, referrer: i64) -> Result<&'a Value, ViewError> {
        self.row(seq, Kind::Model)
            .map(|row| &row.body)
            .filter(|body| verbatim_role(body) == Some(role))
            .ok_or(ViewError::BadRef { seq: referrer })
    }
}

/// The run a delta or terminal names: it must have started and not yet ended.
fn open_run<'r>(
    runs: &'r mut BTreeMap<i64, Run>,
    body: &Value,
    seq: i64,
) -> Result<&'r mut Run, ViewError> {
    let start = body
        .get("run")
        .and_then(Value::as_i64)
        .ok_or(ViewError::BadBody { seq })?;
    runs.get_mut(&start)
        .filter(|run| run.terminal.is_none())
        .ok_or(ViewError::BadRef { seq })
}

/// The role of a verbatim `model` body; `None` for a ref or a body without one.
fn verbatim_role(body: &Value) -> Option<&str> {
    body.as_object()
        .filter(|body| !body.contains_key("ref"))?
        .get("role")?
        .as_str()
}

fn within(object: &Map<String, Value>, keys: &[&str]) -> bool {
    object.keys().all(|key| keys.contains(&key.as_str()))
}

/// `body` as an object holding no keys but `keys`.
fn only<'v>(body: &'v Value, keys: &[&str], seq: i64) -> Result<&'v Map<String, Value>, ViewError> {
    body.as_object()
        .filter(|object| within(object, keys))
        .ok_or(ViewError::BadBody { seq })
}

/// About how many bytes a copy of `value` holds: its strings and keys, and 8 for anything else.
/// It walks the value without allocating.
fn weight(value: &Value) -> u64 {
    match value {
        Value::String(text) => text.len() as u64,
        Value::Array(items) => items.iter().map(weight).sum(),
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| key.len() as u64 + weight(value))
            .sum(),
        Value::Null | Value::Bool(_) | Value::Number(_) => 8,
    }
}

/// A `model` row as a transcript entry: verbatim, or the canonical form of what its ref names.
pub(crate) fn resolve_model(rows: &Index<'_>, seq: i64) -> Result<Value, ViewError> {
    let row = rows
        .row(seq, Kind::Model)
        .ok_or(ViewError::BadRef { seq })?;
    let Some(target) = row.body.get("ref") else {
        return Ok(row.body.clone());
    };
    let target = only(&row.body, &["ref"], seq)
        .ok()
        .and(target.as_i64())
        .ok_or(ViewError::BadBody { seq })?;
    match rows.rows.get(&target).map(|row| row.kind) {
        Some(Kind::UiUser) => {
            let user = &rows.rows[&target].body;
            rows.charge(weight(user))?;
            let message = serde_json::from_value::<ChatMessage>(user.clone())
                .map_err(|_| ViewError::BadBody { seq: target })?;
            Ok(message_form(&message, &[]))
        }
        Some(Kind::UiAssistant) => {
            let mut message = resolve_assistant(rows, target)?;
            Ok(json!({"role": "assistant", "content": message["content"].take()}))
        }
        _ => Err(ViewError::BadRef { seq }),
    }
}

/// A `ui_assistant` row as the UI message: a run terminal merged over its run's progress, a
/// fallback's `full` message, or a standalone message with its refs resolved.
pub(crate) fn resolve_assistant(rows: &Index<'_>, seq: i64) -> Result<Value, ViewError> {
    let row = rows
        .row(seq, Kind::UiAssistant)
        .ok_or(ViewError::BadRef { seq })?;
    let body = row.body.as_object().ok_or(ViewError::BadBody { seq })?;
    let Some(start) = body.get("run") else {
        let mut message = Map::new();
        for (key, value) in body {
            let value = match key.as_str() {
                "content" => json!(content(rows, value, seq)?),
                "tools" => Value::Array(
                    list(value, seq)?
                        .iter()
                        .map(|tool| resolve_tool(rows, tool, seq))
                        .collect::<Result<_, _>>()?,
                ),
                _ => value.clone(),
            };
            message.insert(key.clone(), value);
        }
        return Ok(Value::Object(message));
    };
    let run = start
        .as_i64()
        .and_then(|start| rows.runs.get(&start))
        .filter(|run| run.terminal == Some(seq))
        .ok_or(ViewError::BadRef { seq })?;
    if let Some(full) = body.get("full") {
        only(&row.body, &["run", "full"], seq)?;
        return full
            .is_object()
            .then(|| full.clone())
            .ok_or(ViewError::BadBody { seq });
    }
    // Content and status come only from the terminal; the start owns the rest.
    let complete = body.contains_key("content") && body.contains_key("status");
    let overrides = ["id", "role", "createdAt", "attachments"]
        .iter()
        .any(|key| body.contains_key(*key));
    if !complete || overrides {
        return Err(ViewError::BadBody { seq });
    }
    let mut message = progress(rows, run)?;
    for (key, value) in body.iter().filter(|(key, _)| *key != "run") {
        match key.as_str() {
            "content" => {
                message.insert(key.clone(), json!(content(rows, value, seq)?));
            }
            "tools" => upsert(rows, &mut message, value, seq)?,
            _ => {
                message.insert(key.clone(), value.clone());
            }
        }
    }
    Ok(Value::Object(message))
}

/// A run's message from its start and deltas, before any terminal: `RunFold`'s opening message,
/// then each delta in seq order.
fn progress(rows: &Index<'_>, run: &Run) -> Result<Map<String, Value>, ViewError> {
    let mut message = RunFold::new(&run.id, run.created_at, AssistantShape::Desktop)
        .message()
        .as_object()
        .cloned()
        .unwrap_or_default();
    for &seq in &run.deltas {
        let delta = only(
            &rows.rows[&seq].body,
            &["run", "tools", "children", "reasoning", "reasoningMs"],
            seq,
        )?;
        if let Some(tools) = delta.get("tools") {
            upsert(rows, &mut message, tools, seq)?;
        }
        if let Some(children) = delta.get("children") {
            message.insert(
                "children".into(),
                Value::Array(list(children, seq)?.clone()),
            );
        }
        // Set, as `RunFold` sets them on each `TurnComplete` (ruling A4).
        if let Some(reasoning) = delta.get("reasoning") {
            if !reasoning.is_string() {
                return Err(ViewError::BadBody { seq });
            }
            message.insert("reasoning".into(), reasoning.clone());
        }
        match delta.get("reasoningMs") {
            None => {}
            Some(Value::Null) => {
                message.remove("reasoningMs");
            }
            Some(ms) if ms.is_number() => {
                message.insert("reasoningMs".into(), ms.clone());
            }
            Some(_) => return Err(ViewError::BadBody { seq }),
        }
    }
    Ok(message)
}

fn list(value: &Value, seq: i64) -> Result<&Vec<Value>, ViewError> {
    value.as_array().ok_or(ViewError::BadBody { seq })
}

/// Resolves each tool residual in `tools` and upserts it into the message's tools by `id`, as
/// `RunFold` does: an instructions entry is removed and re-appended, so it moves to the end; any
/// other known id is replaced in place, and a new one goes last (ruling A4-r1).
fn upsert(
    rows: &Index<'_>,
    message: &mut Map<String, Value>,
    tools: &Value,
    seq: i64,
) -> Result<(), ViewError> {
    for residual in list(tools, seq)? {
        let tool = resolve_tool(rows, residual, seq)?;
        let id = tool.get("id").filter(|id| id.is_string()).cloned();
        let id = id.ok_or(ViewError::BadBody { seq })?;
        let Value::Array(known) = message.entry("tools").or_insert_with(|| json!([])) else {
            return Err(ViewError::BadBody { seq });
        };
        if is_instructions(&tool) {
            known.retain(|known| known.get("id") != Some(&id));
        }
        match known.iter_mut().find(|known| known.get("id") == Some(&id)) {
            Some(known) => *known = tool,
            None => known.push(tool),
        }
    }
    Ok(())
}

/// The synthetic tool `RunFold` makes for `InstructionsLoaded`: named `project_instructions`, with
/// the id `instructions` or `instructions_<agent>`.
fn is_instructions(tool: &Value) -> bool {
    tool["name"] == "project_instructions"
        && tool["id"]
            .as_str()
            .is_some_and(|id| id == "instructions" || id.starts_with("instructions_"))
}

/// A `content` field: a string, or `{turns, tail?}` rebuilt with the agent's accumulator.
fn content(rows: &Index<'_>, form: &Value, seq: i64) -> Result<String, ViewError> {
    if let Some(text) = form.as_str() {
        // Counted too: an echo ref in the transcript builds this text again each time.
        rows.charge(text.len() as u64)?;
        return Ok(text.to_owned());
    }
    let form = only(form, &["turns", "tail"], seq)?;
    let turns = form
        .get("turns")
        .and_then(Value::as_array)
        .ok_or(ViewError::BadBody { seq })?;
    let texts = turns
        .iter()
        .map(|turn| {
            let turn = turn.as_i64().ok_or(ViewError::BadBody { seq })?;
            rows.verbatim(turn, "assistant", seq)?["content"]
                .as_str()
                .ok_or(ViewError::BadRef { seq })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let tail = match form.get("tail") {
        None => "",
        Some(tail) => tail.as_str().ok_or(ViewError::BadBody { seq })?,
    };
    // Counted before the text is joined: each turn plus the separator before it.
    rows.charge(texts.iter().map(|text| text.len() as u64 + 2).sum())?;
    Ok(accumulate(texts) + tail)
}

/// A tool residual: inline as is, or `{id, call: [seq, index], result: seq|null, …}` with
/// `name` and `arguments` from that call and `output` from the result row.
fn resolve_tool(rows: &Index<'_>, residual: &Value, seq: i64) -> Result<Value, ViewError> {
    let object = residual.as_object().ok_or(ViewError::BadBody { seq })?;
    let Some(call) = object.get("call") else {
        return Ok(residual.clone());
    };
    let (Some([Some(at), Some(index)]), Some(result)) = (
        call.as_array()
            .map(|call| call.iter().map(Value::as_i64).collect::<Vec<_>>())
            .and_then(|call| <[Option<i64>; 2]>::try_from(call).ok()),
        object.get("result"),
    ) else {
        return Err(ViewError::BadBody { seq });
    };
    let derived = ["name", "arguments"]
        .iter()
        .chain(if result.is_null() {
            None
        } else {
            Some(&"output")
        })
        .any(|key| object.contains_key(*key));
    if derived {
        return Err(ViewError::BadBody { seq });
    }
    let called = usize::try_from(index)
        .ok()
        .and_then(|index| rows.verbatim(at, "assistant", seq).ok()?["tool_calls"].get(index))
        .ok_or(ViewError::BadRef { seq })?;
    let (Some(id), Some(name), Some(arguments)) = (
        called.get("id"),
        called["function"].get("name"),
        called["function"].get("arguments"),
    ) else {
        return Err(ViewError::BadRef { seq });
    };
    rows.charge(weight(name) + weight(arguments))?;
    let mut tool = object.clone();
    tool.remove("call");
    tool.remove("result");
    tool.insert("name".into(), name.clone());
    tool.insert("arguments".into(), arguments.clone());
    if !result.is_null() {
        let output = result
            .as_i64()
            .ok_or(ViewError::BadBody { seq })
            .and_then(|result| rows.verbatim(result, "tool", seq))?;
        if output.get("tool_call_id") != Some(id) {
            return Err(ViewError::BadRef { seq });
        }
        rows.charge(weight(&output["content"]))?;
        tool.insert("output".into(), output["content"].clone());
    }
    Ok(Value::Object(tool))
}

/// A run with no terminal, as v5 stored it: its progress, the text of its turns so far, and
/// status `streaming` (spec correction 5).
fn crash_fold(rows: &Index<'_>, run: &Run) -> Result<Value, ViewError> {
    let mut message = progress(rows, run)?;
    let texts = run
        .turns
        .iter()
        .map(|&turn| {
            rows.rows[&turn].body["content"]
                .as_str()
                .ok_or(ViewError::BadBody { seq: turn })
        })
        .collect::<Result<Vec<_>, _>>()?;
    rows.charge(texts.iter().map(|text| text.len() as u64 + 2).sum())?;
    message.insert("content".into(), json!(accumulate(texts)));
    Ok(Value::Object(message))
}

/// Rebuilds the chat from its log: the UI messages in seq order, each run's message right
/// after its user message, and the model context from `chat_context`.
pub(crate) fn build_view(
    rows: &[EventRow],
    context: Option<&ContextRow>,
    checkpoints: &[Value],
) -> Result<ChatView, ViewError> {
    let index = Index::new(rows)?;
    let mut messages = vec![];
    let mut open_run = None;
    let mut slots: BTreeMap<i64, Vec<(i64, &Run)>> = BTreeMap::new();
    for (&start, run) in &index.runs {
        slots.entry(run.user).or_default().push((start, run));
    }
    for (&seq, row) in &index.rows {
        match row.kind {
            Kind::UiUser => {
                messages.push(row.body.clone());
                for &(start, run) in slots.get(&seq).into_iter().flatten() {
                    match run.terminal {
                        Some(terminal) => messages.push(resolve_assistant(&index, terminal)?),
                        None => {
                            messages.push(crash_fold(&index, run)?);
                            // The newest open run wins: one run per chat at a time.
                            if open_run
                                .as_ref()
                                .is_none_or(|open: &OpenRun| open.start < start)
                            {
                                open_run = Some(OpenRun {
                                    start,
                                    assistant_id: run.id.clone(),
                                    user_seq: seq,
                                    message_index: messages.len() as u64 - 1,
                                    turn_seqs: run.turns.clone(),
                                });
                            }
                        }
                    }
                }
            }
            Kind::UiAssistant if row.body.get("run").is_none() => {
                messages.push(resolve_assistant(&index, seq)?);
            }
            Kind::UiAssistant | Kind::UiProgress | Kind::Model => {}
        }
    }
    let context = context
        .map(|context| context_state(&index, context, checkpoints))
        .transpose()?;
    Ok(ChatView {
        messages,
        context,
        open_run,
    })
}

/// The `contextState`: the first `model_len` model rows from `model_from`, resolved, and the
/// stored bookkeeping verbatim.
fn context_state(
    rows: &Index<'_>,
    context: &ContextRow,
    checkpoints: &[Value],
) -> Result<Value, ViewError> {
    let length = usize::try_from(context.model_len).map_err(|_| ViewError::ContextLength)?;
    let transcript = rows
        .rows
        .range(context.model_from..)
        .filter(|(_, row)| row.kind == Kind::Model)
        .take(length)
        .map(|(&seq, _)| resolve_model(rows, seq))
        .collect::<Result<Vec<_>, _>>()?;
    if transcript.len() < length {
        return Err(ViewError::ContextLength);
    }
    let mut state = json!({
        "transcript": transcript,
        "checkpoints": checkpoints,
        "coveredMessages": context.covered_messages,
        "sourceHash": context.source_hash,
    });
    if let Some(usage) = &context.server_usage {
        state["serverUsage"] = serde_json::from_str(usage).map_err(|_| ViewError::BadContext)?;
    }
    Ok(state)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(seq: i64, kind: Kind, body: Value) -> EventRow {
        EventRow { seq, kind, body }
    }
    fn model(seq: i64, body: Value) -> EventRow {
        row(seq, Kind::Model, body)
    }
    fn reference(seq: i64, target: i64) -> EventRow {
        model(seq, json!({ "ref": target }))
    }
    fn said(seq: i64, text: &str) -> EventRow {
        model(seq, json!({"role": "assistant", "content": text}))
    }
    fn user_message(id: &str, text: &str) -> Value {
        json!({
            "id": id, "role": "user", "content": text, "attachments": [],
            "status": "complete", "createdAt": 1000, "historical": true,
        })
    }
    fn user(seq: i64, id: &str, text: &str) -> EventRow {
        row(seq, Kind::UiUser, user_message(id, text))
    }
    fn start(seq: i64, id: &str, created_at: i64, user: i64) -> EventRow {
        row(
            seq,
            Kind::UiProgress,
            json!({"start": {"id": id, "createdAt": created_at, "user": user}}),
        )
    }
    fn progress(seq: i64, delta: Value) -> EventRow {
        row(seq, Kind::UiProgress, delta)
    }
    fn assistant(seq: i64, body: Value) -> EventRow {
        row(seq, Kind::UiAssistant, body)
    }
    fn context(model_from: i64, model_len: u64) -> ContextRow {
        ContextRow {
            model_from,
            model_len,
            covered_messages: 1,
            source_hash: "hash".into(),
            server_usage: None,
        }
    }
    fn view(rows: &[EventRow]) -> ChatView {
        build_view(rows, None, &[]).unwrap()
    }
    fn transcript(rows: &[EventRow], context: &ContextRow) -> Value {
        build_view(rows, Some(context), &[])
            .unwrap()
            .context
            .unwrap()["transcript"]
            .clone()
    }
    fn error(rows: &[EventRow], context: Option<&ContextRow>) -> ViewError {
        build_view(rows, context, &[]).unwrap_err()
    }

    #[test]
    fn user_refs_resolve_to_the_canonical_form_with_attachments() {
        let mut message = user_message("u1", "Look at these");
        message["attachments"] = json!([
            {"id": "a1", "name": "notes.txt", "mimeType": "text/plain", "sizeBytes": 5, "textContent": "hello"},
            {"id": "a2", "name": "shot.png", "mimeType": "image/png", "sizeBytes": 1, "dataUrl": "data:image/png;base64,AA=="},
            {"id": "a3", "name": "blob.bin", "mimeType": "application/octet-stream", "sizeBytes": 9},
        ]);
        let rows = [
            model(0, json!({"role": "system", "content": "Be brief."})),
            row(1, Kind::UiUser, message.clone()),
            reference(2, 1),
        ];
        let mut ctx = context(0, 2);
        ctx.server_usage =
            Some(r#"{"promptTokens":4,"completionTokens":2,"totalTokens":6}"#.into());
        let checkpoints = [json!({"through": 1, "sourceHash": "c", "summary": {}})];
        let loaded = build_view(&rows, Some(&ctx), &checkpoints).unwrap();

        assert_eq!(loaded.messages, [message.clone()]);
        let canonical = message_form(
            &serde_json::from_value::<ChatMessage>(message).unwrap(),
            &[],
        );
        assert_eq!(
            canonical,
            json!({"role": "user", "content": [
                {"type": "text", "text": "Look at these\n<attachment name=\"notes.txt\">\nhello\n</attachment>\n[Attached file: blob.bin. Binary contents are not available.]"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA=="}},
            ]})
        );
        assert_eq!(
            loaded.context.unwrap(),
            json!({
                "transcript": [{"role": "system", "content": "Be brief."}, canonical],
                "checkpoints": checkpoints,
                "coveredMessages": 1,
                "sourceHash": "hash",
                "serverUsage": {"promptTokens": 4, "completionTokens": 2, "totalTokens": 6},
            })
        );
        assert_eq!(loaded.open_run, None);

        // A NULL `server_usage` leaves the key out.
        let loaded = build_view(&rows, Some(&context(0, 2)), &[]).unwrap();
        let state = loaded.context.unwrap();
        assert!(state.get("serverUsage").is_none());
        assert_eq!(state["checkpoints"], json!([]));
    }

    #[test]
    fn content_turns_and_tail_rebuild_the_exact_text() {
        let rows = [
            user(0, "u", "Go"),
            reference(1, 0),
            said(2, "A"),
            said(3, ""),
            said(4, "B"),
            assistant(
                5,
                json!({
                    "id": "a", "role": "assistant", "attachments": [], "status": "stopped", "createdAt": 9,
                    "content": {"turns": [2, 3, 4], "tail": "\n\npart"},
                }),
            ),
            assistant(
                6,
                json!({
                    "id": "b", "role": "assistant", "attachments": [], "status": "complete", "createdAt": 10,
                    "content": {"turns": [2, 3, 4]},
                }),
            ),
            assistant(
                7,
                json!({
                    "id": "c", "role": "assistant", "attachments": [], "status": "complete", "createdAt": 11,
                    "content": "inline text",
                }),
            ),
        ];
        let loaded = view(&rows);
        assert_eq!(loaded.messages.len(), 4);
        assert_eq!(loaded.messages[1]["content"], "A\n\n\n\nB\n\npart");
        assert_eq!(loaded.messages[2]["content"], "A\n\n\n\nB");
        assert_eq!(loaded.messages[3]["content"], "inline text");
        assert_eq!(
            loaded.messages[1],
            json!({
                "id": "a", "role": "assistant", "attachments": [], "status": "stopped", "createdAt": 9,
                "content": "A\n\n\n\nB\n\npart",
            })
        );
    }

    #[test]
    fn tool_refs_take_name_arguments_and_output_from_their_rows_and_keep_the_ui_id() {
        let instructions = json!({
            "id": "instructions", "name": "project_instructions", "contentOffset": 0,
            "arguments": "{\"path\":\"AGENTS.md\"}", "output": "Loaded instructions: AGENTS.md",
            "status": "complete",
        });
        let rows = [
            user(0, "u", "List it"),
            reference(1, 0),
            model(
                2,
                json!({"role": "assistant", "content": "Looking", "tool_calls": [
                    {"id": "c8", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"a\"}"}},
                    {"id": "c9", "type": "function", "function": {"name": "list_files", "arguments": "{}"}},
                ]}),
            ),
            model(
                3,
                json!({"role": "tool", "tool_call_id": "c9", "content": "a\nb"}),
            ),
            assistant(
                4,
                json!({
                    "id": "a", "role": "assistant", "attachments": [], "status": "stopped", "createdAt": 9,
                    "content": {"turns": [2]},
                    "tools": [
                        instructions,
                        {"id": "1_0_c9", "call": [2, 1], "result": 3, "contentOffset": 7, "status": "complete", "truncated": true},
                        {"id": "1_0_c8", "call": [2, 0], "result": null, "contentOffset": 7, "status": "stopped"},
                    ],
                }),
            ),
        ];
        let loaded = view(&rows);
        assert_eq!(
            loaded.messages[1]["tools"],
            json!([
                instructions,
                {"id": "1_0_c9", "name": "list_files", "arguments": "{}", "output": "a\nb",
                 "contentOffset": 7, "status": "complete", "truncated": true},
                {"id": "1_0_c8", "name": "read_file", "arguments": "{\"path\":\"a\"}",
                 "contentOffset": 7, "status": "stopped"},
            ])
        );
        assert_eq!(loaded.messages[1]["content"], "Looking");
    }

    #[test]
    fn an_assistant_echo_ref_resolves_to_its_messages_content() {
        let rows = [
            user(0, "u1", "First"),
            reference(1, 0),
            start(2, "a1", 5, 0),
            said(3, "Partial"),
            assistant(
                4,
                json!({"run": 2, "content": {"turns": [3], "tail": "\n\nmore"}, "status": "stopped"}),
            ),
            assistant(
                5,
                json!({
                    "id": "a2", "role": "assistant", "attachments": [], "status": "stopped",
                    "createdAt": 6, "content": "Standalone",
                }),
            ),
            user(6, "u2", "Second"),
            reference(7, 4),
            reference(8, 5),
            reference(9, 6),
        ];
        let loaded = build_view(&rows, Some(&context(0, 5)), &[]).unwrap();
        assert_eq!(
            loaded.messages[1],
            json!({
                "id": "a1", "role": "assistant", "content": "Partial\n\nmore", "attachments": [],
                "status": "stopped", "createdAt": 5,
            })
        );
        assert_eq!(
            loaded.context.unwrap()["transcript"],
            json!([
                {"role": "user", "content": "First"},
                {"role": "assistant", "content": "Partial"},
                {"role": "assistant", "content": "Partial\n\nmore"},
                {"role": "assistant", "content": "Standalone"},
                {"role": "user", "content": "Second"},
            ])
        );
        let index = Index::new(&rows).unwrap();
        assert_eq!(resolve_assistant(&index, 4).unwrap(), loaded.messages[1]);
        assert_eq!(
            resolve_model(&index, 7).unwrap(),
            json!({"role": "assistant", "content": "Partial\n\nmore"})
        );
    }

    #[test]
    fn a_run_without_a_terminal_folds_progress_and_turns_as_streaming() {
        let instructions = json!({
            "id": "instructions", "name": "project_instructions", "contentOffset": 0,
            "arguments": "{\"path\":\"AGENTS.md\"}", "output": "Loaded instructions: AGENTS.md",
            "status": "complete",
        });
        let call = |id: &str, name: &str| json!({"id": id, "type": "function", "function": {"name": name, "arguments": "{}"}});
        let rows = [
            user(0, "u", "Work"),
            reference(1, 0),
            start(2, "a", 7, 0),
            model(
                3,
                json!({"role": "assistant", "content": "Looking", "tool_calls": [call("c1", "read_file")]}),
            ),
            model(
                4,
                json!({"role": "tool", "tool_call_id": "c1", "content": "data"}),
            ),
            progress(
                5,
                json!({"run": 2, "reasoning": "think ", "reasoningMs": 10, "tools": [
                    {"id": "c1", "call": [3, 0], "result": 4, "contentOffset": 7, "status": "complete"},
                ]}),
            ),
            said(6, ""),
            model(
                7,
                json!({"role": "assistant", "content": "Next", "tool_calls": [call("c2", "list_files")]}),
            ),
            progress(
                8,
                json!({"run": 2, "reasoning": "more", "children": [{"id": "k", "state": "running", "summary": ""}],
                       "tools": [{"id": "c2", "call": [7, 0], "result": null, "contentOffset": 13, "status": "running"}]}),
            ),
            progress(
                9,
                json!({"run": 2, "reasoningMs": 12, "tools": [
                    instructions,
                    {"id": "c1", "call": [3, 0], "result": 4, "contentOffset": 7, "status": "complete", "truncated": true},
                ]}),
            ),
        ];
        let loaded = view(&rows);
        assert_eq!(
            loaded.messages,
            [
                user_message("u", "Work"),
                json!({
                    "id": "a", "role": "assistant", "content": "Looking\n\n\n\nNext", "attachments": [],
                    "status": "streaming", "createdAt": 7,
                    "reasoning": "more", "reasoningMs": 12,
                    "children": [{"id": "k", "state": "running", "summary": ""}],
                    "tools": [
                        {"id": "c1", "name": "read_file", "arguments": "{}", "output": "data",
                         "contentOffset": 7, "status": "complete", "truncated": true},
                        {"id": "c2", "name": "list_files", "arguments": "{}", "contentOffset": 13, "status": "running"},
                        instructions,
                    ],
                }),
            ]
        );
        assert_eq!(
            loaded.open_run,
            Some(OpenRun {
                start: 2,
                assistant_id: "a".into(),
                user_seq: 0,
                message_index: 1,
                turn_seqs: vec![3, 6, 7],
            })
        );
    }

    #[test]
    fn the_crash_fold_and_a_terminal_match_fold_run() {
        use crate::run_fold::{fold_run, RunEnd};
        // A run up to its last batch: what a crash leaves durable.
        let events: Vec<crate::protocol::AgentEvent> = serde_json::from_value(json!([
            {"type": "instructions_loaded", "requestId": "r", "sources": ["AGENTS.md"], "warnings": []},
            {"type": "assistant_delta", "requestId": "r", "delta": "Looking"},
            {"type": "tool_call", "requestId": "r", "toolCallId": "c1", "name": "read_file", "arguments": "{}"},
            {"type": "tool_result", "requestId": "r", "toolCallId": "c1", "output": "data", "success": true, "truncated": true},
            {"type": "subagent_status", "requestId": "r", "agentId": "k", "state": "running"},
        ]))
        .unwrap();
        let open = fold_run("a", 7, AssistantShape::Desktop, &events, None);
        let mut rows = vec![
            user(0, "u", "Work"),
            reference(1, 0),
            start(2, "a", 7, 0),
            model(
                3,
                json!({"role": "assistant", "content": "Looking", "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "read_file", "arguments": "{}"}},
                ]}),
            ),
            model(
                4,
                json!({"role": "tool", "tool_call_id": "c1", "content": "data"}),
            ),
            progress(
                5,
                json!({"run": 2, "children": open["children"], "tools": [
                    open["tools"][0],
                    {"id": "c1", "call": [3, 0], "result": 4, "contentOffset": 7, "status": "complete", "truncated": true},
                ]}),
            ),
        ];
        assert_eq!(view(&rows).messages[1], open);

        let stopped = fold_run(
            "a",
            7,
            AssistantShape::Desktop,
            &events,
            Some(&RunEnd::Stopped),
        );
        rows.push(assistant(
            6,
            json!({"run": 2, "content": {"turns": [3]}, "status": "stopped", "children": stopped["children"]}),
        ));
        let loaded = view(&rows);
        assert_eq!(loaded.messages[1], stopped);
        assert_eq!(loaded.open_run, None);
    }

    #[test]
    fn progress_reasoning_is_set_and_a_null_reasoning_ms_removes_it_as_fold_run_does() {
        use crate::run_fold::fold_run;
        let events: Vec<crate::protocol::AgentEvent> = serde_json::from_value(json!([
            {"type": "assistant_delta", "requestId": "r", "delta": "One"},
            {"type": "turn_complete", "requestId": "r", "message": {"role": "assistant", "content": "One"},
             "reasoning": "first thought", "reasoningMs": 40},
            {"type": "turn_complete", "requestId": "r", "message": {"role": "assistant", "content": "One"},
             "reasoning": "second"},
        ]))
        .unwrap();
        let open = fold_run("a", 7, AssistantShape::Desktop, &events, None);
        let rows = [
            user(0, "u", "Think"),
            reference(1, 0),
            start(2, "a", 7, 0),
            said(3, "One"),
            progress(
                4,
                json!({"run": 2, "reasoning": "first thought", "reasoningMs": 40}),
            ),
            progress(
                5,
                json!({"run": 2, "reasoning": "second", "reasoningMs": null}),
            ),
        ];
        let loaded = view(&rows);
        assert_eq!(loaded.messages[1]["reasoning"], "second");
        assert!(loaded.messages[1].get("reasoningMs").is_none());
        assert_eq!(loaded.messages[1], open);
    }

    #[test]
    fn a_reloaded_instructions_entry_moves_to_the_end_as_fold_run_does() {
        use crate::run_fold::{fold_run, RunEnd};
        let fixture: Value =
            serde_json::from_str(include_str!("../run_fold/assistant_runs.json")).unwrap();
        let case = fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"] == "instructions_replaced")
            .unwrap();
        let events: Vec<crate::protocol::AgentEvent> =
            serde_json::from_value(case["events"].clone()).unwrap();
        let (id, created_at) = (
            case["id"].as_str().unwrap(),
            case["createdAt"].as_i64().unwrap(),
        );
        let fold = |events: &[crate::protocol::AgentEvent], end: Option<&RunEnd>| {
            fold_run(id, created_at, AssistantShape::Desktop, events, end)
        };
        // The first batch ends after the tool result; the second after the last reload.
        let first = fold(&events[..4], None);
        let open = fold(&events, None);
        assert_eq!(
            first["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| &t["id"])
                .collect::<Vec<_>>(),
            ["instructions", "0_0_r"]
        );
        assert_eq!(
            open["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| &t["id"])
                .collect::<Vec<_>>(),
            ["0_0_r", "instructions_child-1", "instructions"]
        );
        let mut rows = vec![
            user(0, "u", "Read it"),
            reference(1, 0),
            start(2, id, created_at, 0),
            model(
                3,
                json!({"role": "assistant", "content": "Reading 🦀", "tool_calls": [
                    {"id": "0_0_r", "type": "function",
                     "function": {"name": "read_file", "arguments": "{\"path\":\"sub/x.rs\"}"}},
                ]}),
            ),
            model(
                4,
                json!({"role": "tool", "tool_call_id": "0_0_r", "content": "fn x() {}"}),
            ),
            progress(
                5,
                json!({"run": 2, "tools": [
                    first["tools"][0],
                    {"id": "0_0_r", "call": [3, 0], "result": 4, "contentOffset": 10, "status": "complete"},
                ]}),
            ),
            said(6, "Done."),
            progress(
                7,
                json!({"run": 2, "tools": [open["tools"][1], open["tools"][2]]}),
            ),
        ];
        assert_eq!(view(&rows).messages[1], open);

        let complete = fold(&events, Some(&RunEnd::Complete));
        assert_eq!(complete, case["expected"]);
        rows.push(assistant(
            8,
            json!({"run": 2, "content": {"turns": [3, 6]}, "status": "complete"}),
        ));
        assert_eq!(view(&rows).messages[1], complete);
    }

    #[test]
    fn the_transcript_is_the_first_model_len_rows_from_model_from() {
        let rows = [
            said(0, "old"),
            user(1, "u", "Hi"),
            reference(2, 1),
            said(3, "one"),
            user(4, "u2", "Again"),
            said(5, "two"),
            said(6, "three"),
        ];
        // Rows before `model_from` are ignored, and so are rows after the first `model_len`.
        assert_eq!(
            transcript(&rows, &context(2, 2)),
            json!([{"role": "user", "content": "Hi"}, {"role": "assistant", "content": "one"}])
        );
        // `model_from` need not be a model row: counting starts at the next one.
        assert_eq!(
            transcript(&rows, &context(4, 2)),
            json!([{"role": "assistant", "content": "two"}, {"role": "assistant", "content": "three"}])
        );
        assert_eq!(transcript(&rows, &context(7, 0)), json!([]));
        assert_eq!(error(&rows, Some(&context(5, 3))), ViewError::ContextLength);
        // A dangling ref outside the window is never read.
        let mut rows = rows.to_vec();
        rows.push(reference(7, 99));
        assert_eq!(
            transcript(&rows, &context(5, 2)).as_array().unwrap().len(),
            2
        );
        assert_eq!(
            error(&rows, Some(&context(5, 3))),
            ViewError::BadRef { seq: 7 }
        );
    }

    /// Security L5: a few bytes of references cannot make a load build without bound.
    #[test]
    fn references_that_would_build_past_the_cap_are_refused_before_they_are_built() {
        let big = "x".repeat(1 << 20);
        let turns = (MAX_RESOLVED_BYTES >> 20) as usize + 1;
        let rows = |content: Value| {
            vec![
                user(0, "u", "Hi"),
                start(1, "a", 5, 0),
                said(2, &big),
                assistant(
                    3,
                    json!({"run": 1, "content": content, "status": "complete"}),
                ),
            ]
        };
        // One honest reference to the turn is fine.
        let honest = view(&rows(json!({"turns": [2]})));
        assert_eq!(honest.messages[1]["content"], big);
        // The same turn named over and over is refused, typed, before any text is joined.
        let tampered = rows(json!({"turns": vec![2; turns]}));
        assert_eq!(error(&tampered, None), ViewError::TooLarge);
        // The same through repeated tool refs, and through repeated model refs to that answer.
        let mut tools = rows(json!("done"));
        tools[2] = model(
            2,
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "c", "type": "function", "function": {"name": "f", "arguments": big}},
            ]}),
        );
        tools[3] = assistant(
            3,
            json!({"run": 1, "content": "done", "status": "complete",
                   "tools": [{"id": "t", "call": [2, 0], "result": null}]}),
        );
        assert!(build_view(&tools, None, &[]).is_ok());
        let mut many = vec![user(0, "u", "Hi")];
        many.push(model(1, tools[2].body.clone()));
        many.push(assistant(
            2,
            json!({"id": "s", "role": "assistant", "content": "done",
                   "tools": vec![json!({"id": "t", "call": [1, 0], "result": null}); turns]}),
        ));
        assert_eq!(error(&many, None), ViewError::TooLarge);
        let mut echoes = rows(json!({"turns": [2]}));
        echoes.extend((0..turns as i64).map(|n| reference(4 + n, 3)));
        let mut ctx = context(0, 0);
        ctx.model_len = echoes.iter().filter(|r| r.kind == Kind::Model).count() as u64;
        assert_eq!(error(&echoes, Some(&ctx)), ViewError::TooLarge);
        // Echo refs to a standalone answer whose content is inline text, not turns.
        let mut inline = vec![
            user(0, "u", "Hi"),
            assistant(
                1,
                json!({"id": "s", "role": "assistant", "content": big, "status": "complete"}),
            ),
        ];
        inline.extend((0..turns as i64).map(|n| reference(2 + n, 1)));
        let mut ctx = context(0, 0);
        ctx.model_len = turns as u64;
        assert_eq!(error(&inline, Some(&ctx)), ViewError::TooLarge);
        ctx.model_len = 2;
        assert!(build_view(&inline, Some(&ctx), &[]).is_ok());
    }

    #[test]
    fn a_dangling_or_wrong_kind_ref_is_an_error_not_a_guess() {
        let base = || {
            vec![
                user(0, "u", "Hi"),
                reference(1, 0),
                start(2, "a", 5, 0),
                said(3, "Hello"),
                model(4, json!({"role": "user", "content": "verbatim user"})),
                model(
                    5,
                    json!({"role": "assistant", "content": "", "tool_calls": [
                        {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                    ]}),
                ),
                model(
                    6,
                    json!({"role": "tool", "tool_call_id": "c1", "content": "out"}),
                ),
            ]
        };
        let all = Some(context(0, 5));
        let fails = |extra: Vec<EventRow>, expected: ViewError| {
            let mut rows = base();
            rows.extend(extra);
            let mut ctx = context(0, 6);
            ctx.model_len = rows.iter().filter(|r| r.kind == Kind::Model).count() as u64;
            assert_eq!(error(&rows, Some(&ctx)), expected, "{:?}", rows.last());
        };
        // The base log is fine.
        assert!(build_view(&base(), all.as_ref(), &[]).is_ok());

        // Model refs: missing, at a model row, at a progress row.
        fails(vec![reference(7, 99)], ViewError::BadRef { seq: 7 });
        fails(vec![reference(7, 3)], ViewError::BadRef { seq: 7 });
        fails(vec![reference(7, 2)], ViewError::BadRef { seq: 7 });
        fails(
            vec![model(7, json!({"ref": "0"}))],
            ViewError::BadBody { seq: 7 },
        );
        fails(
            vec![model(7, json!({"ref": 0, "role": "user"}))],
            ViewError::BadBody { seq: 7 },
        );

        let terminal = |seq: i64, residual: Value| {
            let mut body = json!({"run": 2, "content": "done", "status": "complete"});
            body.as_object_mut()
                .unwrap()
                .extend(residual.as_object().unwrap().clone());
            assistant(seq, body)
        };
        // Content turns: at a ui_user, at a model ref, at a verbatim user entry, missing.
        for turn in [0, 1, 4, 99] {
            fails(
                vec![terminal(7, json!({"content": {"turns": [3, turn]}}))],
                ViewError::BadRef { seq: 7 },
            );
        }
        fails(
            vec![terminal(7, json!({"content": {"turns": "3"}}))],
            ViewError::BadBody { seq: 7 },
        );
        fails(
            vec![terminal(7, json!({"content": {"turns": [3], "tail": 1}}))],
            ViewError::BadBody { seq: 7 },
        );
        fails(
            vec![terminal(7, json!({"content": 5}))],
            ViewError::BadBody { seq: 7 },
        );

        // Tool refs: an index past the calls, a call row without calls, a result for another call,
        // a result that is not a tool row, and a residual that also carries a derived field.
        let tool = |call: Value, result: Value| json!({"tools": [{"id": "t", "call": call, "result": result, "status": "complete"}]});
        fails(
            vec![terminal(7, tool(json!([5, 1]), json!(6)))],
            ViewError::BadRef { seq: 7 },
        );
        fails(
            vec![terminal(7, tool(json!([3, 0]), json!(null)))],
            ViewError::BadRef { seq: 7 },
        );
        fails(
            vec![
                model(
                    7,
                    json!({"role": "tool", "tool_call_id": "c2", "content": "other"}),
                ),
                terminal(8, tool(json!([5, 0]), json!(7))),
            ],
            ViewError::BadRef { seq: 8 },
        );
        fails(
            vec![terminal(7, tool(json!([5, 0]), json!(3)))],
            ViewError::BadRef { seq: 7 },
        );
        fails(
            vec![terminal(
                7,
                json!({"tools": [{"id": "t", "call": [5, 0], "result": 6, "output": "x"}]}),
            )],
            ViewError::BadBody { seq: 7 },
        );
        fails(
            vec![terminal(
                7,
                json!({"tools": [{"id": "t", "call": [5, 0], "result": 6, "name": "g"}]}),
            )],
            ViewError::BadBody { seq: 7 },
        );

        // Runs: a delta for a run that never started, or for a row that is not a start; a start
        // whose user is not a ui_user; a second terminal; a delta after the terminal.
        fails(
            vec![progress(7, json!({"run": 99, "reasoning": "x"}))],
            ViewError::BadRef { seq: 7 },
        );
        fails(
            vec![progress(7, json!({"run": 3, "reasoning": "x"}))],
            ViewError::BadRef { seq: 7 },
        );
        fails(
            vec![progress(7, json!({"run": 2, "status": "x"}))],
            ViewError::BadBody { seq: 7 },
        );
        fails(vec![start(7, "b", 6, 3)], ViewError::BadRef { seq: 7 });
        fails(
            vec![terminal(7, json!({})), terminal(8, json!({}))],
            ViewError::BadRef { seq: 8 },
        );
        fails(
            vec![
                terminal(7, json!({})),
                progress(8, json!({"run": 2, "reasoning": "x"})),
            ],
            ViewError::BadRef { seq: 8 },
        );
        fails(
            vec![assistant(7, json!({"run": 3, "status": "complete"}))],
            ViewError::BadRef { seq: 7 },
        );
        fails(
            vec![assistant(
                7,
                json!({"run": 2, "full": {}, "status": "complete"}),
            )],
            ViewError::BadBody { seq: 7 },
        );

        // A terminal must carry content and status, and may not set what the start set.
        for residual in [
            json!({"run": 2, "status": "complete"}),
            json!({"run": 2, "content": "done"}),
            json!({"run": 2, "content": "done", "status": "complete", "id": "b"}),
            json!({"run": 2, "content": "done", "status": "complete", "role": "user"}),
            json!({"run": 2, "content": "done", "status": "complete", "createdAt": 1}),
            json!({"run": 2, "content": "done", "status": "complete", "attachments": []}),
            json!({"run": 2, "full": "not a message"}),
        ] {
            fails(vec![assistant(7, residual)], ViewError::BadBody { seq: 7 });
        }

        // An assistant echo whose run terminal is broken is broken too.
        fails(
            vec![
                terminal(7, json!({"content": {"turns": [0]}})),
                reference(8, 7),
            ],
            ViewError::BadRef { seq: 7 },
        );
        // resolve_* refuse rows of the wrong kind.
        let rows = base();
        let index = Index::new(&rows).unwrap();
        assert_eq!(resolve_model(&index, 0), Err(ViewError::BadRef { seq: 0 }));
        assert_eq!(
            resolve_assistant(&index, 3),
            Err(ViewError::BadRef { seq: 3 })
        );
    }

    #[test]
    fn concurrent_progress_rows_fold_into_their_own_runs() {
        let full = json!({
            "id": "b", "role": "assistant", "content": "B whole", "attachments": [],
            "status": "complete", "createdAt": 8,
        });
        let rows = [
            user(0, "u1", "One"),
            reference(1, 0),
            start(2, "a", 7, 0),
            user(3, "u2", "Two"),
            reference(4, 3),
            start(5, "b", 8, 3),
            progress(
                6,
                json!({"run": 5, "children": [{"id": "k", "state": "done", "summary": "ok"}]}),
            ),
            progress(7, json!({"run": 2, "reasoning": "r"})),
            said(8, "A text"),
            assistant(9, json!({"run": 5, "full": full})),
            assistant(
                10,
                json!({"run": 2, "content": {"turns": [8]}, "status": "complete"}),
            ),
        ];
        let loaded = view(&rows);
        assert_eq!(
            loaded.messages,
            [
                user_message("u1", "One"),
                json!({
                    "id": "a", "role": "assistant", "content": "A text", "attachments": [],
                    "status": "complete", "createdAt": 7, "reasoning": "r",
                }),
                user_message("u2", "Two"),
                full,
            ]
        );
        assert_eq!(loaded.open_run, None);

        // Users first, then both starts, neither finished: each slot still follows its user, and
        // the latest run is the open one.
        let rows = [
            user(0, "u1", "One"),
            user(1, "u2", "Two"),
            start(2, "a", 7, 0),
            start(3, "b", 8, 1),
            progress(4, json!({"run": 2, "reasoning": "ra"})),
            progress(5, json!({"run": 3, "reasoning": "rb"})),
        ];
        let loaded = view(&rows);
        let ids: Vec<_> = loaded.messages.iter().map(|m| m["id"].clone()).collect();
        assert_eq!(ids, ["u1", "a", "u2", "b"]);
        assert_eq!(loaded.messages[1]["reasoning"], "ra");
        assert_eq!(loaded.messages[3]["reasoning"], "rb");
        assert_eq!(loaded.messages[1]["status"], "streaming");
        assert_eq!(
            loaded.open_run,
            Some(OpenRun {
                start: 3,
                assistant_id: "b".into(),
                user_seq: 1,
                message_index: 3,
                turn_seqs: vec![],
            })
        );
    }
}
