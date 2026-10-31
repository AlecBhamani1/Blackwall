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
/// The reason for a legacy shape the v6 log cannot hold. Data, not a conversion bug: it is not
/// counted in `upgrade_inline_values`.
const UNSUPPORTED: &str = "unsupported legacy shape";
/// The reason when a body's id is not its row's.
const ID_MISMATCH: &str = "The saved conversation's identifier does not match its record.";
/// What an upgrade that would write a row it did not plan returns. It rolls everything back.
const PLAN_BROKEN: &str =
    "Blackwall could not upgrade a conversation safely. Your existing data has been left in place.";
/// The `contextState` keys v5 writes. `serverUsage` may be absent.
const CONTEXT_KEYS: [&str; 5] = [
    "transcript",
    "checkpoints",
    "coveredMessages",
    "sourceHash",
    "serverUsage",
];
/// The most set-aside chats the compatibility save adds to; past it, it refuses instead.
const MAX_QUARANTINED: i64 = 100;
/// What the compatibility save says when it cannot save a chat and has no room to set it aside.
const NO_ROOM: &str = "This conversation could not be saved as it is. Nothing was saved.";
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
    // Before `validate_session`, whose strict parse would report it as invalid JSON.
    if !context_keys_supported(&session) {
        return Err(unsupported(id, "contextState keys"));
    }
    validate_session(&session)?;
    if session["id"] != id {
        return Err(Fail::SetAside(ID_MISMATCH.into()));
    }
    if let Some(what) = unsupported_shape(&session) {
        return Err(unsupported(id, what));
    }
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
        // A cap keeps its own name (F9); any other refusal is a v5 value v6 does not accept.
        match validate_ui_message(message, message["role"].as_str().unwrap_or_default()) {
            Err(StorageError::Invalid(_)) => return Err(unsupported(id, "message")),
            result => result?,
        }
    }

    // The session row: mode and workspace columns, the other keys in metadata, the v5 body emptied.
    // v6 refuses a mode or workspace that is not a string, or that `fill_mode_workspace` rejects.
    let text = |key: &'static str| match session.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(unsupported(id, key)),
    };
    let (mode, workspace) = (text("mode")?, text("workspace")?);
    match fill_mode_workspace(conn, id, mode, workspace) {
        Err(StorageError::Invalid(_)) => return Err(unsupported(id, "mode or workspace")),
        result => result?,
    }
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
    // A v5 row gets its key here, so keys follow the conversion order (R6-1); a row inserted
    // since the v6 schema already has one.
    if stmt(
        conn,
        "UPDATE sessions SET body='',metadata=?2,
         key=COALESCE(key,(SELECT COALESCE(MAX(key),0)+1 FROM sessions)) WHERE id=?1",
    )?
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
            Kind::Model => append_model(conn, id, &mut c, &row.body)?,
            // An upgraded chat has no runs, so the plan never makes one.
            Kind::UiProgress => return Err(Fail::Storage(StorageError::Invalid(PLAN_BROKEN))),
        };
        // Every reference was proved against the planned seqs.
        if seq != row.seq {
            return Err(Fail::Storage(StorageError::Invalid(PLAN_BROKEN)));
        }
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

/// Sets a chat aside as an unsupported legacy shape, logging which part (never its content).
fn unsupported(id: &str, what: &str) -> Fail {
    eprintln!("warn: chat {id} is set aside: unsupported legacy shape ({what})");
    Fail::SetAside(UNSUPPORTED.into())
}

/// Whether a non-null `contextState` is an object with exactly the keys v5 writes.
fn context_keys_supported(session: &Value) -> bool {
    let Some(context) = session.get("contextState").filter(|value| !value.is_null()) else {
        return true;
    };
    context.as_object().is_some_and(|context| {
        context
            .keys()
            .all(|key| CONTEXT_KEYS.contains(&key.as_str()))
            && CONTEXT_KEYS[..4]
                .iter()
                .all(|key| context.contains_key(*key))
    })
}

/// A legacy shape that A4's view would read as something else: an assistant with a `run` key
/// (a run terminal), `tools` that are not a list, a tool that is not an object or has a `call`
/// key (a reference), or a transcript entry with a `ref` key. Returns which, for the log.
fn unsupported_shape(session: &Value) -> Option<&'static str> {
    for message in session["messages"].as_array().into_iter().flatten() {
        if message["role"] != "assistant" {
            continue;
        }
        if message.get("run").is_some() {
            return Some("an assistant with a run key");
        }
        match message.get("tools") {
            None => {}
            Some(Value::Array(tools)) => {
                if tools
                    .iter()
                    .any(|tool| !tool.is_object() || tool.get("call").is_some())
                {
                    return Some("a tool with a call key");
                }
            }
            Some(_) => return Some("tools that are not a list"),
        }
    }
    let transcript = session["contextState"]["transcript"].as_array();
    if transcript
        .into_iter()
        .flatten()
        .any(|entry| entry.get("ref").is_some())
    {
        return Some("a transcript entry with a ref key");
    }
    None
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
/// `inline` is the stored size of that value, and `net` what the reference saves (ruling, Minor
/// 3: a reference is proposed only when it is smaller than the value).
enum Proposal {
    /// The `model` row at `row` as `{"ref"}` to the UI message at `message`.
    Model {
        row: usize,
        message: usize,
        inline: u64,
        net: u64,
    },
    /// An assistant's `content` as `{turns, tail?}`.
    Content {
        message: usize,
        inline: u64,
        net: u64,
    },
    /// An assistant's tool at `tool` as a call/result residual.
    Tool {
        message: usize,
        tool: usize,
        inline: u64,
        net: u64,
    },
}

fn role(entry: &Value) -> Option<&str> {
    entry.get("role")?.as_str()
}

/// A value's stored size: its JSON.
fn json_len(value: &Value) -> u64 {
    serde_json::to_string(value).map_or(0, |text| text.len()) as u64
}

/// What a reference saves over the value it replaces, when it saves anything.
fn saving(value: &Value, reference: &Value) -> Option<(u64, u64)> {
    let (inline, stored) = (json_len(value), json_len(reference));
    // `then`, not `then_some`: the difference is only taken when it is positive.
    (stored < inline).then(|| (inline, inline - stored))
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

/// Proposes the `model` row at `row` as a ref to the UI message at `message`, if that is smaller.
fn propose_ref(
    proposals: &mut Vec<Proposal>,
    target: &mut [Option<usize>],
    entry: &Value,
    base: i64,
    row: usize,
    message: usize,
) {
    let reference = json!({ "ref": base + message as i64 });
    if let Some((inline, net)) = saving(entry, &reference) {
        target[row] = Some(message);
        proposals.push(Proposal::Model {
            row,
            message,
            inline,
            net,
        });
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
        let Some(candidates) = serde_json::to_string(entry)
            .ok()
            .and_then(|key| forms.get(&key))
        else {
            continue;
        };
        let at = candidates.partition_point(|&index| index < next);
        let Some(&message) = candidates.get(at).or(candidates.first()) else {
            continue;
        };
        propose_ref(
            &mut proposals,
            &mut target,
            &transcript[row],
            base,
            row,
            message,
        );
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
        let content = &message["content"];
        let text = content.as_str().unwrap_or_default();
        let (tail, echo, unproven) = content_form(transcript, &mut turns, text);
        if let Some(echo) = echo {
            propose_ref(
                &mut proposals,
                &mut target,
                &transcript[echo],
                base,
                echo,
                index,
            );
        }
        match tail {
            Some(tail) => {
                let seqs: Vec<i64> = turns.iter().map(|&row| model_seq(row)).collect();
                let mut form = json!({ "turns": seqs });
                if !tail.is_empty() {
                    form["tail"] = json!(tail);
                }
                // Too short to be worth a reference: inline, and not a failure.
                if let Some((inline, net)) = saving(content, &form) {
                    set(&mut bodies[index], "content", form);
                    proposals.push(Proposal::Content {
                        message: index,
                        inline,
                        net,
                    });
                }
            }
            None if unproven => report.inline("content", json_len(content)),
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
            if tool.get("name") != Some(&function["name"])
                || tool.get("arguments") != Some(&function["arguments"])
                || tool.get("output") != output
            {
                report.inline("tool", json_len(tool));
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
            let residual = Value::Object(residual);
            if let Some((inline, net)) = saving(tool, &residual) {
                set_tool(&mut bodies[index], position, residual);
                proposals.push(Proposal::Tool {
                    message: index,
                    tool: position,
                    inline,
                    net,
                });
            }
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
    settle(&mut rows, &proposals, messages, transcript, base, report);
    rows
}

/// Proves every proposal in `rows`, puts each disproved one back inline, and reports the result.
/// The UI side is proved first, since a `model` ref to an assistant resolves through its content.
fn settle(
    rows: &mut [EventRow],
    proposals: &[Proposal],
    messages: &[Value],
    transcript: &[Value],
    base: i64,
    report: &mut ConvertReport,
) {
    let n = messages.len();
    let mut kept = vec![true; proposals.len()];
    for model_side in [false, true] {
        for at in disproved(rows, proposals, model_side, messages, transcript, base) {
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
        let (reason, inline, net) = match *proposal {
            Proposal::Model {
                message,
                inline,
                net,
                ..
            } => {
                let user = messages[message]["role"] == "user";
                (if user { "user" } else { "echo" }, inline, net)
            }
            Proposal::Content { inline, net, .. } => ("content", inline, net),
            Proposal::Tool { inline, net, .. } => ("tool", inline, net),
        };
        if kept {
            report.deduplicated_bytes += net;
        } else {
            report.inline(reason, inline);
        }
    }
}

/// How an assistant's content can be written: `(Some(tail), …)` for `{turns, tail?}` over the
/// final `turns`, with the echo row it found (the restore-appended copy of this message, which
/// then refers to it), and whether model turns were there but could not cover the content.
fn content_form(
    transcript: &[Value],
    turns: &mut Vec<usize>,
    content: &str,
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
    if content.starts_with(&covered) {
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

/// Imports the browser's legacy sessions once, in one transaction. Each one's `contextState` is
/// removed first, as a compatibility save removes it (security M1). Ids already kept or set aside
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
        // Webview-supplied, like a compatibility save: its transcript never becomes model rows.
        let session = &without_context(session);
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

/// A session the webview supplied, with any `contextState` removed (security H4, M1). Only the
/// backend writes model rows; a transcript from the webview is never stored as one.
fn without_context(session: &Value) -> Value {
    let mut session = session.clone();
    if let Some(object) = session.as_object_mut() {
        object.remove("contextState");
    }
    session
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
    let session = without_context(session);
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
    let quarantined: i64 =
        stmt(&tx, "SELECT count(*) FROM quarantined_sessions")?.query_row([], |row| row.get(0))?;
    match convert_session_in_tx(&tx, id, title, updated_at, &raw, now_ms())? {
        ConvertOutcome::Kept(_) => {
            tx.commit()?;
            Ok(())
        }
        // Dropping the transaction rolls the set-aside back: the table stays bounded.
        ConvertOutcome::SetAside { .. } if quarantined >= MAX_QUARANTINED => {
            Err(StorageError::Invalid(NO_ROOM))
        }
        ConvertOutcome::SetAside { .. } => {
            tx.commit()?;
            Err(StorageError::Invalid(SET_ASIDE))
        }
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
    use crate::storage::fixtures::{golden, platform_neutral, v5_sessions, RECALL_QUERIES};
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

    /// Converts `body` under the row id `id`, on a row inserted for it.
    fn convert_one(db: &LocalStore, id: &str, body: &Value) -> ConvertOutcome {
        let tx = write_tx(db.connection()).unwrap();
        tx.execute(
            "INSERT INTO sessions(id,title,updated_at,body) VALUES (?1,'T',1,'')",
            [id],
        )
        .unwrap();
        let raw = serde_json::to_string(body).unwrap();
        let outcome = convert_session_in_tx(&tx, id, "T", 1, &raw, NOW).unwrap();
        tx.commit().unwrap();
        outcome
    }

    fn set_aside_for(reason: &str) -> ConvertOutcome {
        ConvertOutcome::SetAside {
            reason: reason.into(),
        }
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
            .prepare(
                "SELECT kind,body FROM chat_events
                 WHERE sid=(SELECT key FROM sessions WHERE id=?1) ORDER BY seq",
            )
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
        let text = "an answer long enough that a reference to it is smaller";
        let messages = vec![
            json!({"id":"u","role":"user","content":"a question","attachments":[]}),
            json!({"id":"a","role":"assistant","content":text,"attachments":[],"status":"complete",
                "tools":[{"id":"0_0_c1","name":"a_tool_with_a_long_name","arguments":null,"status":"complete"}]}),
        ];
        let transcript = vec![
            json!({"role":"user","content":"a question"}),
            json!({"role":"assistant","content":text,
                "tool_calls":[{"id":"c1","type":"function","function":{"name":"a_tool_with_a_long_name"}}]}),
        ];
        let mut report = ConvertReport::default();
        let rows = plan(10, &messages, &transcript, &mut report);
        assert_eq!(
            report.inline_reasons,
            BTreeMap::from([("content", 1), ("tool", 1)])
        );
        assert_eq!(
            report.inline_bytes,
            json_len(&messages[1]["content"]) + json_len(&messages[1]["tools"][0])
        );
        assert_eq!(rows[1].body, messages[1]);
        assert_eq!(rows[2].body, json!({"ref": 10}));
        assert_eq!(rows[3].body, transcript[1]);
        // Net: the entry's bytes less the reference's.
        assert_eq!(
            report.deduplicated_bytes,
            json_len(&transcript[0]) - json_len(&json!({"ref": 10}))
        );
        let view = crate::storage::view::build_view(&rows, None, &[]).unwrap();
        assert_eq!(view.messages, messages);
    }

    #[test]
    fn a_disproved_model_ref_is_put_back_inline() {
        // A stopped run's echo carries an extra key, so its reference (which resolves to just
        // role and content) is disproved on the model side and put back verbatim.
        let content = "Looking at the files in the project folder.\n\nThe text streamed after the last saved turn";
        let messages = vec![
            json!({"id":"u","role":"user","content":"look at the files","attachments":[]}),
            json!({"id":"a","role":"assistant","content":content,"attachments":[],"status":"stopped"}),
            json!({"id":"v","role":"user","content":"go on","attachments":[]}),
        ];
        let transcript = vec![
            json!({"role":"user","content":"look at the files"}),
            json!({"role":"assistant","content":"Looking at the files in the project folder.",
                "tool_calls":[{"id":"c1","type":"function","function":{"name":"ls","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"c1","content":"a.txt"}),
            json!({"role":"assistant","content":content,"name":"extra"}),
            json!({"role":"user","content":"go on"}),
        ];
        let mut report = ConvertReport::default();
        let rows = plan(0, &messages, &transcript, &mut report);
        assert_eq!(report.inline_reasons, BTreeMap::from([("echo", 1)]));
        assert_eq!(report.inline_bytes, json_len(&transcript[3]));
        assert_eq!(rows[3 + 3].body, transcript[3]);
        // The content kept its turns and a tail; the two user refs held.
        assert_eq!(
            rows[1].body["content"],
            json!({"turns": [4], "tail": "\n\nThe text streamed after the last saved turn"})
        );
        assert_eq!(rows[3].body, json!({"ref": 0}));
        assert_eq!(rows[7].body, json!({"ref": 2}));
        let context = ContextRow {
            model_from: 3,
            model_len: 5,
            covered_messages: 0,
            source_hash: String::new(),
            server_usage: None,
        };
        let view = crate::storage::view::build_view(&rows, Some(&context), &[]).unwrap();
        assert_eq!(view.messages, messages);
        assert_eq!(view.context.unwrap()["transcript"], json!(transcript));

        // A user ref that does not resolve to its entry is put back the same way. The matcher
        // never proposes one (an entry with an extra key is not any message's form), so the
        // proof step is driven directly.
        let user = json!({"id":"u","role":"user","content":"q","attachments":[]});
        let entry = json!({"role":"user","content":"q","name":"extra"});
        let mut report = ConvertReport::default();
        let planned = plan(
            0,
            std::slice::from_ref(&user),
            std::slice::from_ref(&entry),
            &mut report,
        );
        assert_eq!(planned[1].body, entry);
        assert_eq!(report, ConvertReport::default());
        let mut rows = vec![
            EventRow {
                seq: 0,
                kind: Kind::UiUser,
                body: user.clone(),
            },
            EventRow {
                seq: 1,
                kind: Kind::Model,
                body: json!({"ref": 0}),
            },
        ];
        let (inline, net) = saving(&entry, &json!({"ref": 0})).unwrap();
        let proposals = [Proposal::Model {
            row: 0,
            message: 0,
            inline,
            net,
        }];
        let mut report = ConvertReport::default();
        settle(
            &mut rows,
            &proposals,
            &[user],
            std::slice::from_ref(&entry),
            0,
            &mut report,
        );
        assert_eq!(rows[1].body, entry);
        assert_eq!(report.inline_reasons, BTreeMap::from([("user", 1)]));
        assert_eq!(report.deduplicated_bytes, 0);
    }

    #[test]
    fn a_user_entry_matches_backwards_when_nothing_later_does() {
        let messages = vec![
            json!({"id":"one","role":"user","content":"the first question","attachments":[]}),
            json!({"id":"two","role":"user","content":"the second question","attachments":[]}),
        ];
        // The second message's entry comes first; the first one's is then found from 0.
        let transcript = vec![
            json!({"role":"user","content":"the second question"}),
            json!({"role":"user","content":"the first question"}),
        ];
        let mut report = ConvertReport::default();
        let rows = plan(5, &messages, &transcript, &mut report);
        assert_eq!(rows[2].body, json!({"ref": 6}));
        assert_eq!(rows[3].body, json!({"ref": 5}));
        assert!(report.inline_reasons.is_empty());
        let context = ContextRow {
            model_from: 7,
            model_len: 2,
            covered_messages: 0,
            source_hash: String::new(),
            server_usage: None,
        };
        let view = crate::storage::view::build_view(&rows, Some(&context), &[]).unwrap();
        assert_eq!(view.context.unwrap()["transcript"], json!(transcript));
    }

    #[test]
    fn a_reference_is_made_only_when_it_saves_space() {
        // `"ok"` is smaller than any `{"turns":[…]}`: it stays inline, and that is no failure.
        let messages = vec![
            json!({"id":"u","role":"user","content":"is it fine?","attachments":[]}),
            json!({"id":"a","role":"assistant","content":"ok","attachments":[],"status":"complete"}),
        ];
        let transcript = vec![
            json!({"role":"user","content":"is it fine?"}),
            json!({"role":"assistant","content":"ok"}),
        ];
        let mut report = ConvertReport::default();
        let rows = plan(0, &messages, &transcript, &mut report);
        assert_eq!(rows[1].body, messages[1]);
        assert_eq!(rows[2].body, json!({"ref": 0}));
        // Only the user ref saved anything, and the stats count what it saved, net.
        assert_eq!(report.inline_bytes, 0);
        assert!(report.inline_reasons.is_empty());
        assert_eq!(
            report.deduplicated_bytes,
            json_len(&transcript[0]) - json_len(&json!({"ref": 0}))
        );
        // A long one is referenced, and counted net of its form.
        let long = "a reply long enough that its turns reference is the smaller copy";
        let mut messages = messages;
        messages[1]["content"] = json!(long);
        let mut transcript = transcript;
        transcript[1]["content"] = json!(long);
        let mut report = ConvertReport::default();
        let rows = plan(0, &messages, &transcript, &mut report);
        assert_eq!(rows[1].body["content"], json!({"turns": [3]}));
        assert_eq!(
            report.deduplicated_bytes,
            json_len(&transcript[0]) - json_len(&json!({"ref": 0})) + json_len(&json!(long))
                - json_len(&json!({"turns": [3]}))
        );
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
        assert_eq!(platform_neutral(v6_recall(&db)), golden("v5_recall.json"));
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
                // A reference costs at least `{"turns":[n]}`, so a text this short is cheaper stored
                // again than referenced (ruling, Minor 3).
                let short = marker.len() < 16;
                assert!(
                    found == 1 || (short && found == 2),
                    "{id}: {marker:?} is stored {found} times"
                );
            }
        }
        // The stopped run's echo is a reference to its message.
        let rows = event_bodies(&db, "stopped_then_continued");
        assert!(rows
            .iter()
            .any(|(kind, body)| kind == "model" && body == &json!({"ref": 1})));
    }

    #[test]
    fn content_that_extends_its_turns_keeps_the_extension_as_a_tail_whatever_the_status() {
        // The fixture's complete row reads "Checking\n\nAll good, edited after the run": its turns
        // are a prefix, so the rest is a tail (ruling: any status, still proven by resolution).
        let db = v6_in_memory();
        // Turns long enough that a reference pays off.
        let long = "All good, and here is a longer line so that a reference saves space";
        let session: Value = serde_json::from_str(
            &fixture("text_tool_mismatch")
                .to_string()
                .replace("All good", long),
        )
        .unwrap();
        assert_eq!(session["messages"][1]["status"], "complete");
        assert_eq!(
            session["messages"][1]["content"],
            format!("Checking\n\n{long}, edited after the run")
        );
        let outcomes = upgrade(&db, std::slice::from_ref(&session));
        let report = kept(&outcomes["text_tool_mismatch"]);
        assert_eq!(report.inline_bytes, 0);
        assert!(report.inline_reasons.is_empty());
        assert_eq!(
            db.load_json_v6("text_tool_mismatch").unwrap().unwrap(),
            session
        );
        let rows = event_bodies(&db, "text_tool_mismatch");
        assert_eq!(rows[1].1["content"]["tail"], ", edited after the run");
        assert!(rows[1].1["tools"][0].get("call").is_some());
    }

    #[test]
    fn content_that_does_not_start_with_its_turns_stays_inline_and_says_why() {
        let db = v6_in_memory();
        let mut session = fixture("text_tool_mismatch");
        session["messages"][1]["content"] = json!("Edited\n\nAll good, edited after the run");
        let outcomes = upgrade(&db, std::slice::from_ref(&session));
        let report = kept(&outcomes["text_tool_mismatch"]);
        assert_eq!(report.inline_reasons, BTreeMap::from([("content", 1)]));
        assert_eq!(
            report.inline_bytes,
            json_len(&json!("Edited\n\nAll good, edited after the run"))
        );
        assert_eq!(
            db.load_json_v6("text_tool_mismatch").unwrap().unwrap(),
            session
        );
        // The tool is still a reference: only the content stayed inline.
        let rows = event_bodies(&db, "text_tool_mismatch");
        assert_eq!(
            rows[1].1["content"],
            "Edited\n\nAll good, edited after the run"
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
        // The only chat was set aside, so nothing of it is left in any table.
        for table in [
            "chat_events",
            "chat_context",
            "chat_checkpoints",
            "chat_search_keys",
        ] {
            let sql = format!("SELECT count(*) FROM {table} WHERE ?1 IS NOT NULL");
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
        // Every chat was set aside, so no event of any of them is left.
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM chat_events WHERE ?1 IS NOT NULL",
                "any"
            ),
            0
        );
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
        // The same import without the fault succeeds whole, each chat without its webview
        // `contextState` (security M1).
        assert_eq!(migrate_v6(&mut db, &sessions).unwrap(), 0);
        for session in &sessions {
            let id = session["id"].as_str().unwrap();
            assert_eq!(
                db.load_json_v6(id).unwrap().unwrap(),
                without_context(session)
            );
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
        assert_eq!(
            db.load_json_v6("desktop_plain").unwrap().unwrap(),
            without_context(&good)
        );
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
                "SELECT count(*) FROM chat_events WHERE sid=(SELECT key FROM sessions WHERE id=?1)",
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

    /// R4-7: the webview cannot append model rows. Both of its ways to hand the backend a whole
    /// chat, the compatibility save and the browser import, drop the `contextState` it supplied,
    /// so its transcript never becomes model rows (security H4, M1).
    #[test]
    fn import_session_ignores_a_supplied_context_state() {
        let mut imported = v6_in_memory();
        migrate_v6(&mut imported, &[fixture("agent_tools")]).unwrap();
        let db = v6_in_memory();
        let session = fixture("agent_tools");
        import_session(&db, &session).unwrap();
        let mut without = session.clone();
        without.as_object_mut().unwrap().remove("contextState");
        for db in [&db, &imported] {
            assert_eq!(db.load_json_v6("agent_tools").unwrap().unwrap(), without);
            assert_eq!(
                count(
                    db,
                    "SELECT count(*) FROM chat_context WHERE session_id=?1",
                    "agent_tools"
                ),
                0
            );
            assert!(event_bodies(db, "agent_tools")
                .iter()
                .all(|(kind, _)| kind != "model"));
        }
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

    #[test]
    fn each_count_cap_is_named_when_a_legacy_chat_is_over_it() {
        let db = v6_in_memory();
        let mut messages = fixture("desktop_plain");
        let one = messages["messages"][0].clone();
        messages["messages"] = json!(vec![one; 2001]);
        assert_eq!(
            convert_one(&db, "desktop_plain", &messages),
            set_aside_for(Cap::UiMessages.message())
        );
        let mut entries = fixture("agent_tools");
        let entry = json!({"role":"user","content":"x"});
        entries["contextState"]["transcript"] = json!(vec![entry; 20_001]);
        assert_eq!(
            convert_one(&db, "agent_tools", &entries),
            set_aside_for(Cap::ModelEntries.message())
        );
        let mut checkpoints = fixture("compacted");
        let checkpoint = checkpoints["contextState"]["checkpoints"][0].clone();
        checkpoints["contextState"]["checkpoints"] = json!(vec![checkpoint; 101]);
        assert_eq!(
            convert_one(&db, "compacted", &checkpoints),
            set_aside_for(Cap::Checkpoints.message())
        );
    }

    #[test]
    fn an_unsupported_legacy_shape_is_set_aside_and_not_counted_as_a_bug() {
        let mut cases: Vec<(&str, Value)> = vec![];
        let mut case = |name: &'static str, base: &str, edit: &dyn Fn(&mut Value)| {
            let mut session = fixture(base);
            edit(&mut session);
            cases.push((name, session));
        };
        case("run key", "agent_tools", &|s| {
            s["messages"][1]["run"] = json!(1)
        });
        case("tool call key", "agent_tools", &|s| {
            s["messages"][1]["tools"][0]["call"] = json!([0, 0]);
        });
        case("tool not an object", "agent_tools", &|s| {
            s["messages"][1]["tools"][0] = json!("tool");
        });
        case("tools not a list", "agent_tools", &|s| {
            s["messages"][1]["tools"] = json!({});
        });
        case("transcript ref key", "desktop_plain", &|s| {
            s["contextState"]["transcript"][1]["ref"] = json!(0);
        });
        case("extra context key", "desktop_plain", &|s| {
            s["contextState"]["extra"] = json!(1);
        });
        case("missing context key", "desktop_plain", &|s| {
            s["contextState"]
                .as_object_mut()
                .unwrap()
                .remove("coveredMessages");
        });
        case("context not an object", "desktop_plain", &|s| {
            s["contextState"] = json!("state");
        });
        case("relative workspace", "agent_tools", &|s| {
            s["workspace"] = json!("relative/dir");
        });
        case("null mode", "desktop_plain", &|s| s["mode"] = Value::Null);
        case("empty mode", "desktop_plain", &|s| s["mode"] = json!(""));
        case("attachment not an object", "desktop_plain", &|s| {
            s["messages"][0]["attachments"] = json!([1]);
        });
        let db = v6_in_memory();
        let bugs = upgrade_inline_values();
        for (at, (name, session)) in cases.iter().enumerate() {
            let id = format!("case_{at}");
            let mut session = session.clone();
            session["id"] = json!(id);
            assert_eq!(
                convert_one(&db, &id, &session),
                set_aside_for(UNSUPPORTED),
                "{name}"
            );
            // Every case so far was set aside, so no event is left at all.
            assert_eq!(
                count(
                    &db,
                    "SELECT count(*) FROM chat_events WHERE ?1 IS NOT NULL",
                    &id
                ),
                0
            );
        }
        assert_eq!(upgrade_inline_values(), bugs);
        // An optional `serverUsage` may be absent: still supported.
        let mut session = fixture("desktop_plain");
        session["contextState"]
            .as_object_mut()
            .unwrap()
            .remove("serverUsage");
        let outcomes = upgrade(&db, std::slice::from_ref(&session));
        kept(&outcomes["desktop_plain"]);
        assert_eq!(db.load_json_v6("desktop_plain").unwrap().unwrap(), session);
    }

    #[test]
    fn a_body_whose_id_is_not_its_rows_is_set_aside() {
        let db = v6_in_memory();
        assert_eq!(
            convert_one(&db, "other_row", &fixture("desktop_plain")),
            set_aside_for(ID_MISMATCH)
        );
        assert_eq!(upgrade_inline_values(), 0);
    }

    #[test]
    fn the_compatibility_save_stops_setting_aside_at_its_cap() {
        let db = v6_in_memory();
        let mut broken = fixture("desktop_plain");
        broken["messages"][0]["attachments"] = json!(vec![json!({"name":"a.txt"}); 9]);
        for n in 0..MAX_QUARANTINED - 1 {
            db.connection()
                .execute(
                    "INSERT INTO quarantined_sessions(id,title,body,reason,found_at) VALUES (?1,'T','{}','r',1)",
                    [format!("old_{n}")],
                )
                .unwrap();
        }
        let total = || -> i64 {
            db.connection()
                .query_row("SELECT count(*) FROM quarantined_sessions", [], |r| {
                    r.get(0)
                })
                .unwrap()
        };
        // Below the cap a bad chat is still set aside.
        broken["id"] = json!("broken_one");
        assert!(matches!(
            import_session(&db, &broken),
            Err(StorageError::Invalid(SET_ASIDE))
        ));
        assert_eq!(total(), MAX_QUARANTINED);
        // At the cap it is refused, and nothing is written.
        broken["id"] = json!("broken_two");
        assert!(matches!(
            import_session(&db, &broken),
            Err(StorageError::Invalid(NO_ROOM))
        ));
        assert_eq!(total(), MAX_QUARANTINED);
        assert!(quarantined(&db, "broken_two").is_none());
        assert!(db.load_json_v6("broken_two").unwrap().is_none());
        // A good chat still saves.
        import_session(&db, &fixture("desktop_plain")).unwrap();
    }
}
