//! The v5 to v6 conversion of one legacy chat. It copies the chat, lets a matcher propose
//! references, keeps each reference only once it resolves to exactly the value it replaces, then
//! rebuilds the whole chat through the load path and compares it with the v5 body. A chat that
//! fails any of this is set aside whole, never stored lossy. The matcher only decides how much is
//! deduplicated, never whether the result is right.
#![cfg_attr(not(test), allow(dead_code))] // the upgrade that calls it lands later

use super::chats::{
    append_model, append_ui_assistant, append_ui_user, fill_mode_workspace, load_json_v6_from,
    read_counters, stmt, validate_ui_message, write_context, write_counters, write_tx,
};
use super::search::index_message;
use super::view::{resolve_assistant, resolve_model, ContextRow, EventRow, Index, Kind};
use super::{
    validate_id, Cap, LocalStore, StorageError, MAX_CHECKPOINTS, MAX_METADATA_BYTES,
    MAX_MODEL_ENTRIES, MAX_SESSIONS, MAX_SESSION_BYTES, MAX_UI_MESSAGES,
};
use crate::{model::message_form, protocol::ChatMessage, run_fold::accumulate};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// The reason a chat is set aside when its rebuilt copy is not its v5 body. It is a bug.
const CHECK_FAILED: &str = "upgrade check failed";
/// What the compatibility save says when it set a new chat aside instead of saving it.
const SET_ASIDE: &str =
    "This conversation could not be saved as it is, so Blackwall set it aside. Nothing was deleted.";
/// Top-level keys that a column or the log owns. Every other key is kept in `metadata`, and so is
/// a literal `contextState: null` (F5).
const COLUMN_KEYS: [&str; 6] = ["id", "title", "updatedAt", "messages", "mode", "workspace"];

/// What one conversion deduplicated and what it left inline, with why. Bytes are those of the
/// values themselves: a string's length, or any other value's JSON.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct ConvertReport {
    pub renamed_ids: u32,
    /// Bytes a proven reference keeps from being stored a second time.
    pub deduplicated_bytes: u64,
    /// Bytes stored a second time because their model copy could not be referenced.
    pub inline_bytes: u64,
    /// `content`, `tool`, `user` or `echo`, and how often each was left inline.
    pub inline_reasons: BTreeMap<&'static str, u32>,
}

impl ConvertReport {
    fn inline(&mut self, reason: &'static str, bytes: u64) {
        self.inline_bytes += bytes;
        *self.inline_reasons.entry(reason).or_default() += 1;
    }
}

/// How one legacy chat came through.
#[derive(Debug, PartialEq)]
pub(crate) enum ConvertOutcome {
    Kept(ConvertReport),
    SetAside { reason: String },
}

// ---- telemetry and test faults ----

static UPGRADE_INLINE_VALUES: AtomicU64 = AtomicU64::new(0);
static COMPAT_SAVES: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
thread_local! {
    // Per test thread, as `counter_repairs` is, so tests never see each other's counts or faults.
    static INLINE_VALUES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static SAVES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static FAULT: std::cell::Cell<Option<Fault>> = const { std::cell::Cell::new(None) };
    static CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// How many legacy chats failed the whole-session check and were set aside. Each one is a bug.
pub(crate) fn upgrade_inline_values() -> u64 {
    #[cfg(test)]
    return INLINE_VALUES.with(std::cell::Cell::get);
    #[cfg(not(test))]
    UPGRADE_INLINE_VALUES.load(Ordering::Relaxed)
}

fn note_inline_value(id: &str) {
    UPGRADE_INLINE_VALUES.fetch_add(1, Ordering::Relaxed);
    #[cfg(test)]
    INLINE_VALUES.with(|n| n.set(n.get() + 1));
    eprintln!(
        "warn: upgrade_inline_values: chat {id} did not rebuild to its v5 copy and was set aside"
    );
}

/// How many compatibility saves named a chat that already exists.
pub(crate) fn compat_saves() -> u64 {
    #[cfg(test)]
    return SAVES.with(std::cell::Cell::get);
    #[cfg(not(test))]
    COMPAT_SAVES.load(Ordering::Relaxed)
}

fn note_compat_save(id: &str) {
    COMPAT_SAVES.fetch_add(1, Ordering::Relaxed);
    #[cfg(test)]
    SAVES.with(|n| n.set(n.get() + 1));
    eprintln!("warn: compat_saves: a compatibility save named the existing chat {id}");
}

/// A fault the tests inject into this thread's conversions.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    /// The whole-session check reports a difference.
    ForceMismatch,
    /// The conversion with this number (from 0, counted since `set_fault`) hits an SQL error
    /// after writing its rows.
    ConvertError(u32),
}

#[cfg(test)]
pub(crate) fn set_fault(fault: Option<Fault>) {
    FAULT.with(|f| f.set(fault));
    CALLS.with(|n| n.set(0));
}

#[cfg(test)]
fn fault() -> Option<Fault> {
    FAULT.with(std::cell::Cell::get)
}

// ---- one chat ----

/// Why a conversion stopped.
enum Fail {
    /// Invalid, unparsable or over a cap: set aside with this reason.
    SetAside(String),
    /// The rebuilt chat is not the v5 body.
    Mismatch,
    /// SQL or I/O: the whole upgrade rolls back.
    Storage(StorageError),
}

impl From<StorageError> for Fail {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::Invalid(_) | StorageError::Json(_) | StorageError::Limit(_) => {
                Self::SetAside(error.to_string())
            }
            error => Self::Storage(error),
        }
    }
}

impl From<rusqlite::Error> for Fail {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}

impl From<serde_json::Error> for Fail {
    fn from(error: serde_json::Error) -> Self {
        StorageError::from(error).into()
    }
}

/// Converts one legacy chat whose `sessions` row exists, inside the caller's transaction. It owns
/// only `SAVEPOINT convert`: it never begins or commits the outer transaction (P2-4).
///
/// `raw` is the v5 body. A chat that is invalid, unparsable or over a cap, or that does not
/// rebuild to its v5 body, is rolled back to the savepoint and set aside: its raw body goes to
/// `quarantined_sessions` and its `sessions` row is deleted. An SQL or I/O error is returned, and
/// the caller rolls back everything. `updated_at` is the row's own date (F6); `now_ms` only dates
/// a set-aside.
pub(crate) fn convert_session_in_tx(
    conn: &Connection,
    id: &str,
    title: &str,
    updated_at: i64,
    raw: &str,
    now_ms: i64,
) -> Result<ConvertOutcome, StorageError> {
    conn.execute_batch("SAVEPOINT convert")?;
    let reason = match convert(conn, id, updated_at, raw) {
        Ok(report) => {
            conn.execute_batch("RELEASE convert")?;
            return Ok(ConvertOutcome::Kept(report));
        }
        Err(Fail::SetAside(reason)) => reason,
        Err(Fail::Mismatch) => {
            note_inline_value(id);
            CHECK_FAILED.to_owned()
        }
        Err(Fail::Storage(error)) => {
            // Best effort: the caller rolls the whole transaction back anyway.
            let _ = conn.execute_batch("ROLLBACK TO convert; RELEASE convert");
            return Err(error);
        }
    };
    conn.execute_batch("ROLLBACK TO convert")?;
    set_aside(conn, id, title, raw, &reason, now_ms)?;
    conn.execute_batch("RELEASE convert")?;
    Ok(ConvertOutcome::SetAside { reason })
}

fn set_aside(
    conn: &Connection,
    id: &str,
    title: &str,
    raw: &str,
    reason: &str,
    now_ms: i64,
) -> Result<(), StorageError> {
    stmt(
        conn,
        "INSERT INTO quarantined_sessions(id,title,body,reason,found_at) VALUES (?1,?2,?3,?4,?5)",
    )?
    .execute(params![id, title, raw, reason, now_ms])?;
    stmt(conn, "DELETE FROM sessions WHERE id=?1")?.execute([id])?;
    Ok(())
}

fn convert(conn: &Connection, id: &str, updated_at: i64, raw: &str) -> Result<ConvertReport, Fail> {
    #[cfg(test)]
    let inject_error = {
        let call = CALLS.with(|n| n.replace(n.get() + 1));
        fault() == Some(Fault::ConvertError(call))
    };
    // 0. Validate.
    if raw.len() as u64 > MAX_SESSION_BYTES {
        return Err(Fail::SetAside(Cap::SessionTotal.message().into()));
    }
    let mut session: Value = serde_json::from_str(raw)?;
    check_caps(&session)?;
    validate_session(&session)?;
    let mut report = ConvertReport::default();
    // 1. Ids. The renamed body is what the rebuilt chat must equal.
    if let Some(messages) = session.get_mut("messages").and_then(Value::as_array_mut) {
        report.renamed_ids = assign_ids(messages);
    }
    let session = session;
    let messages = session
        .get("messages")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    for message in messages {
        validate_ui_message(message, message["role"].as_str().unwrap_or_default())?;
    }

    // The session row: mode and workspace columns, the other keys in metadata, the v5 body emptied.
    let text = |key: &str, invalid: &'static str| match session.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(StorageError::Invalid(invalid)),
    };
    let mode = text("mode", "Invalid conversation mode.")?;
    let workspace = text("workspace", "Invalid conversation workspace.")?;
    fill_mode_workspace(conn, id, mode, workspace)?;
    let metadata: Map<String, Value> = session
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, value)| {
            !COLUMN_KEYS.contains(&key.as_str()) && !(*key == "contextState" && !value.is_null())
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let metadata = serde_json::to_string(&metadata)?;
    if metadata.len() > MAX_METADATA_BYTES {
        return Err(StorageError::Limit(Cap::Metadata).into());
    }
    if stmt(conn, "UPDATE sessions SET body='',metadata=?2 WHERE id=?1")?
        .execute(params![id, metadata])?
        == 0
    {
        return Err(Fail::Storage(StorageError::NotFound));
    }
    let mut c = read_counters(conn, id)?;
    // An empty `{}` is the column default and counts for nothing, as in `chats::metadata_bytes`.
    if metadata != "{}" {
        c.total_bytes += metadata.len() as u64;
    }

    // 2-3. Copy, propose and prove in memory, then insert each row once.
    let context = session.get("contextState").filter(|value| !value.is_null());
    let transcript = context
        .and_then(|context| context["transcript"].as_array())
        .map_or(&[][..], Vec::as_slice);
    let base = c.next_seq;
    for row in plan(base, messages, transcript, &mut report) {
        let seq = match row.kind {
            Kind::UiUser => append_ui_user(conn, id, &mut c, &row.body)?,
            Kind::UiAssistant => append_ui_assistant(conn, id, &mut c, &row.body)?,
            // The plan makes no progress rows: an upgraded chat has no runs.
            Kind::Model | Kind::UiProgress => append_model(conn, id, &mut c, &row.body)?,
        };
        debug_assert_eq!(seq, row.seq, "a planned seq moved");
    }
    if let Some(context) = context {
        let row = ContextRow {
            model_from: base + messages.len() as i64,
            model_len: transcript.len() as u64,
            covered_messages: context["coveredMessages"].as_u64().unwrap_or_default(),
            source_hash: context["sourceHash"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            // Raw, so a v5 `serverUsage: null` stays `null` and an absent one stays absent (F26).
            server_usage: context
                .get("serverUsage")
                .map(serde_json::to_string)
                .transpose()?,
        };
        let checkpoints = context["checkpoints"]
            .as_array()
            .map_or(&[][..], Vec::as_slice);
        write_context(conn, id, &mut c, &row, checkpoints, false)?;
    }
    #[cfg(test)]
    if inject_error {
        stmt(conn, "SELECT injected FROM convert_fault")?;
    }

    // 4. The whole session, rebuilt by the real load path, must be the v5 body.
    let loaded = load_json_v6_from(conn, id).map_err(|error| match error {
        StorageError::Database(_) | StorageError::Io(_) => Fail::Storage(error),
        _ => Fail::Mismatch,
    })?;
    #[cfg(test)]
    let forced = fault() == Some(Fault::ForceMismatch);
    #[cfg(not(test))]
    let forced = false;
    if forced || loaded != session {
        return Err(Fail::Mismatch);
    }

    // Search: each UI message, resolved, in UI order (ruling A6).
    for (position, message) in loaded["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        index_message(
            conn,
            id,
            &mut c,
            base + position as i64,
            position as u64,
            message,
        )?;
    }
    write_counters(conn, id, &c, updated_at)?;
    Ok(report)
}

/// The caps v5 never checked, each named by its own reason (F9). The rest are checked where they
/// are enforced: attachments by `validate_ui_message`, bodies and totals by the appends.
fn check_caps(session: &Value) -> Result<(), StorageError> {
    let length = |value: &Value| value.as_array().map_or(0, Vec::len) as u64;
    if length(&session["messages"]) > MAX_UI_MESSAGES {
        return Err(StorageError::Limit(Cap::UiMessages));
    }
    let context = &session["contextState"];
    if length(&context["transcript"]) > MAX_MODEL_ENTRIES {
        return Err(StorageError::Limit(Cap::ModelEntries));
    }
    if length(&context["checkpoints"]) > MAX_CHECKPOINTS as u64 {
        return Err(StorageError::Limit(Cap::Checkpoints));
    }
    Ok(())
}

/// Gives every message a usable, unique id. A missing, non-string, empty, over-128-byte or
/// repeated id becomes `message_{position}`, or `message_{position}_{n}` with the smallest free
/// `n >= 1`. Returns how many it renamed.
fn assign_ids(messages: &mut [Value]) -> u32 {
    let mut taken = HashSet::new();
    let mut renamed = vec![];
    for (position, message) in messages.iter().enumerate() {
        let kept = message
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| (1..=128).contains(&id.len()))
            .is_some_and(|id| taken.insert(id.to_owned()));
        if !kept {
            renamed.push(position);
        }
    }
    for &position in &renamed {
        let mut id = format!("message_{position}");
        let mut n = 1;
        while taken.contains(&id) {
            id = format!("message_{position}_{n}");
            n += 1;
        }
        taken.insert(id.clone());
        if let Some(message) = messages[position].as_object_mut() {
            message.insert("id".into(), json!(id));
        }
    }
    renamed.len() as u32
}

// ---- the matcher ----

/// A reference the matcher proposed. It is kept only once it resolves to the value it replaces.
enum Proposal {
    /// The `model` row at `row` as `{"ref"}` to the UI message at `message`.
    Model {
        row: usize,
        message: usize,
        bytes: u64,
    },
    /// An assistant's `content` as `{turns, tail?}`, covering `covered` bytes.
    Content { message: usize, covered: u64 },
    /// An assistant's tool at `tool` as a call/result residual.
    Tool {
        message: usize,
        tool: usize,
        bytes: u64,
    },
}

fn role(entry: &Value) -> Option<&str> {
    entry.get("role")?.as_str()
}

fn value_bytes(value: &Value) -> u64 {
    value.as_str().map_or_else(
        || serde_json::to_string(value).map_or(0, |text| text.len()),
        str::len,
    ) as u64
}

fn set(body: &mut Value, key: &str, value: Value) {
    if let Some(body) = body.as_object_mut() {
        body.insert(key.into(), value);
    }
}

fn set_tool(body: &mut Value, tool: usize, value: Value) {
    if let Some(slot) = body.get_mut("tools").and_then(|tools| tools.get_mut(tool)) {
        *slot = value;
    }
}

/// Plans the rows: the UI messages first, in order, then the transcript, with every proven
/// reference in place. The seqs start at `base`.
fn plan(
    base: i64,
    messages: &[Value],
    transcript: &[Value],
    report: &mut ConvertReport,
) -> Vec<EventRow> {
    let n = messages.len();
    let model_seq = |row: usize| base + (n + row) as i64;
    // The UI message each `model` row is proposed to refer to.
    let mut target: Vec<Option<usize>> = vec![None; transcript.len()];
    let mut proposals = vec![];

    // User refs: each user entry to the first user message at or after the last match whose
    // canonical model form it is, else to the first one anywhere.
    let mut forms: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        if message["role"] != "user" {
            continue;
        }
        if let Ok(chat) = ChatMessage::deserialize(message) {
            if let Ok(key) = serde_json::to_string(&message_form(&chat, &[])) {
                forms.entry(key).or_default().push(index);
            }
        }
    }
    let mut next = 0;
    for (row, entry) in transcript.iter().enumerate() {
        if role(entry) != Some("user") {
            continue;
        }
        let Ok(key) = serde_json::to_string(entry) else {
            continue;
        };
        let Some(candidates) = forms.get(&key) else {
            continue;
        };
        let at = candidates.partition_point(|&index| index < next);
        let Some(&message) = candidates.get(at).or(candidates.first()) else {
            continue;
        };
        target[row] = Some(message);
        proposals.push(Proposal::Model {
            row,
            message,
            bytes: key.len() as u64,
        });
        next = message + 1;
    }
    let mut last_ref = HashMap::new();
    for (row, message) in target.iter().enumerate() {
        if let Some(message) = message {
            last_ref.insert(*message, row);
        }
    }

    // Runs: each assistant's turns are the verbatim assistant rows after its user's last ref, up
    // to the next user row or ref.
    let mut bodies = messages.to_vec();
    let mut last_user = None;
    for (index, message) in messages.iter().enumerate() {
        if message["role"] == "user" {
            last_user = Some(index);
            continue;
        }
        let window = last_user
            .and_then(|user| last_ref.get(&user))
            .map_or(0..0, |&row| {
                let end = (row + 1..transcript.len())
                    .find(|&r| target[r].is_some() || role(&transcript[r]) == Some("user"))
                    .unwrap_or(transcript.len());
                row + 1..end
            });
        let mut turns: Vec<usize> = window
            .clone()
            .filter(|&r| role(&transcript[r]) == Some("assistant"))
            .collect();
        let content = message["content"].as_str().unwrap_or_default();
        // Only a run that did not complete shows text that is in no model row (correction 2).
        let may_tail = message["status"] != "complete";
        let (tail, echo, unproven) = content_form(transcript, &mut turns, content, may_tail);
        if let Some(echo) = echo {
            target[echo] = Some(index);
            proposals.push(Proposal::Model {
                row: echo,
                message: index,
                bytes: value_bytes(&transcript[echo]),
            });
        }
        match tail {
            Some(tail) => {
                let seqs: Vec<i64> = turns.iter().map(|&row| model_seq(row)).collect();
                let mut form = json!({ "turns": seqs });
                if !tail.is_empty() {
                    form["tail"] = json!(tail);
                }
                set(&mut bodies[index], "content", form);
                proposals.push(Proposal::Content {
                    message: index,
                    covered: (content.len() - tail.len()) as u64,
                });
            }
            None if unproven => report.inline("content", content.len() as u64),
            None => {}
        }
        for (position, tool) in message["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let Some((turn, call, result)) = tool_source(transcript, &turns, window.clone(), tool)
            else {
                continue;
            };
            let function = &transcript[turn]["tool_calls"][call]["function"];
            let output = result.map(|row| &transcript[row]["content"]);
            let bytes = value_bytes(&function["name"])
                + value_bytes(&function["arguments"])
                + output.map_or(0, value_bytes);
            if tool.get("name") != Some(&function["name"])
                || tool.get("arguments") != Some(&function["arguments"])
                || tool.get("output") != output
            {
                report.inline("tool", bytes);
                continue;
            }
            let mut residual = tool.as_object().cloned().unwrap_or_default();
            residual.remove("name");
            residual.remove("arguments");
            if result.is_some() {
                residual.remove("output");
            }
            residual.insert("call".into(), json!([model_seq(turn), call]));
            residual.insert(
                "result".into(),
                result.map_or(Value::Null, |row| json!(model_seq(row))),
            );
            set_tool(&mut bodies[index], position, Value::Object(residual));
            proposals.push(Proposal::Tool {
                message: index,
                tool: position,
                bytes,
            });
        }
    }

    let mut rows: Vec<EventRow> = bodies
        .into_iter()
        .enumerate()
        .map(|(index, body)| EventRow {
            seq: base + index as i64,
            kind: if body["role"] == "user" {
                Kind::UiUser
            } else {
                Kind::UiAssistant
            },
            body,
        })
        .collect();
    rows.extend(transcript.iter().enumerate().map(|(row, entry)| EventRow {
        seq: model_seq(row),
        kind: Kind::Model,
        body: match target[row] {
            Some(message) => json!({ "ref": base + message as i64 }),
            None => entry.clone(),
        },
    }));

    // Prove the UI side first: a `model` ref to an assistant resolves through its content.
    let mut kept = vec![true; proposals.len()];
    for model_side in [false, true] {
        let failed = disproved(&rows, &proposals, model_side, messages, transcript, base);
        for at in failed {
            kept[at] = false;
            match proposals[at] {
                Proposal::Model { row, .. } => rows[n + row].body = transcript[row].clone(),
                Proposal::Content { message, .. } => {
                    set(
                        &mut rows[message].body,
                        "content",
                        messages[message]["content"].clone(),
                    );
                }
                Proposal::Tool { message, tool, .. } => {
                    set_tool(
                        &mut rows[message].body,
                        tool,
                        messages[message]["tools"][tool].clone(),
                    );
                }
            }
        }
    }
    for (proposal, kept) in proposals.iter().zip(kept) {
        match *proposal {
            Proposal::Model { bytes, .. }
            | Proposal::Content { covered: bytes, .. }
            | Proposal::Tool { bytes, .. }
                if kept =>
            {
                report.deduplicated_bytes += bytes;
            }
            Proposal::Model { message, bytes, .. } => {
                let reason = if messages[message]["role"] == "user" {
                    "user"
                } else {
                    "echo"
                };
                report.inline(reason, bytes);
            }
            Proposal::Content { message, .. } => {
                let content = messages[message]["content"].as_str().unwrap_or_default();
                report.inline("content", content.len() as u64);
            }
            Proposal::Tool { bytes, .. } => report.inline("tool", bytes),
        }
    }
    rows
}

/// How an assistant's content can be written: `(Some(tail), …)` for `{turns, tail?}` over the
/// final `turns`, with the echo row it found (the restore-appended copy of this message, which
/// then refers to it), and whether model turns were there but could not cover the content.
fn content_form(
    transcript: &[Value],
    turns: &mut Vec<usize>,
    content: &str,
    may_tail: bool,
) -> (Option<String>, Option<usize>, bool) {
    let texts: Option<Vec<&str>> = turns
        .iter()
        .map(|&row| transcript[row]["content"].as_str())
        .collect();
    let Some(mut texts) = texts else {
        return (None, None, true);
    };
    if texts.is_empty() {
        return (None, None, false);
    }
    if accumulate(texts.iter().copied()) == content {
        return (Some(String::new()), None, false);
    }
    let mut echo = None;
    if let Some(&last) = turns.last() {
        if transcript[last].get("tool_calls").is_none() && texts.last() == Some(&content) {
            echo = turns.pop();
            texts.pop();
            if turns.is_empty() {
                return (None, echo, false);
            }
        }
    }
    let covered = accumulate(texts.iter().copied());
    if covered == content || (may_tail && content.starts_with(&covered)) {
        return (Some(content[covered.len()..].to_owned()), echo, false);
    }
    (None, echo, true)
}

fn decimal(text: &str) -> Option<usize> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Where a UI tool came from: its id `{iter}_{index}_{call id}` names call `index` of the run's
/// turn `iter`, whose result is the first tool row after that turn in the window. Returns the
/// turn row, the call index and the result row.
fn tool_source(
    transcript: &[Value],
    turns: &[usize],
    window: Range<usize>,
    tool: &Value,
) -> Option<(usize, usize, Option<usize>)> {
    let (iter, rest) = tool.get("id")?.as_str()?.split_once('_')?;
    let (index, call_id) = rest.split_once('_')?;
    let (iter, index) = (decimal(iter)?, decimal(index)?);
    let turn = *turns.get(iter)?;
    let call = transcript[turn].get("tool_calls")?.get(index)?;
    if call.get("id")?.as_str()? != call_id {
        return None;
    }
    let result = window.filter(|&row| row > turn).find(|&row| {
        role(&transcript[row]) == Some("tool")
            && transcript[row].get("tool_call_id") == call.get("id")
    });
    Some((turn, index, result))
}

/// The proposals on one side (`model_side`: the `model` refs; otherwise the assistants' content
/// and tools) whose reference does not resolve, through A4's resolvers, to the value in hand.
fn disproved(
    rows: &[EventRow],
    proposals: &[Proposal],
    model_side: bool,
    messages: &[Value],
    transcript: &[Value],
    base: i64,
) -> Vec<usize> {
    let picked = proposals
        .iter()
        .enumerate()
        .filter(|(_, proposal)| matches!(proposal, Proposal::Model { .. }) == model_side)
        .map(|(at, _)| at);
    let Ok(index) = Index::new(rows) else {
        return picked.collect();
    };
    // Each assistant is resolved once, however many of its fields were proposed.
    let mut resolved: HashMap<usize, Option<Value>> = HashMap::new();
    for at in picked.clone() {
        if let Proposal::Content { message, .. } | Proposal::Tool { message, .. } = proposals[at] {
            resolved
                .entry(message)
                .or_insert_with(|| resolve_assistant(&index, base + message as i64).ok());
        }
    }
    let assistant = |message: usize| resolved.get(&message).and_then(Option::as_ref);
    picked
        .filter(|&at| match proposals[at] {
            Proposal::Model { row, .. } => {
                let seq = base + (messages.len() + row) as i64;
                !resolve_model(&index, seq).is_ok_and(|entry| entry == transcript[row])
            }
            Proposal::Content { message, .. } => assistant(message)
                .is_none_or(|resolved| resolved.get("content") != messages[message].get("content")),
            Proposal::Tool { message, tool, .. } => assistant(message).is_none_or(|resolved| {
                resolved["tools"].get(tool) != messages[message]["tools"].get(tool)
            }),
        })
        .collect()
}

// ---- the legacy import and the compatibility save ----

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Whether a chat with this id is kept or set aside.
fn known(conn: &Connection, id: &str) -> Result<bool, StorageError> {
    Ok(stmt(
        conn,
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1) OR EXISTS(SELECT 1 FROM quarantined_sessions WHERE id=?1)",
    )?
    .query_row([id], |row| row.get(0))?)
}

/// `stem`, or `stem_{n}` with the smallest free `n >= 1`.
fn free_id(conn: &Connection, stem: &str) -> Result<String, StorageError> {
    let mut id = stem.to_owned();
    let mut n = 1;
    while known(conn, &id)? {
        id = format!("{stem}_{n}");
        n += 1;
    }
    Ok(id)
}

fn session_columns(session: &Value) -> (&str, i64) {
    (
        session
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        session
            .get("updatedAt")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
    )
}

/// Imports the browser's legacy sessions once, in one transaction. Ids already kept or set aside
/// are skipped, so a chat deleted since is never brought back. A bad session is set aside, not
/// fatal (one with no usable id under `import_{position}`), and the number set aside is returned
/// (F24). An SQL or I/O error rolls the whole import back.
pub(crate) fn migrate_v6(store: &mut LocalStore, sessions: &[Value]) -> Result<u32, StorageError> {
    if store.setting("legacy_sessions_migrated")?.is_some() {
        return Ok(0);
    }
    if sessions.len() > 30 {
        return Err(StorageError::Invalid(
            "The old history contains too many conversations to migrate safely.",
        ));
    }
    let now = now_ms();
    let tx = write_tx(&store.connection)?;
    let mut set_aside_count = 0;
    for (position, session) in sessions.iter().enumerate() {
        let raw = serde_json::to_string(session)?;
        let (title, updated_at) = session_columns(session);
        let id = match session
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| validate_id(id).is_ok())
        {
            Some(id) if known(&tx, id)? => continue,
            Some(id) => id.to_owned(),
            None => free_id(&tx, &format!("import_{position}"))?,
        };
        let count: i64 =
            stmt(&tx, "SELECT count(*) FROM sessions")?.query_row([], |row| row.get(0))?;
        if count >= MAX_SESSIONS {
            set_aside(&tx, &id, title, &raw, Cap::Sessions.message(), now)?;
            set_aside_count += 1;
            continue;
        }
        stmt(
            &tx,
            "INSERT INTO sessions(id,title,updated_at,body) VALUES (?1,?2,?3,'')",
        )?
        .execute(params![id, title, updated_at])?;
        if let ConvertOutcome::SetAside { .. } =
            convert_session_in_tx(&tx, &id, title, updated_at, &raw, now)?
        {
            set_aside_count += 1;
        }
    }
    stmt(
        &tx,
        "INSERT INTO settings(key,value) VALUES ('legacy_sessions_migrated','true')",
    )?
    .execute([])?;
    tx.commit()?;
    Ok(set_aside_count)
}

/// The compatibility save of a chat the backend has not seen: a new id only, converted like a
/// legacy chat with any supplied `contextState` removed first (security H4). An existing id is a
/// `Conflict` at its current revision, and is counted. A session that cannot be converted is set
/// aside, and the caller is told.
pub(crate) fn import_session(store: &LocalStore, session: &Value) -> Result<(), StorageError> {
    let id = session
        .get("id")
        .and_then(Value::as_str)
        .ok_or(StorageError::Invalid("Missing conversation identifier."))?;
    validate_id(id)?;
    let mut session = session.clone();
    if let Some(object) = session.as_object_mut() {
        object.remove("contextState");
    }
    let raw = serde_json::to_string(&session)?;
    let (title, updated_at) = session_columns(&session);
    let tx = write_tx(&store.connection)?;
    let current: Option<i64> = stmt(&tx, "SELECT revision FROM sessions WHERE id=?1")?
        .query_row([id], |row| row.get(0))
        .optional()?;
    if let Some(current) = current {
        note_compat_save(id);
        return Err(StorageError::Conflict { current });
    }
    if known(&tx, id)? {
        return Err(StorageError::Invalid(SET_ASIDE));
    }
    let count: i64 = stmt(&tx, "SELECT count(*) FROM sessions")?.query_row([], |row| row.get(0))?;
    if count >= MAX_SESSIONS {
        return Err(StorageError::Limit(Cap::Sessions));
    }
    stmt(
        &tx,
        "INSERT INTO sessions(id,title,updated_at,body) VALUES (?1,?2,?3,'')",
    )?
    .execute(params![id, title, updated_at])?;
    let outcome = convert_session_in_tx(&tx, id, title, updated_at, &raw, now_ms())?;
    tx.commit()?;
    match outcome {
        ConvertOutcome::Kept(_) => Ok(()),
        ConvertOutcome::SetAside { .. } => Err(StorageError::Invalid(SET_ASIDE)),
    }
}

/// The v5 rules for a whole session body: its id, title, date, messages and model context.
pub(crate) fn validate_session(session: &Value) -> Result<(&str, &str, i64, String), StorageError> {
    let id = session
        .get("id")
        .and_then(Value::as_str)
        .ok_or(StorageError::Invalid("Missing conversation identifier."))?;
    validate_id(id)?;
    let title = session
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 1000)
        .ok_or(StorageError::Invalid("Invalid conversation title."))?;
    let updated = session
        .get("updatedAt")
        .and_then(Value::as_u64)
        .filter(|n| *n <= i64::MAX as u64)
        .ok_or(StorageError::Invalid("Invalid conversation date."))?;
    let messages = session
        .get("messages")
        .and_then(Value::as_array)
        .filter(|m| m.len() <= 2000)
        .ok_or(StorageError::Invalid(
            "This conversation contains too many messages.",
        ))?;
    for message in messages {
        if !matches!(
            message.get("role").and_then(Value::as_str),
            Some("user" | "assistant")
        ) || message.get("content").and_then(Value::as_str).is_none()
            || message
                .get("attachments")
                .and_then(Value::as_array)
                .is_none()
        {
            return Err(StorageError::Invalid(
                "This conversation contains an invalid message.",
            ));
        }
    }
    if let Some(context) = session.get("contextState").filter(|v| !v.is_null()) {
        let state: crate::context::ContextState = serde_json::from_value(context.clone())?;
        state
            .validate()
            .map_err(|_| StorageError::Invalid("Invalid saved model context."))?;
    }
    let body = serde_json::to_string(session)?;
    if body.len() as u64 > MAX_SESSION_BYTES {
        return Err(StorageError::Invalid(Cap::SessionTotal.message()));
    }
    Ok((id, title, updated as i64, body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::memory::MemoryClient;
    use crate::storage::chats::{
        counter_repairs, read_counters, recount, v6_in_memory, write_tx, MetadataPatch,
    };
    use crate::storage::fixtures::{golden, v5_sessions, RECALL_QUERIES};
    use crate::storage::search::search_v6;
    use crate::storage::RecallQuery;
    use rusqlite::params;
    use serde_json::{json, Map};

    const NOW: i64 = 1_800_000_000_000;
    /// The ids fixture's renames: rows 0 and 5 had no id, and row 3 repeated row 2's `a`.
    const RENAMED: [(usize, &str); 3] = [(0, "message_0"), (3, "message_3"), (5, "message_5")];

    fn fixture(id: &str) -> Value {
        v5_sessions()
            .into_iter()
            .find(|session| session["id"] == id)
            .unwrap()
    }

    /// The v5 body as v6 must load it: the ids fixture with its documented ids.
    fn expected(session: &Value) -> Value {
        let mut session = session.clone();
        if session["id"] == "ids_missing_and_duplicate" {
            for (position, id) in RENAMED {
                session["messages"][position]["id"] = json!(id);
            }
        }
        session
    }

    /// The upgrade's shape: v5 rows with their bodies, the v6 schema, then one conversion per row
    /// in one outer transaction, reading each body from its row.
    fn upgrade(db: &LocalStore, bodies: &[Value]) -> BTreeMap<String, ConvertOutcome> {
        let tx = write_tx(db.connection()).unwrap();
        for body in bodies {
            tx.execute(
                "INSERT INTO sessions(id,title,updated_at,body) VALUES (?1,?2,?3,?4)",
                params![
                    body["id"].as_str().unwrap(),
                    body["title"].as_str().unwrap(),
                    body["updatedAt"].as_i64().unwrap(),
                    serde_json::to_string(body).unwrap()
                ],
            )
            .unwrap();
        }
        let rows: Vec<(String, String, i64, String)> = {
            let mut statement = tx
                .prepare("SELECT id,title,updated_at,body FROM sessions ORDER BY updated_at DESC")
                .unwrap();
            let rows = statement
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap();
            rows.collect::<Result<_, _>>().unwrap()
        };
        let mut outcomes = BTreeMap::new();
        for (id, title, updated_at, body) in rows {
            let outcome = convert_session_in_tx(&tx, &id, &title, updated_at, &body, NOW).unwrap();
            outcomes.insert(id, outcome);
        }
        tx.commit().unwrap();
        outcomes
    }

    fn kept(outcome: &ConvertOutcome) -> &ConvertReport {
        match outcome {
            ConvertOutcome::Kept(report) => report,
            ConvertOutcome::SetAside { reason } => panic!("set aside: {reason}"),
        }
    }

    fn count(db: &LocalStore, sql: &str, id: &str) -> i64 {
        db.connection()
            .query_row(sql, [id], |row| row.get(0))
            .unwrap()
    }

    fn event_bodies(db: &LocalStore, id: &str) -> Vec<(String, Value)> {
        let mut statement = db
            .connection()
            .prepare("SELECT kind,body FROM chat_events WHERE session_id=?1 ORDER BY seq")
            .unwrap();
        let rows = statement
            .query_map([id], |row| Ok((row.get(0)?, row.get::<_, String>(1)?)))
            .unwrap();
        rows.map(|row| {
            let (kind, body) = row.unwrap();
            (kind, serde_json::from_str(&body).unwrap())
        })
        .collect()
    }

    fn quarantined(db: &LocalStore, id: &str) -> Option<(String, String, String, i64)> {
        use rusqlite::OptionalExtension;
        db.connection()
            .query_row(
                "SELECT title,body,reason,found_at FROM quarantined_sessions WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .unwrap()
    }

    fn v6_recall(db: &LocalStore) -> Value {
        let mut results = Map::new();
        for query in RECALL_QUERIES {
            let hits = search_v6(
                db,
                MemoryClient::Cli,
                &RecallQuery {
                    query: query.into(),
                    limit: Some(10),
                    ..Default::default()
                },
            )
            .unwrap();
            results.insert(query.into(), serde_json::to_value(hits).unwrap());
        }
        Value::Object(results)
    }

    #[test]
    fn a_reference_that_does_not_resolve_is_stored_inline() {
        // The call has no `arguments`, so the tool's proposed reference cannot resolve, and the
        // assistant's proposals are all disproved and put back inline. The user ref still holds.
        let messages = vec![
            json!({"id":"u","role":"user","content":"q","attachments":[]}),
            json!({"id":"a","role":"assistant","content":"x","attachments":[],"status":"complete",
                "tools":[{"id":"0_0_c1","name":"f","arguments":null,"status":"complete"}]}),
        ];
        let transcript = vec![
            json!({"role":"user","content":"q"}),
            json!({"role":"assistant","content":"x",
                "tool_calls":[{"id":"c1","type":"function","function":{"name":"f"}}]}),
        ];
        let mut report = ConvertReport::default();
        let rows = plan(10, &messages, &transcript, &mut report);
        assert_eq!(
            report.inline_reasons,
            BTreeMap::from([("content", 1), ("tool", 1)])
        );
        // "x", then the tool's "f" and its `null` arguments as JSON.
        assert_eq!(report.inline_bytes, 1 + 1 + 4);
        assert_eq!(rows[1].body, messages[1]);
        assert_eq!(rows[2].body, json!({"ref": 10}));
        assert_eq!(rows[3].body, transcript[1]);
        assert_eq!(report.deduplicated_bytes, value_bytes(&transcript[0]));
        let view = crate::storage::view::build_view(&rows, None, &[]).unwrap();
        assert_eq!(view.messages, messages);
    }

    #[test]
    fn every_fixture_round_trips_exactly_through_the_load_path() {
        let db = v6_in_memory();
        let fixtures = v5_sessions();
        let outcomes = upgrade(&db, &fixtures);
        assert_eq!(outcomes.len(), fixtures.len());
        let repairs = counter_repairs();
        for fixture in &fixtures {
            let id = fixture["id"].as_str().unwrap();
            if id == "bad_role" {
                assert_eq!(
                    outcomes[id],
                    ConvertOutcome::SetAside {
                        reason: "This conversation contains an invalid message.".into()
                    }
                );
                continue;
            }
            kept(&outcomes[id]);
            assert_eq!(
                db.load_json_v6(id).unwrap().unwrap(),
                expected(fixture),
                "{id}"
            );
            // The counters are what the rows add up to, and every v5 body is emptied.
            let tx = write_tx(db.connection()).unwrap();
            assert_eq!(
                read_counters(&tx, id).unwrap(),
                recount(&tx, id).unwrap(),
                "{id}"
            );
            drop(tx);
            assert_eq!(
                count(&db, "SELECT length(body) FROM sessions WHERE id=?1", id),
                0
            );
        }
        assert_eq!(
            counter_repairs(),
            repairs,
            "a converted chat needed a repair"
        );
        assert_eq!(upgrade_inline_values(), 0);

        // The list, in order, with titles, modes and workspaces, is v5's (F6).
        let v5 = LocalStore::initialize(Connection::open_in_memory().unwrap()).unwrap();
        for fixture in fixtures.iter().filter(|f| f["id"] != "bad_role") {
            v5.save_session(fixture).unwrap();
        }
        assert_eq!(db.list_v6().unwrap(), v5.list_sessions().unwrap());
        // Recall is v5's, from the index the conversion wrote.
        assert_eq!(v6_recall(&db), golden("v5_recall.json"));
        // The chat v5 refused is kept whole, set aside.
        let raw = serde_json::to_string(&fixture("bad_role")).unwrap();
        assert_eq!(
            quarantined(&db, "bad_role"),
            Some((
                fixture("bad_role")["title"].as_str().unwrap().into(),
                raw,
                "This conversation contains an invalid message.".into(),
                NOW
            ))
        );
        assert_eq!(
            count(&db, "SELECT count(*) FROM sessions WHERE id=?1", "bad_role"),
            0
        );
    }

    #[test]
    fn generated_ids_never_collide() {
        // The rule on its own: a missing id and a repeated id, beside a kept `message_0`.
        let mut messages = vec![
            json!({"role":"user"}),
            json!({"id":"a"}),
            json!({"id":"a"}),
            json!({"id":"message_0"}),
        ];
        assert_eq!(assign_ids(&mut messages), 2);
        let ids: Vec<_> = messages.iter().map(|m| m["id"].clone()).collect();
        assert_eq!(ids, ["message_0_1", "a", "message_2", "message_0"]);
        // Empty, too long and non-string ids are replaced as well.
        let mut odd = vec![
            json!({"id":""}),
            json!({"id":"x".repeat(129)}),
            json!({"id":7}),
            json!({"id":"y".repeat(128)}),
        ];
        assert_eq!(assign_ids(&mut odd), 3);
        assert_eq!(odd[0]["id"], "message_0");
        assert_eq!(odd[1]["id"], "message_1");
        assert_eq!(odd[2]["id"], "message_2");
        assert_eq!(odd[3]["id"], json!("y".repeat(128)));

        // The real fixture, through the whole conversion.
        let db = v6_in_memory();
        let session = fixture("ids_missing_and_duplicate");
        let outcomes = upgrade(&db, std::slice::from_ref(&session));
        assert_eq!(kept(&outcomes["ids_missing_and_duplicate"]).renamed_ids, 3);
        let loaded = db
            .load_json_v6("ids_missing_and_duplicate")
            .unwrap()
            .unwrap();
        let ids = |session: &Value| -> Vec<String> {
            session["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap_or("").to_owned())
                .collect()
        };
        let mut want = ids(&session);
        for (position, id) in RENAMED {
            want[position] = id.into();
        }
        assert_eq!(ids(&loaded), want);
        assert_eq!(want[2], "a");
    }

    /// Every string leaf in the chat's event bodies.
    fn leaves(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(text) => out.push(text.clone()),
            Value::Array(items) => items.iter().for_each(|item| leaves(item, out)),
            Value::Object(map) => map.values().for_each(|item| leaves(item, out)),
            _ => {}
        }
    }

    /// The texts of a fixture: user messages, each piece of an assistant message between the
    /// agent's `"\n\n"` separators, and tool outputs.
    fn markers(session: &Value) -> Vec<String> {
        let mut markers = vec![];
        for message in session["messages"].as_array().unwrap() {
            let content = message["content"].as_str().unwrap();
            if message["role"] == "user" {
                markers.push(content.to_owned());
            } else {
                markers.extend(content.split("\n\n").map(str::to_owned));
            }
            for tool in message["tools"].as_array().into_iter().flatten() {
                markers.extend(tool["output"].as_str().map(str::to_owned));
            }
        }
        markers.retain(|marker| !marker.is_empty());
        markers.sort();
        markers.dedup();
        markers
    }

    #[test]
    fn plain_fixtures_store_every_text_once() {
        let plain = [
            "desktop_plain",
            "agent_tools",
            "empty_middle_turn",
            "stopped_then_continued",
            "compacted",
        ];
        let db = v6_in_memory();
        let sessions: Vec<Value> = plain.iter().map(|id| fixture(id)).collect();
        let outcomes = upgrade(&db, &sessions);
        for session in &sessions {
            let id = session["id"].as_str().unwrap();
            let report = kept(&outcomes[id]);
            assert_eq!(report.inline_bytes, 0, "{id}: {report:?}");
            assert!(report.inline_reasons.is_empty(), "{id}: {report:?}");
            assert!(report.deduplicated_bytes > 0, "{id}");
            let mut texts = vec![];
            for (_, body) in event_bodies(&db, id) {
                leaves(&body, &mut texts);
            }
            // A copy is a string value, or a piece of one between separators, equal to the text.
            for marker in markers(session) {
                let found: usize = texts
                    .iter()
                    .map(|text| text.split("\n\n").filter(|piece| *piece == marker).count())
                    .sum();
                assert_eq!(found, 1, "{id}: {marker:?} is stored {found} times");
            }
        }
        // The stopped run's echo is a reference to its message, and its unsaved text is a tail.
        let rows = event_bodies(&db, "stopped_then_continued");
        let assistant = &rows[1].1;
        assert_eq!(assistant["content"]["tail"], "\n\nReading the build script");
        assert!(rows
            .iter()
            .any(|(kind, body)| kind == "model" && body == &json!({"ref": 1})));
    }

    #[test]
    fn the_mismatch_fixture_stays_inline_and_says_why() {
        let db = v6_in_memory();
        let session = fixture("text_tool_mismatch");
        let outcomes = upgrade(&db, std::slice::from_ref(&session));
        let report = kept(&outcomes["text_tool_mismatch"]);
        assert_eq!(report.inline_reasons, BTreeMap::from([("content", 1)]));
        assert_eq!(
            report.inline_bytes,
            "Checking\n\nAll good, edited after the run".len() as u64
        );
        assert_eq!(
            db.load_json_v6("text_tool_mismatch").unwrap().unwrap(),
            session
        );
        // The tool is still a reference: only the content stayed inline.
        let rows = event_bodies(&db, "text_tool_mismatch");
        assert_eq!(
            rows[1].1["content"],
            "Checking\n\nAll good, edited after the run"
        );
        assert!(rows[1].1["tools"][0].get("call").is_some());
    }

    #[test]
    fn a_failed_whole_session_check_sets_aside_never_writes_lossy() {
        let db = v6_in_memory();
        let before = upgrade_inline_values();
        set_fault(Some(Fault::ForceMismatch));
        let outcomes = upgrade(&db, &[fixture("desktop_plain")]);
        set_fault(None);
        assert_eq!(
            outcomes["desktop_plain"],
            ConvertOutcome::SetAside {
                reason: "upgrade check failed".into()
            }
        );
        assert_eq!(upgrade_inline_values(), before + 1);
        for table in [
            "chat_events",
            "chat_context",
            "chat_checkpoints",
            "chat_search_keys",
        ] {
            let sql = format!("SELECT count(*) FROM {table} WHERE session_id=?1");
            assert_eq!(count(&db, &sql, "desktop_plain"), 0, "{table}");
        }
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM sessions WHERE id=?1",
                "desktop_plain"
            ),
            0
        );
        let raw = serde_json::to_string(&fixture("desktop_plain")).unwrap();
        assert_eq!(quarantined(&db, "desktop_plain").unwrap().1, raw);
    }

    #[test]
    fn an_over_cap_legacy_chat_is_set_aside_with_the_cap_named() {
        let db = v6_in_memory();
        let mut many = fixture("desktop_plain");
        many["id"] = json!("many_attachments");
        many["messages"][0]["attachments"] = json!(vec![json!({"name":"a.txt"}); 9]);
        let mut big = fixture("desktop_plain");
        big["id"] = json!("big_attachment");
        big["messages"][2]["attachments"] = json!([{"name":"a.bin","sizeBytes": 16u64 << 20}]);
        let outcomes = upgrade(&db, &[many, big]);
        assert_eq!(
            outcomes["many_attachments"],
            ConvertOutcome::SetAside {
                reason: Cap::AttachmentsPerMessage.message().into()
            }
        );
        assert_eq!(
            outcomes["big_attachment"],
            ConvertOutcome::SetAside {
                reason: Cap::Attachment.message().into()
            }
        );
        // A body over the session total is set aside without being parsed.
        let tx = write_tx(db.connection()).unwrap();
        tx.execute(
            "INSERT INTO sessions(id,title,updated_at,body) VALUES ('huge','Huge',1,'')",
            [],
        )
        .unwrap();
        let raw = format!("{{\"id\":\"huge\",{}", " ".repeat(48 << 20));
        assert_eq!(
            convert_session_in_tx(&tx, "huge", "Huge", 1, &raw, NOW).unwrap(),
            ConvertOutcome::SetAside {
                reason: Cap::SessionTotal.message().into()
            }
        );
        tx.commit().unwrap();
        assert_eq!(quarantined(&db, "huge").unwrap().1.len(), raw.len());
        for id in ["many_attachments", "big_attachment", "huge"] {
            assert_eq!(
                count(
                    &db,
                    "SELECT count(*) FROM chat_events WHERE session_id=?1",
                    id
                ),
                0
            );
        }
    }

    #[test]
    fn an_sql_error_inside_a_session_rolls_back_everything() {
        let mut db = v6_in_memory();
        let sessions: Vec<Value> = ["desktop_plain", "agent_tools", "compacted"]
            .iter()
            .map(|id| fixture(id))
            .collect();
        set_fault(Some(Fault::ConvertError(2)));
        let result = migrate_v6(&mut db, &sessions);
        set_fault(None);
        assert!(
            matches!(result, Err(StorageError::Database(_))),
            "{result:?}"
        );
        for table in [
            "sessions",
            "chat_events",
            "quarantined_sessions",
            "chat_search_keys",
        ] {
            let rows: i64 = db
                .connection()
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(rows, 0, "{table}");
        }
        assert_eq!(db.setting("legacy_sessions_migrated").unwrap(), None);
        // The same import without the fault succeeds whole.
        assert_eq!(migrate_v6(&mut db, &sessions).unwrap(), 0);
        for session in &sessions {
            let id = session["id"].as_str().unwrap();
            assert_eq!(db.load_json_v6(id).unwrap().unwrap(), *session);
        }
    }

    #[test]
    fn system_entries_in_the_transcript_are_kept_verbatim() {
        let db = v6_in_memory();
        let session = fixture("system_in_transcript");
        let outcomes = upgrade(&db, std::slice::from_ref(&session));
        kept(&outcomes["system_in_transcript"]);
        let system = session["contextState"]["transcript"][0].clone();
        assert_eq!(system["role"], "system");
        let models: Vec<Value> = event_bodies(&db, "system_in_transcript")
            .into_iter()
            .filter(|(kind, _)| kind == "model")
            .map(|(_, body)| body)
            .collect();
        assert_eq!(models[0], system);
        assert_eq!(
            db.load_json_v6("system_in_transcript").unwrap().unwrap(),
            session
        );
    }

    #[test]
    fn import_sets_aside_a_bad_session_and_never_resurrects_a_deleted_one() {
        let mut db = v6_in_memory();
        let good = fixture("desktop_plain");
        let bad = fixture("bad_role");
        let mut nameless = fixture("no_context_with_history");
        nameless["id"] = json!("not a valid id!");
        assert_eq!(
            migrate_v6(&mut db, &[good.clone(), bad.clone(), nameless.clone()]).unwrap(),
            2
        );
        assert_eq!(db.load_json_v6("desktop_plain").unwrap().unwrap(), good);
        assert_eq!(
            quarantined(&db, "bad_role").unwrap().2,
            "This conversation contains an invalid message."
        );
        // A body with no usable id is kept under one made for it.
        let made = quarantined(&db, "import_2").unwrap();
        assert_eq!(made.1, serde_json::to_string(&nameless).unwrap());
        assert_eq!(made.2, "Invalid saved item identifier.");
        // Deleted, then imported again: the migration does not run twice.
        db.delete_v6("desktop_plain").unwrap();
        assert_eq!(migrate_v6(&mut db, std::slice::from_ref(&good)).unwrap(), 0);
        assert!(db.list_v6().unwrap().is_empty());
        // Even if it did run again, an id already kept or set aside is skipped.
        db.connection()
            .execute(
                "DELETE FROM settings WHERE key='legacy_sessions_migrated'",
                [],
            )
            .unwrap();
        let other = fixture("compacted");
        db.connection()
            .execute(
                "INSERT INTO sessions(id,title,updated_at,body) VALUES ('compacted','Kept',1,'')",
                [],
            )
            .unwrap();
        assert_eq!(migrate_v6(&mut db, &[bad, other]).unwrap(), 0);
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM chat_events WHERE session_id=?1",
                "compacted"
            ),
            0
        );
        // The compatibility save sets a bad new chat aside too, and says so.
        let mut broken = fixture("desktop_plain");
        broken["id"] = json!("broken");
        broken["messages"][0]["attachments"] = json!(vec![json!({"name":"a.txt"}); 9]);
        assert!(matches!(
            import_session(&db, &broken),
            Err(StorageError::Invalid(_))
        ));
        assert_eq!(
            quarantined(&db, "broken").unwrap().2,
            Cap::AttachmentsPerMessage.message()
        );
        assert!(db.load_json_v6("broken").unwrap().is_none());
    }

    #[test]
    fn import_session_ignores_a_supplied_context_state() {
        let db = v6_in_memory();
        let session = fixture("agent_tools");
        import_session(&db, &session).unwrap();
        let mut without = session.clone();
        without.as_object_mut().unwrap().remove("contextState");
        assert_eq!(db.load_json_v6("agent_tools").unwrap().unwrap(), without);
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM chat_context WHERE session_id=?1",
                "agent_tools"
            ),
            0
        );
        assert!(event_bodies(&db, "agent_tools")
            .iter()
            .all(|(kind, _)| kind != "model"));
        // An id that exists is a conflict at its current revision, and is counted.
        let saves = compat_saves();
        db.update_session_metadata("agent_tools", &MetadataPatch::default(), 1)
            .unwrap();
        assert!(matches!(
            import_session(&db, &session),
            Err(StorageError::Conflict { current: 2 })
        ));
        assert_eq!(compat_saves(), saves + 1);
    }
}
