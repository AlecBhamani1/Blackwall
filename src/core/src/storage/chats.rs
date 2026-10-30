//! The v6 chat event-log schema, the caps that guard it and the session API on top of it.
//!
//! Dormant: `LocalStore::open` still opens v5. Nothing here runs outside tests until the upgrade lands.
#![cfg_attr(not(test), allow(dead_code))] // the upgrade and the run recorder that call it land later

use super::search;
use super::view::{build_view, ContextRow, EventRow, Kind, ViewError};
use super::{
    validate_id, LocalStore, StorageError, MAX_CHECKPOINTS, MAX_CHECKPOINT_BYTES, MAX_EVENT_BYTES,
    MAX_METADATA_BYTES, MAX_MODEL_ENTRIES, MAX_SESSIONS, MAX_SESSION_BYTES, MAX_UI_MESSAGES,
};
use crate::{
    context::ContextState,
    protocol::{MAX_ATTACHMENTS_PER_MESSAGE, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENT_TOTAL_BYTES},
};
use rusqlite::{
    params, CachedStatement, Connection, OptionalExtension, Transaction, TransactionBehavior,
};
use serde_json::{json, Map, Value};
use std::path::Path;

/// The limit a refused write ran into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    Attachment,
    AttachmentsPerMessage,
    AttachmentTotal,
    Event,
    Checkpoint,
    Checkpoints,
    UiMessages,
    ModelEntries,
    SessionTotal,
    Metadata,
    Sessions,
}

impl Cap {
    /// What the user is told. Each text names its own limit.
    pub fn message(self) -> &'static str {
        match self {
            Self::Attachment => "An attachment is over the 15 MiB limit.",
            Self::AttachmentsPerMessage => "A message can have at most 8 attachments.",
            Self::AttachmentTotal => "A message's attachments are over the 30 MiB limit.",
            Self::Event => "A saved message or model step is over its 32 MiB limit.",
            Self::Checkpoint => "A context checkpoint is over its 1 MiB limit.",
            Self::Checkpoints => "This conversation has reached its 100-checkpoint limit.",
            Self::UiMessages => "This conversation has reached its 2,000-message limit. Export it and start a new conversation.",
            Self::ModelEntries => "This conversation has reached its 20,000-step model history limit. Export it and start a new conversation.",
            Self::SessionTotal => "This conversation is too large to save. Export it before closing Blackwall.",
            Self::Metadata => "This conversation's details are over their 64 KiB limit.",
            Self::Sessions => "Your history has reached 500 conversations. Export or delete an older conversation before starting another.",
        }
    }
}

/// Turns a v5 database into the v6 shape. The caller owns the surrounding transaction.
///
/// The v5 `sessions.body` column stays, and new rows leave it empty: no `DROP COLUMN`, no rebuild.
#[cfg_attr(not(test), allow(dead_code))] // the upgrade that calls it lands later
pub(crate) fn apply_v6_schema(tx: &Connection) -> Result<(), StorageError> {
    tx.execute_batch(
        "DROP TRIGGER IF EXISTS message_insert;
        DROP TRIGGER IF EXISTS message_delete;
        DROP TABLE IF EXISTS message_search;
        DROP TABLE IF EXISTS session_messages;
        DROP TABLE IF EXISTS indexed_sessions;
        ALTER TABLE sessions ADD COLUMN mode TEXT;
        ALTER TABLE sessions ADD COLUMN workspace TEXT;
        ALTER TABLE sessions ADD COLUMN metadata TEXT NOT NULL DEFAULT '{}';
        ALTER TABLE sessions ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;
        ALTER TABLE sessions ADD COLUMN next_seq INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE sessions ADD COLUMN total_bytes INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE sessions ADD COLUMN ui_count INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE sessions ADD COLUMN model_count INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE sessions ADD COLUMN search_bytes INTEGER NOT NULL DEFAULT 0;
        CREATE TABLE chat_events (
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            seq INTEGER NOT NULL,
            kind TEXT NOT NULL CHECK (kind IN ('ui_user','model','ui_progress','ui_assistant')),
            body TEXT NOT NULL,
            PRIMARY KEY (session_id, seq)) WITHOUT ROWID;
        CREATE TABLE chat_context (
            session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
            model_from INTEGER NOT NULL,
            model_len INTEGER NOT NULL,
            covered_messages INTEGER NOT NULL,
            source_hash TEXT NOT NULL,
            server_usage TEXT);
        CREATE TABLE chat_checkpoints (
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            ordinal INTEGER NOT NULL,
            body TEXT NOT NULL,
            PRIMARY KEY (session_id, ordinal)) WITHOUT ROWID;
        CREATE TABLE chat_search_keys (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            seq INTEGER NOT NULL,
            message_index INTEGER NOT NULL,
            message_id TEXT NOT NULL,
            role TEXT NOT NULL,
            UNIQUE (session_id, seq));
        CREATE VIRTUAL TABLE chat_search USING fts5(content, content='', contentless_delete=1);
        INSERT INTO chat_search(chat_search, rank) VALUES ('secure-delete', 1);
        CREATE TRIGGER chat_search_keys_ad AFTER DELETE ON chat_search_keys BEGIN
            DELETE FROM chat_search WHERE rowid = old.id; END;
        CREATE TABLE quarantined_sessions (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            reason TEXT NOT NULL,
            found_at INTEGER NOT NULL);",
    )?;
    Ok(())
}

// ---- telemetry and statement counting ----

static COUNTER_REPAIRS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
thread_local! {
    // Under test each test runs on its own thread, so one test's repairs never leak into another
    // test that asserts none happened.
    static REPAIRS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static STATEMENTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many times a load found a session's counters wrong and rewrote them from its rows.
/// Process-wide (per test thread under `cfg(test)`).
pub fn counter_repairs() -> u64 {
    #[cfg(test)]
    return REPAIRS.with(std::cell::Cell::get);
    #[cfg(not(test))]
    COUNTER_REPAIRS.load(std::sync::atomic::Ordering::Relaxed)
}

fn note_counter_repair(id: &str) {
    COUNTER_REPAIRS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    REPAIRS.with(|n| n.set(n.get() + 1));
    eprintln!("warn: counter_repairs: rewrote the counters of chat {id} from its rows");
}

/// How many chat-store statements this thread has run, for the statement-count tests.
#[cfg(test)]
pub(crate) fn statements() -> u64 {
    STATEMENTS.with(std::cell::Cell::get)
}

/// Every chat-store statement goes through here, so the tests can count them.
pub(super) fn stmt<'c>(
    conn: &'c Connection,
    sql: &str,
) -> Result<CachedStatement<'c>, StorageError> {
    #[cfg(test)]
    STATEMENTS.with(|n| n.set(n.get() + 1));
    Ok(conn.prepare_cached(sql)?)
}

// ---- the public session types ----

/// A new session's columns. `mode` and `workspace` are set once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSession {
    pub id: String,
    pub title: String,
    pub mode: Option<String>,
    pub workspace: Option<String>,
    pub updated_at: i64,
}

/// What the webview may change: the title, and the whole `metadata` object.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MetadataPatch {
    pub title: Option<String>,
    pub metadata: Option<Map<String, Value>>,
}

/// The model context a chat holds.
#[derive(Clone, Debug, PartialEq)]
pub enum StoredContext {
    None,
    Valid(ContextState),
    /// Stored, but it does not parse, validate or resolve. Rows are left as they are.
    Invalid {
        reason: &'static str,
    },
}

/// A chat as the log rebuilds it.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredChat {
    pub id: String,
    pub title: String,
    pub updated_at: i64,
    pub mode: Option<String>,
    pub workspace: Option<String>,
    pub metadata: Map<String, Value>,
    pub revision: i64,
    pub messages: Vec<Value>,
    pub context: StoredContext,
    pub open_run: bool,
}

/// A chat's UI messages, and the index of the one a still-open run has folded.
pub(crate) type SearchSource = (Vec<Value>, Option<u64>);

/// A session's running totals, kept on its `sessions` row and moved with every write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Counters {
    pub next_seq: i64,
    /// Event bodies, checkpoint bodies and a non-empty `metadata`, in bytes.
    pub total_bytes: u64,
    /// UI messages, with each run's assistant slot counted from `begin_run`.
    pub ui_count: u64,
    pub model_count: u64,
    pub search_bytes: u64,
}

// ---- messages the owner may see ----

const BAD_MESSAGE: &str = "This conversation contains an invalid message.";
const UNREADABLE: &str =
    "This conversation's saved history can't be read. Your existing data has been left in place.";
const CONTEXT_BAD: &str = "The saved model context is not valid.";
const CONTEXT_TOO_SHORT: &str = "The saved model history is shorter than its context says.";
const CONTEXT_REF: &str = "The saved model context refers to an entry that cannot be read.";

fn int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn unsigned(value: i64) -> Result<u64, StorageError> {
    u64::try_from(value).map_err(|_| StorageError::Invalid(UNREADABLE))
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::UiUser => "ui_user",
        Kind::Model => "model",
        Kind::UiProgress => "ui_progress",
        Kind::UiAssistant => "ui_assistant",
    }
}

fn parse_kind(name: &str) -> Result<Kind, StorageError> {
    Ok(match name {
        "ui_user" => Kind::UiUser,
        "model" => Kind::Model,
        "ui_progress" => Kind::UiProgress,
        "ui_assistant" => Kind::UiAssistant,
        _ => return Err(StorageError::Invalid(UNREADABLE)),
    })
}

/// A `metadata` of `{}` is the column's default and counts for nothing, so a bare row is balanced.
fn metadata_bytes(text: &str) -> u64 {
    if text == "{}" {
        0
    } else {
        text.len() as u64
    }
}

// ---- transactions and counters ----

/// Every chat write is one of these: it takes the writer lock up front, so the counters it reads
/// are the counters it writes.
pub(crate) fn write_tx(connection: &Connection) -> Result<Transaction<'_>, StorageError> {
    Ok(Transaction::new_unchecked(
        connection,
        TransactionBehavior::Immediate,
    )?)
}

/// The session's counters, or `NotFound` when it has no row.
pub(crate) fn read_counters(tx: &Connection, id: &str) -> Result<Counters, StorageError> {
    let raw = stmt(
        tx,
        "SELECT next_seq,total_bytes,ui_count,model_count,search_bytes FROM sessions WHERE id=?1",
    )?
    .query_row([id], |row| {
        Ok([
            row.get::<_, i64>(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
        ])
    })
    .optional()?
    .ok_or(StorageError::NotFound)?;
    Ok(Counters {
        next_seq: raw[0],
        total_bytes: unsigned(raw[1])?,
        ui_count: unsigned(raw[2])?,
        model_count: unsigned(raw[3])?,
        search_bytes: unsigned(raw[4])?,
    })
}

/// Stores the counters. `updated_at` only ever moves forward.
pub(crate) fn write_counters(
    tx: &Connection,
    id: &str,
    c: &Counters,
    updated_at: i64,
) -> Result<(), StorageError> {
    let changed = stmt(
        tx,
        "UPDATE sessions SET next_seq=?2,total_bytes=?3,ui_count=?4,model_count=?5,search_bytes=?6,
         updated_at=max(updated_at,?7) WHERE id=?1",
    )?
    .execute(params![
        id,
        c.next_seq,
        int(c.total_bytes),
        int(c.ui_count),
        int(c.model_count),
        int(c.search_bytes),
        updated_at
    ])?;
    if changed == 0 {
        return Err(StorageError::NotFound);
    }
    Ok(())
}

/// The counters as the rows say they should be. It reads every row, so it is for repairs and
/// tests, never for a normal write. `search_bytes` comes from the rebuilt UI messages.
#[derive(Default)]
struct Tally {
    next_seq: i64,
    total_bytes: u64,
    ui_user: u64,
    ui_standalone: u64,
    run_starts: u64,
    model_count: u64,
}

impl Tally {
    /// `marks_run` is whether a `ui_progress` row is a run start or a `ui_assistant` row belongs to
    /// a run (is a terminal); see [`marks_run`].
    fn event(&mut self, seq: i64, kind: Kind, len: usize, marks_run: bool) {
        self.next_seq = self.next_seq.max(seq + 1);
        self.total_bytes += len as u64;
        match kind {
            Kind::UiUser => self.ui_user += 1,
            Kind::Model => self.model_count += 1,
            Kind::UiProgress => self.run_starts += u64::from(marks_run),
            Kind::UiAssistant => self.ui_standalone += u64::from(!marks_run),
        }
    }

    fn finish(self, metadata: &str, checkpoint_bytes: u64, search_bytes: u64) -> Counters {
        Counters {
            next_seq: self.next_seq,
            total_bytes: self.total_bytes + checkpoint_bytes + metadata_bytes(metadata),
            // A run's start reserves its assistant slot; its terminal fills it.
            ui_count: self.ui_user + self.ui_standalone + self.run_starts,
            model_count: self.model_count,
            search_bytes,
        }
    }
}

/// Whether a `ui_progress` row is a run start (`start`) or a `ui_assistant` row is a run terminal (`run`).
fn marks_run(kind: Kind, body: &Value) -> bool {
    match kind {
        Kind::UiProgress => body.get("start").is_some(),
        Kind::UiAssistant => body.get("run").is_some(),
        Kind::UiUser | Kind::Model => false,
    }
}

/// Recomputes the counters from the session's rows.
pub(crate) fn recount(tx: &Connection, id: &str) -> Result<Counters, StorageError> {
    let (metadata, search_bytes): (String, i64) =
        stmt(tx, "SELECT metadata,search_bytes FROM sessions WHERE id=?1")?
            .query_row([id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?
            .ok_or(StorageError::NotFound)?;
    let mut tally = Tally::default();
    let mut events = vec![];
    let mut statement = stmt(
        tx,
        "SELECT seq,kind,body FROM chat_events WHERE session_id=?1 ORDER BY seq",
    )?;
    let mut rows = statement.query([id])?;
    while let Some(row) = rows.next()? {
        let seq: i64 = row.get(0)?;
        let kind = parse_kind(&row.get::<_, String>(1)?)?;
        let text: String = row.get(2)?;
        let body: Value = serde_json::from_str(&text)?;
        tally.event(seq, kind, text.len(), marks_run(kind, &body));
        events.push(EventRow { seq, kind, body });
    }
    drop(rows);
    // The search allowance is what the rebuilt messages index to. If the log cannot be rebuilt
    // there is nothing to derive it from, and a load of this chat fails anyway: keep what is stored.
    let search_bytes = match build_view(&events, None, &[]) {
        Ok(view) => search::derived_bytes(
            &view.messages,
            view.open_run.as_ref().map(|open| open.message_index),
        ),
        Err(_) => unsigned(search_bytes)?,
    };
    let checkpoint_bytes: i64 = stmt(
        tx,
        "SELECT COALESCE(SUM(length(CAST(body AS BLOB))),0) FROM chat_checkpoints WHERE session_id=?1",
    )?
    .query_row([id], |row| row.get(0))?;
    Ok(tally.finish(&metadata, unsigned(checkpoint_bytes)?, search_bytes))
}

// ---- message rules ----

/// The one copy of the rules for a UI message the backend is asked to store.
pub(crate) fn validate_ui_message(message: &Value, role: &str) -> Result<(), StorageError> {
    let bad = || StorageError::Invalid(BAD_MESSAGE);
    let object = message.as_object().ok_or_else(bad)?;
    object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| (1..=128).contains(&id.len()))
        .ok_or_else(bad)?;
    if object.get("role").and_then(Value::as_str) != Some(role) {
        return Err(bad());
    }
    object
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(bad)?;
    let attachments = object
        .get("attachments")
        .and_then(Value::as_array)
        .ok_or_else(bad)?;
    if attachments.len() > MAX_ATTACHMENTS_PER_MESSAGE {
        return Err(StorageError::Limit(Cap::AttachmentsPerMessage));
    }
    let mut total = 0u64;
    for attachment in attachments {
        let attachment = attachment.as_object().ok_or_else(bad)?;
        // A missing `sizeBytes` counts as 0.
        let size = match attachment.get("sizeBytes") {
            None => 0,
            Some(size) => size.as_u64().ok_or_else(bad)?,
        };
        if size > MAX_ATTACHMENT_BYTES {
            return Err(StorageError::Limit(Cap::Attachment));
        }
        total = total.saturating_add(size);
    }
    if total > MAX_ATTACHMENT_TOTAL_BYTES {
        return Err(StorageError::Limit(Cap::AttachmentTotal));
    }
    if serde_json::to_vec(message)?.len() > MAX_EVENT_BYTES {
        return Err(StorageError::Limit(Cap::Event));
    }
    Ok(())
}

// ---- appending ----

/// Adds one event: a single `INSERT`, with the caps checked first and nothing written on a
/// refusal. It moves `next_seq`, `total_bytes` and (for a `model` row) `model_count` in `c`; the
/// caller stores `c` with [`write_counters`] in the same transaction. It never counts a UI
/// message: [`append_ui_user`], [`append_run_start`] and [`append_ui_assistant`] do.
pub(crate) fn append(
    tx: &Connection,
    id: &str,
    c: &mut Counters,
    kind: Kind,
    body: &Value,
) -> Result<i64, StorageError> {
    let text = serde_json::to_string(body)?;
    let len = text.len();
    if len > MAX_EVENT_BYTES {
        return Err(StorageError::Limit(Cap::Event));
    }
    if c.total_bytes + len as u64 > MAX_SESSION_BYTES {
        return Err(StorageError::Limit(Cap::SessionTotal));
    }
    let model = u64::from(kind == Kind::Model);
    if c.model_count + model > MAX_MODEL_ENTRIES {
        return Err(StorageError::Limit(Cap::ModelEntries));
    }
    let seq = c.next_seq;
    stmt(
        tx,
        "INSERT INTO chat_events(session_id,seq,kind,body) VALUES (?1,?2,?3,?4)",
    )?
    .execute(params![id, seq, kind_name(kind), text])?;
    c.next_seq += 1;
    c.total_bytes += len as u64;
    c.model_count += model;
    Ok(seq)
}

fn reserve_ui(c: &Counters, slots: u64) -> Result<(), StorageError> {
    if c.ui_count + slots > MAX_UI_MESSAGES {
        return Err(StorageError::Limit(Cap::UiMessages));
    }
    Ok(())
}

/// Adds a `ui_user` row and counts one UI message.
pub(crate) fn append_ui_user(
    tx: &Connection,
    id: &str,
    c: &mut Counters,
    body: &Value,
) -> Result<i64, StorageError> {
    reserve_ui(c, 1)?;
    let seq = append(tx, id, c, Kind::UiUser, body)?;
    c.ui_count += 1;
    Ok(seq)
}

/// Adds a `model` row. It counts no UI message.
pub(crate) fn append_model(
    tx: &Connection,
    id: &str,
    c: &mut Counters,
    body: &Value,
) -> Result<i64, StorageError> {
    append(tx, id, c, Kind::Model, body)
}

/// Adds the `ui_progress` start row of a run, `{"start":{"id","createdAt","user"}}`. This is the
/// assistant half of `begin_run`'s two reserved UI messages (the `ui_user` is the other), so the
/// run's terminal later adds none.
pub(crate) fn append_run_start(
    tx: &Connection,
    id: &str,
    c: &mut Counters,
    assistant_id: &str,
    created_at: i64,
    user_seq: i64,
) -> Result<i64, StorageError> {
    reserve_ui(c, 1)?;
    let body = json!({"start": {"id": assistant_id, "createdAt": created_at, "user": user_seq}});
    let seq = append(tx, id, c, Kind::UiProgress, &body)?;
    c.ui_count += 1;
    Ok(seq)
}

/// Adds a `ui_assistant` row. A run terminal (it has `run`) fills the slot its start reserved and
/// counts nothing; a standalone message counts one.
pub(crate) fn append_ui_assistant(
    tx: &Connection,
    id: &str,
    c: &mut Counters,
    body: &Value,
) -> Result<i64, StorageError> {
    let reserved = marks_run(Kind::UiAssistant, body);
    if !reserved {
        reserve_ui(c, 1)?;
    }
    let seq = append(tx, id, c, Kind::UiAssistant, body)?;
    c.ui_count += u64::from(!reserved);
    Ok(seq)
}

/// The next checkpoint ordinal, which is also the count (ordinals are contiguous from 0): one
/// primary-key seek that reads no body.
const NEXT_ORDINAL_SQL: &str =
    "SELECT COALESCE(MAX(ordinal)+1,0) FROM chat_checkpoints WHERE session_id=?1";
const CHECKPOINT_BYTES_SQL: &str =
    "SELECT COALESCE(SUM(length(CAST(body AS BLOB))),0) FROM chat_checkpoints WHERE session_id=?1";

/// Upserts the `chat_context` row, and adds `new_checkpoints` after the stored ones, or with
/// `replace_checkpoints` instead of them. Every check runs before the first write.
pub(crate) fn write_context(
    tx: &Connection,
    id: &str,
    c: &mut Counters,
    ctx: &ContextRow,
    new_checkpoints: &[Value],
    replace_checkpoints: bool,
) -> Result<(), StorageError> {
    let texts = new_checkpoints
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?;
    // Appending reads one key (the next ordinal, which is also how many are kept) and no body.
    // Only a replace sums the old bodies, to give their bytes back.
    let (kept, mut total) = if replace_checkpoints {
        let stored_bytes: i64 =
            stmt(tx, CHECKPOINT_BYTES_SQL)?.query_row([id], |row| row.get(0))?;
        (0, c.total_bytes.saturating_sub(unsigned(stored_bytes)?))
    } else {
        let next: i64 = stmt(tx, NEXT_ORDINAL_SQL)?.query_row([id], |row| row.get(0))?;
        (unsigned(next)?, c.total_bytes)
    };
    for (number, text) in texts.iter().enumerate() {
        if text.len() > MAX_CHECKPOINT_BYTES {
            return Err(StorageError::Limit(Cap::Checkpoint));
        }
        if kept + number as u64 + 1 > MAX_CHECKPOINTS as u64 {
            return Err(StorageError::Limit(Cap::Checkpoints));
        }
        total += text.len() as u64;
        if total > MAX_SESSION_BYTES {
            return Err(StorageError::Limit(Cap::SessionTotal));
        }
    }
    if replace_checkpoints {
        stmt(tx, "DELETE FROM chat_checkpoints WHERE session_id=?1")?.execute([id])?;
    }
    for (number, text) in texts.iter().enumerate() {
        stmt(
            tx,
            "INSERT INTO chat_checkpoints(session_id,ordinal,body) VALUES (?1,?2,?3)",
        )?
        .execute(params![id, kept as i64 + number as i64, text])?;
    }
    c.total_bytes = total;
    stmt(
        tx,
        "INSERT INTO chat_context(session_id,model_from,model_len,covered_messages,source_hash,server_usage)
         VALUES (?1,?2,?3,?4,?5,?6)
         ON CONFLICT(session_id) DO UPDATE SET model_from=excluded.model_from,model_len=excluded.model_len,
         covered_messages=excluded.covered_messages,source_hash=excluded.source_hash,server_usage=excluded.server_usage",
    )?
    .execute(params![
        id,
        ctx.model_from,
        int(ctx.model_len),
        int(ctx.covered_messages),
        ctx.source_hash,
        ctx.server_usage
    ])?;
    Ok(())
}

// ---- session columns ----

fn check_title(title: &str) -> Result<(), StorageError> {
    if title.is_empty() || title.len() > 1000 {
        return Err(StorageError::Invalid("Invalid conversation title."));
    }
    Ok(())
}

fn check_mode_workspace(mode: Option<&str>, workspace: Option<&str>) -> Result<(), StorageError> {
    if mode.is_some_and(|mode| mode.is_empty() || mode.len() > 64) {
        return Err(StorageError::Invalid("Invalid conversation mode."));
    }
    if workspace
        .is_some_and(|workspace| workspace.len() > 2048 || !Path::new(workspace).is_absolute())
    {
        return Err(StorageError::Invalid("Invalid conversation workspace."));
    }
    Ok(())
}

/// Sets `mode` and `workspace` where they are still NULL. A value already there is never changed.
pub(crate) fn fill_mode_workspace(
    tx: &Connection,
    id: &str,
    mode: Option<&str>,
    workspace: Option<&str>,
) -> Result<(), StorageError> {
    check_mode_workspace(mode, workspace)?;
    let changed = stmt(
        tx,
        "UPDATE sessions SET mode=COALESCE(mode,?2),workspace=COALESCE(workspace,?3) WHERE id=?1",
    )?
    .execute(params![id, mode, workspace])?;
    if changed == 0 {
        return Err(StorageError::NotFound);
    }
    Ok(())
}

// ---- loading ----

/// A session read from its rows, with the counters it stored and the ones its rows add up to.
struct Loaded {
    chat: StoredChat,
    /// The `contextState` exactly as `build_view` made it, when the context is `Valid`. This is
    /// what the session JSON carries, so a `serverUsage` that was absent stays absent.
    context_json: Option<Value>,
    stored: Counters,
    counted: Counters,
    /// Whether the stored counters could be read at all (negative ones cannot).
    stored_ok: bool,
    /// The UI index of the message a run still open has folded, which is not indexed yet.
    open_message: Option<u64>,
}

impl Loaded {
    fn consistent(&self) -> bool {
        self.stored_ok && self.stored == self.counted
    }
}

fn view_error(id: &str, error: &ViewError) -> StorageError {
    // The entry number is for the log only.
    eprintln!("warn: chat {id} could not be rebuilt: {error}");
    StorageError::Invalid(UNREADABLE)
}

fn context_reason(error: &ViewError) -> &'static str {
    match error {
        ViewError::ContextLength => CONTEXT_TOO_SHORT,
        ViewError::BadRef { .. } | ViewError::BadBody { .. } => CONTEXT_REF,
        ViewError::BadContext => CONTEXT_BAD,
    }
}

/// Reads and rebuilds a session. It starts no transaction: the caller owns the snapshot, which is
/// how the upgrade reads inside its own.
fn read_chat(conn: &Connection, id: &str) -> Result<Option<Loaded>, StorageError> {
    type Columns = (
        String,
        i64,
        Option<String>,
        Option<String>,
        String,
        i64,
        [i64; 5],
    );
    let columns: Option<Columns> = stmt(
        conn,
        "SELECT title,updated_at,mode,workspace,metadata,revision,
         next_seq,total_bytes,ui_count,model_count,search_bytes FROM sessions WHERE id=?1",
    )?
    .query_row([id], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            [
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
            ],
        ))
    })
    .optional()?;
    let Some((title, updated_at, mode, workspace, metadata_text, revision, stored)) = columns
    else {
        return Ok(None);
    };
    let metadata: Map<String, Value> = serde_json::from_str(&metadata_text)?;

    let mut tally = Tally::default();
    let mut events = vec![];
    {
        let mut statement = stmt(
            conn,
            "SELECT seq,kind,body FROM chat_events WHERE session_id=?1 ORDER BY seq",
        )?;
        let mut rows = statement.query([id])?;
        while let Some(row) = rows.next()? {
            let seq: i64 = row.get(0)?;
            let kind = parse_kind(&row.get::<_, String>(1)?)?;
            let text: String = row.get(2)?;
            let body: Value = serde_json::from_str(&text)?;
            tally.event(seq, kind, text.len(), marks_run(kind, &body));
            events.push(EventRow { seq, kind, body });
        }
    }
    let mut checkpoint_bytes = 0u64;
    let mut checkpoints = vec![];
    let mut invalid = None;
    {
        let mut statement = stmt(
            conn,
            "SELECT body FROM chat_checkpoints WHERE session_id=?1 ORDER BY ordinal",
        )?;
        let mut rows = statement.query([id])?;
        while let Some(row) = rows.next()? {
            let text: String = row.get(0)?;
            checkpoint_bytes += text.len() as u64;
            match serde_json::from_str(&text) {
                Ok(checkpoint) => checkpoints.push(checkpoint),
                Err(_) => invalid = Some(CONTEXT_BAD),
            }
        }
    }
    let context_row = stmt(
        conn,
        "SELECT model_from,model_len,covered_messages,source_hash,server_usage FROM chat_context WHERE session_id=?1",
    )?
    .query_row([id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })
    .optional()?;
    let mut context = None;
    if let Some((model_from, model_len, covered, source_hash, server_usage)) = context_row {
        match (u64::try_from(model_len), u64::try_from(covered)) {
            (Ok(model_len), Ok(covered_messages)) => {
                context = Some(ContextRow {
                    model_from,
                    model_len,
                    covered_messages,
                    source_hash,
                    server_usage,
                });
            }
            _ => invalid = Some(CONTEXT_BAD),
        }
    }
    if invalid.is_some() {
        context = None;
    }

    let view = match build_view(&events, context.as_ref(), &checkpoints) {
        Ok(view) => view,
        Err(error) if context.is_some() => {
            // The context is what failed, or the rows are bad. A chat with unreadable rows has no
            // messages to show; one with only a bad context still does.
            let view = build_view(&events, None, &[]).map_err(|_| view_error(id, &error))?;
            eprintln!("warn: chat {id} has a saved context that cannot be rebuilt: {error}");
            invalid = Some(context_reason(&error));
            view
        }
        Err(error) => return Err(view_error(id, &error)),
    };
    let mut context_json = None;
    let context = match (invalid, view.context) {
        (Some(reason), _) => StoredContext::Invalid { reason },
        (None, None) => StoredContext::None,
        (None, Some(value)) => match serde_json::from_value::<ContextState>(value.clone()) {
            Ok(state) if state.validate().is_ok() => {
                context_json = Some(value);
                StoredContext::Valid(state)
            }
            _ => StoredContext::Invalid {
                reason: CONTEXT_BAD,
            },
        },
    };

    // A run still open is not indexed until its terminal, so its folded message counts for nothing.
    let open_message = view.open_run.as_ref().map(|open| open.message_index);
    let search_bytes = search::derived_bytes(&view.messages, open_message);
    let counted = tally.finish(&metadata_text, checkpoint_bytes, search_bytes);
    let stored_ok = stored.iter().all(|value| *value >= 0);
    let stored = Counters {
        next_seq: stored[0],
        total_bytes: stored[1].max(0) as u64,
        ui_count: stored[2].max(0) as u64,
        model_count: stored[3].max(0) as u64,
        search_bytes: stored[4].max(0) as u64,
    };
    Ok(Some(Loaded {
        chat: StoredChat {
            id: id.to_owned(),
            title,
            updated_at,
            mode,
            workspace,
            metadata,
            revision,
            messages: view.messages,
            context,
            open_run: view.open_run.is_some(),
        },
        context_json,
        stored,
        counted,
        stored_ok,
        open_message,
    }))
}

/// Today's session JSON for a chat: the typed columns, each `metadata` key at the top level, the
/// messages, and a `contextState` only when the stored context is valid. That value is
/// `build_view`'s own, not a re-serialized `ContextState`.
/// Top-level session JSON keys that metadata may not carry. `contextState` is reserved only for
/// a non-null value: a literal `contextState:null` is how v5 saved "no context", and metadata may
/// keep it (see [`reserved_in_metadata`]).
const RESERVED_KEYS: [&str; 7] = [
    "id",
    "title",
    "updatedAt",
    "mode",
    "workspace",
    "messages",
    "contextState",
];

fn reserved_in_metadata(key: &str, value: &Value) -> bool {
    RESERVED_KEYS.contains(&key) && !(key == "contextState" && value.is_null())
}

fn chat_json(loaded: &Loaded) -> Value {
    let chat = &loaded.chat;
    let mut json = Map::new();
    for (key, value) in &chat.metadata {
        // A reserved key is owned by a column, the log or the context, so metadata never shows it.
        // The one exception is a literal `contextState:null`, shown only while the chat has no
        // context row; a real context always wins.
        let null_context =
            key == "contextState" && value.is_null() && matches!(chat.context, StoredContext::None);
        if null_context || !RESERVED_KEYS.contains(&key.as_str()) {
            json.insert(key.clone(), value.clone());
        }
    }
    json.insert("id".into(), json!(chat.id));
    json.insert("title".into(), json!(chat.title));
    json.insert("updatedAt".into(), json!(chat.updated_at));
    for (key, value) in [("mode", &chat.mode), ("workspace", &chat.workspace)] {
        if let Some(value) = value {
            json.insert(key.into(), json!(value));
        }
    }
    json.insert("messages".into(), Value::Array(chat.messages.clone()));
    if let Some(context) = &loaded.context_json {
        json.insert("contextState".into(), context.clone());
    }
    Value::Object(json)
}

/// The session JSON on any connection, inside the caller's transaction or out of one. It does not
/// repair counters, since the upgrade reads a chat it has just written. `NotFound` when absent.
pub(crate) fn load_json_v6_from(conn: &Connection, id: &str) -> Result<Value, StorageError> {
    let loaded = read_chat(conn, id)?.ok_or(StorageError::NotFound)?;
    Ok(chat_json(&loaded))
}

impl LocalStore {
    /// Adds an empty session at revision 1, inside one write transaction that also enforces the
    /// 500-session limit.
    pub fn create_session(&self, session: &NewSession) -> Result<i64, StorageError> {
        validate_id(&session.id)?;
        check_title(&session.title)?;
        check_mode_workspace(session.mode.as_deref(), session.workspace.as_deref())?;
        if session.updated_at < 0 {
            return Err(StorageError::Invalid("Invalid conversation date."));
        }
        let tx = write_tx(&self.connection)?;
        let sessions: i64 =
            stmt(&tx, "SELECT count(*) FROM sessions")?.query_row([], |r| r.get(0))?;
        if sessions >= MAX_SESSIONS {
            return Err(StorageError::Limit(Cap::Sessions));
        }
        let exists: bool = stmt(&tx, "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)")?
            .query_row([&session.id], |r| r.get(0))?;
        if exists {
            return Err(StorageError::Invalid("This conversation already exists."));
        }
        stmt(
            &tx,
            "INSERT INTO sessions(id,title,updated_at,body,mode,workspace) VALUES (?1,?2,?3,'',?4,?5)",
        )?
        .execute(params![
            session.id,
            session.title,
            session.updated_at,
            session.mode,
            session.workspace
        ])?;
        tx.commit()?;
        Ok(1)
    }

    /// Changes the title and/or replaces the metadata, and bumps the revision: the only write that
    /// does. A stale `revision` is a `Conflict`.
    pub fn update_session_metadata(
        &self,
        id: &str,
        patch: &MetadataPatch,
        revision: i64,
    ) -> Result<i64, StorageError> {
        validate_id(id)?;
        if let Some(title) = &patch.title {
            check_title(title)?;
        }
        if patch.metadata.as_ref().is_some_and(|map| {
            map.iter()
                .any(|(key, value)| reserved_in_metadata(key, value))
        }) {
            return Err(StorageError::Invalid(
                "These conversation details use a name that is reserved, so they were not saved.",
            ));
        }
        let metadata = patch
            .metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let tx = write_tx(&self.connection)?;
        let (current, old, total): (i64, String, i64) = stmt(
            &tx,
            "SELECT revision,metadata,total_bytes FROM sessions WHERE id=?1",
        )?
        .query_row([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .optional()?
        .ok_or(StorageError::NotFound)?;
        if current != revision {
            return Err(StorageError::Conflict { current });
        }
        let mut total = unsigned(total)?;
        if let Some(text) = &metadata {
            if text.len() > MAX_METADATA_BYTES {
                return Err(StorageError::Limit(Cap::Metadata));
            }
            total = total.saturating_sub(metadata_bytes(&old)) + metadata_bytes(text);
            if total > MAX_SESSION_BYTES {
                return Err(StorageError::Limit(Cap::SessionTotal));
            }
        }
        stmt(
            &tx,
            "UPDATE sessions SET title=COALESCE(?2,title),metadata=COALESCE(?3,metadata),total_bytes=?4,
             revision=revision+1 WHERE id=?1",
        )?
        .execute(params![id, patch.title, metadata, int(total)])?;
        tx.commit()?;
        Ok(current + 1)
    }

    /// Rebuilds a chat from its log. A counter that disagrees with the rows is rewritten from them.
    /// A load never changes the log or the model context.
    pub fn load_chat(&self, id: &str) -> Result<Option<StoredChat>, StorageError> {
        Ok(self.load_checked(id)?.map(|loaded| loaded.chat))
    }

    fn load_checked(&self, id: &str) -> Result<Option<Loaded>, StorageError> {
        validate_id(id)?;
        let read = Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let loaded = read_chat(&read, id)?;
        read.commit()?;
        let Some(loaded) = loaded else {
            return Ok(None);
        };
        if !loaded.consistent() {
            let tx = write_tx(&self.connection)?;
            match recount(&tx, id) {
                Ok(counters) => {
                    write_counters(&tx, id, &counters, 0)?;
                    tx.commit()?;
                    note_counter_repair(id);
                }
                // Deleted since the read: nothing left to repair.
                Err(StorageError::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(Some(loaded))
    }

    /// A chat's UI messages and the index of the one a still-open run has folded, for search to
    /// rebuild the text it indexed. It repairs counters as [`Self::load_chat`] does.
    pub(crate) fn load_for_search(&self, id: &str) -> Result<Option<SearchSource>, StorageError> {
        Ok(self
            .load_checked(id)?
            .map(|loaded| (loaded.chat.messages, loaded.open_message)))
    }

    /// The session list as today's JSON, from typed columns only; no `chat_events` row is read.
    pub(crate) fn list_v6(&self) -> Result<Vec<Value>, StorageError> {
        let mut statement = stmt(
            &self.connection,
            "SELECT id, title, updated_at, mode, workspace
             FROM (
                 SELECT id, title, updated_at, mode, workspace
                 FROM sessions
                 ORDER BY updated_at DESC LIMIT 500
             )
             ORDER BY updated_at DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "title": row.get::<_, String>(1)?,
                "updatedAt": row.get::<_, i64>(2)?,
                "messages": [],
                "mode": row.get::<_, Option<String>>(3)?.unwrap_or_else(|| "chat".into()),
                "workspace": row.get::<_, Option<String>>(4)?,
            }))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Today's session JSON, or `None`. It repairs counters as [`Self::load_chat`] does.
    pub(crate) fn load_json_v6(&self, id: &str) -> Result<Option<Value>, StorageError> {
        Ok(self.load_checked(id)?.map(|loaded| chat_json(&loaded)))
    }

    /// Deletes a chat and everything keyed to it. Deleted search terms stay in the index's
    /// segments until it is merged, so an index that held any is optimized in the same transaction.
    pub(crate) fn delete_v6(&self, id: &str) -> Result<(), StorageError> {
        validate_id(id)?;
        let tx = write_tx(&self.connection)?;
        let indexed: bool = stmt(
            &tx,
            "SELECT EXISTS(SELECT 1 FROM chat_search_keys WHERE session_id=?1)",
        )?
        .query_row([id], |row| row.get(0))?;
        stmt(&tx, "DELETE FROM sessions WHERE id=?1")?.execute([id])?;
        if indexed {
            stmt(
                &tx,
                "INSERT INTO chat_search(chat_search) VALUES ('optimize')",
            )?
            .execute([])?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// A v5 in-memory store with the v6 schema applied. Its `user_version` stays 5: `open` is still v5.
#[cfg(test)]
pub(crate) fn v6_in_memory() -> LocalStore {
    let store = LocalStore::initialize(Connection::open_in_memory().unwrap()).unwrap();
    let transaction = store.connection().unchecked_transaction().unwrap();
    apply_v6_schema(&transaction).unwrap();
    transaction.commit().unwrap();
    store
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::protocol::{
        MAX_ATTACHMENTS_PER_MESSAGE, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENT_TOTAL_BYTES,
    };
    use crate::storage::fixtures::abs;
    use crate::storage::{configure, FOREIGN_KEYS_OFF};
    use crate::storage::{
        MAX_CHECKPOINTS, MAX_CHECKPOINT_BYTES, MAX_EVENT_BYTES, MAX_METADATA_BYTES,
        MAX_MODEL_ENTRIES, MAX_SESSION_BYTES, MAX_UI_MESSAGES,
    };
    use rusqlite::params;
    use std::time::Duration;

    const ALL: [Cap; 11] = [
        Cap::Attachment,
        Cap::AttachmentsPerMessage,
        Cap::AttachmentTotal,
        Cap::Event,
        Cap::Checkpoint,
        Cap::Checkpoints,
        Cap::UiMessages,
        Cap::ModelEntries,
        Cap::SessionTotal,
        Cap::Metadata,
        Cap::Sessions,
    ];

    fn count(db: &LocalStore, sql: &str) -> i64 {
        db.connection().query_row(sql, [], |r| r.get(0)).unwrap()
    }
    fn names(db: &LocalStore, sql: &str) -> Vec<String> {
        let mut statement = db.connection().prepare(sql).unwrap();
        let rows = statement.query_map([], |r| r.get(0)).unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    }

    #[test]
    fn v6_schema_has_the_new_tables_and_keeps_an_empty_body_column() {
        let db = v6_in_memory();
        let tables = names(
            &db,
            "SELECT name FROM sqlite_master WHERE type IN ('table','trigger') ORDER BY name",
        );
        for wanted in [
            "chat_events",
            "chat_context",
            "chat_checkpoints",
            "chat_search_keys",
            "chat_search",
            "chat_search_keys_ad",
            "quarantined_sessions",
            "sessions",
        ] {
            assert!(tables.iter().any(|t| t == wanted), "missing {wanted}");
        }
        for gone in [
            "message_insert",
            "message_delete",
            "message_search",
            "session_messages",
            "indexed_sessions",
        ] {
            assert!(
                !tables.iter().any(|t| t == gone),
                "{gone} should be dropped"
            );
        }
        let columns = names(&db, "SELECT name FROM pragma_table_info('sessions')");
        for wanted in [
            "body",
            "mode",
            "workspace",
            "metadata",
            "revision",
            "next_seq",
            "total_bytes",
            "ui_count",
            "model_count",
            "search_bytes",
        ] {
            assert!(columns.iter().any(|c| c == wanted), "missing {wanted}");
        }
        let context = names(&db, "SELECT name FROM pragma_table_info('chat_context')");
        assert!(context.iter().any(|c| c == "model_from"));
        assert!(context.iter().any(|c| c == "model_len"));
        // A new row writes an empty v5 body and takes the counter defaults.
        db.connection()
            .execute(
                "INSERT INTO sessions(id,title,updated_at,body) VALUES ('a','t',1,'')",
                [],
            )
            .unwrap();
        let row: (String, String, i64, i64, i64, i64, i64) = db
            .connection()
            .query_row(
                "SELECT body,metadata,revision,next_seq,total_bytes,ui_count,model_count FROM sessions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )
            .unwrap();
        assert_eq!(row, (String::new(), "{}".into(), 1, 0, 0, 0, 0));
        // chat_events is a rowid-less add-only log keyed by (session, seq).
        assert!(db
            .connection()
            .execute(
                "INSERT INTO chat_events(session_id,seq,kind,body) VALUES ('a',0,'bogus','{}')",
                [],
            )
            .is_err());
    }

    #[test]
    fn bundled_sqlite_supports_contentless_delete_and_secure_delete() {
        let db = v6_in_memory();
        let version: String = db
            .connection()
            .query_row("SELECT sqlite_version()", [], |r| r.get(0))
            .unwrap();
        let parts: Vec<u32> = version.split('.').map(|p| p.parse().unwrap()).collect();
        assert!((parts[0], parts[1]) >= (3, 43), "sqlite {version}");
        let shadow = |db: &LocalStore| -> Vec<u8> {
            let mut bytes = Vec::new();
            for table in names(
                db,
                "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'chat_search\\_%' ESCAPE '\\' AND name NOT LIKE 'chat_search\\_keys' ESCAPE '\\'",
            ) {
                let mut statement = db
                    .connection()
                    .prepare(&format!("SELECT * FROM {table}"))
                    .unwrap();
                let columns = statement.column_count();
                let mut rows = statement.query([]).unwrap();
                while let Some(row) = rows.next().unwrap() {
                    for index in 0..columns {
                        match row.get_ref(index).unwrap() {
                            rusqlite::types::ValueRef::Blob(b) => bytes.extend_from_slice(b),
                            rusqlite::types::ValueRef::Text(t) => bytes.extend_from_slice(t),
                            _ => {}
                        }
                    }
                }
            }
            bytes
        };
        let contains =
            |haystack: &[u8], needle: &[u8]| haystack.windows(needle.len()).any(|w| w == needle);
        let marker = b"zqxjmarkerword";
        db.connection()
            .execute(
                "INSERT INTO chat_search(rowid,content) VALUES (7, 'alpha zqxjmarkerword omega')",
                [],
            )
            .unwrap();
        assert!(
            contains(&shadow(&db), marker),
            "the marker should be indexed first"
        );
        db.connection()
            .execute("DELETE FROM chat_search WHERE rowid = 7", [])
            .unwrap();
        // FINDING: with `contentless_delete=1`, a plain DELETE only tombstones the row. The term
        // bytes stay in `chat_search_data` until a merge rewrites the segments, and `secure-delete`
        // does not scrub them. Anything that must erase terms (deleting a chat) has to follow the
        // delete with an `optimize`, which this test pins.
        db.connection()
            .execute(
                "INSERT INTO chat_search(chat_search) VALUES ('optimize')",
                [],
            )
            .unwrap();
        assert!(
            !contains(&shadow(&db), marker),
            "the marker survived the delete and an optimize"
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM chat_search WHERE chat_search MATCH 'zqxjmarkerword'"
            ),
            0
        );
    }

    #[test]
    fn configure_refuses_a_connection_without_foreign_keys() {
        let connection = Connection::open_in_memory().unwrap();
        configure(&connection, Duration::from_millis(1)).unwrap();
        let on: i64 = connection
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .unwrap();
        assert_eq!(on, 1);
        let inside = Connection::open_in_memory().unwrap();
        // The bundled build defaults foreign_keys to ON, so turn it off first; inside the
        // transaction `configure`'s `foreign_keys=ON` is then a silent no-op and only the readback sees it.
        inside
            .execute_batch("PRAGMA foreign_keys=OFF; BEGIN;")
            .unwrap();
        // The readback must be what refuses it, not a later pragma that errors inside a transaction.
        assert!(matches!(
            configure(&inside, Duration::from_millis(1)),
            Err(StorageError::Invalid(FOREIGN_KEYS_OFF))
        ));
    }

    #[test]
    fn every_cap_has_its_own_message() {
        let messages: Vec<&str> = ALL.iter().map(|c| c.message()).collect();
        for (cap, message) in ALL.iter().zip(&messages) {
            assert!(!message.is_empty(), "{cap:?}");
            assert_eq!(
                messages.iter().filter(|m| *m == message).count(),
                1,
                "{cap:?} shares its text"
            );
            assert_eq!(StorageError::Limit(*cap).to_string(), *message);
        }
        let mib = |bytes: u64| format!("{} MiB", bytes >> 20);
        let has = |cap: Cap, text: String| {
            assert!(cap.message().contains(&text), "{cap:?} lacks {text:?}")
        };
        has(Cap::Event, mib(MAX_EVENT_BYTES as u64));
        has(Cap::Attachment, mib(MAX_ATTACHMENT_BYTES));
        has(Cap::AttachmentTotal, mib(MAX_ATTACHMENT_TOTAL_BYTES));
        has(Cap::Checkpoint, mib(MAX_CHECKPOINT_BYTES as u64));
        assert_eq!(MAX_SESSION_BYTES, 48 << 20); // SessionTotal keeps the v5 text, which names no number
        has(Cap::Metadata, format!("{} KiB", MAX_METADATA_BYTES >> 10));
        has(Cap::Checkpoints, format!("{MAX_CHECKPOINTS}-checkpoint"));
        has(
            Cap::AttachmentsPerMessage,
            format!("{MAX_ATTACHMENTS_PER_MESSAGE} attachments"),
        );
        has(
            Cap::UiMessages,
            format!(
                "{},{:03}-message",
                MAX_UI_MESSAGES / 1000,
                MAX_UI_MESSAGES % 1000
            ),
        );
        has(
            Cap::ModelEntries,
            format!(
                "{},{:03}-step",
                MAX_MODEL_ENTRIES / 1000,
                MAX_MODEL_ENTRIES % 1000
            ),
        );
        has(Cap::Sessions, super::super::MAX_SESSIONS.to_string());
    }

    #[test]
    fn a_cascade_delete_clears_events_context_checkpoints_and_search() {
        let db = v6_in_memory();
        let c = db.connection();
        c.execute(
            "INSERT INTO sessions(id,title,updated_at,body) VALUES ('s','t',1,'')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO chat_events(session_id,seq,kind,body) VALUES ('s',0,'ui_user','{}')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO chat_context(session_id,model_from,model_len,covered_messages,source_hash) VALUES ('s',0,1,1,'h')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO chat_checkpoints(session_id,ordinal,body) VALUES ('s',0,'{}')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO chat_search_keys(id,session_id,seq,message_index,message_id,role) VALUES (5,'s',0,0,'m','user')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO chat_search(rowid,content) VALUES (?1, 'hello needle')",
            params![5],
        )
        .unwrap();
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM chat_search WHERE chat_search MATCH 'needle'"
            ),
            1
        );
        c.execute("DELETE FROM sessions WHERE id='s'", []).unwrap();
        for table in [
            "chat_events",
            "chat_context",
            "chat_checkpoints",
            "chat_search_keys",
        ] {
            assert_eq!(
                count(&db, &format!("SELECT count(*) FROM {table}")),
                0,
                "{table}"
            );
        }
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM chat_search WHERE chat_search MATCH 'needle'"
            ),
            0
        );
    }

    // ---- the session API, counters and the append primitive (A5) ----

    use crate::context::{fingerprint, ContextState};
    use crate::storage::is_busy;
    use std::{fs, thread};

    const MIB: usize = 1 << 20;

    fn new_session(id: &str) -> NewSession {
        NewSession {
            id: id.into(),
            title: format!("Chat {id}"),
            mode: None,
            workspace: None,
            updated_at: 1_000,
        }
    }
    fn make(db: &LocalStore, id: &str) {
        assert_eq!(db.create_session(&new_session(id)).unwrap(), 1);
    }
    fn user_message(n: usize) -> Value {
        json!({"id": format!("u{n}"), "role": "user", "content": format!("hello {n}"),
               "attachments": [], "status": "complete", "createdAt": 5})
    }
    /// One write transaction over the session's counters, as a real writer runs it.
    fn write<R>(
        db: &LocalStore,
        id: &str,
        f: impl FnOnce(&Connection, &mut Counters) -> Result<R, StorageError>,
    ) -> Result<R, StorageError> {
        let tx = write_tx(db.connection())?;
        let mut counters = read_counters(&tx, id)?;
        let result = f(&tx, &mut counters)?;
        write_counters(&tx, id, &counters, 0)?;
        tx.commit()?;
        Ok(result)
    }
    fn rows(db: &LocalStore, id: &str) -> Vec<(i64, String, String)> {
        let mut statement = db
            .connection()
            .prepare("SELECT seq,kind,body FROM chat_events WHERE session_id=?1 ORDER BY seq")
            .unwrap();
        let rows = statement
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    }
    fn counters(db: &LocalStore, id: &str) -> Counters {
        read_counters(db.connection(), id).unwrap()
    }
    /// A complete run: user, start, one assistant turn, a progress delta and a terminal.
    fn run(db: &LocalStore, id: &str, n: usize) {
        write(db, id, |tx, c| {
            let at = c.ui_count;
            let user = append_ui_user(tx, id, c, &user_message(n))?;
            search::index_message(tx, id, c, user, at, &user_message(n))?;
            let start = append_run_start(tx, id, c, &format!("a{n}"), 7, user)?;
            let turn = append_model(tx, id, c, &json!({"role":"assistant","content":"hi"}))?;
            append(
                tx,
                id,
                c,
                Kind::UiProgress,
                &json!({"run": start, "reasoning": "thinking"}),
            )?;
            let end = append_ui_assistant(
                tx,
                id,
                c,
                &json!({"run": start, "content": {"turns": [turn]}, "status": "complete"}),
            )?;
            let reply = json!({"id": format!("a{n}"), "role": "assistant", "content": "hi"});
            search::index_message(tx, id, c, end, at + 1, &reply)?;
            Ok(())
        })
        .unwrap();
    }
    fn shadow_bytes(db: &LocalStore) -> Vec<u8> {
        let mut bytes = Vec::new();
        for table in names(
            db,
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'chat_search\\_%' ESCAPE '\\' AND name NOT LIKE 'chat_search\\_keys' ESCAPE '\\'",
        ) {
            let mut statement = db
                .connection()
                .prepare(&format!("SELECT * FROM {table}"))
                .unwrap();
            let columns = statement.column_count();
            let mut rows = statement.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                for index in 0..columns {
                    match row.get_ref(index).unwrap() {
                        rusqlite::types::ValueRef::Blob(b) => bytes.extend_from_slice(b),
                        rusqlite::types::ValueRef::Text(t) => bytes.extend_from_slice(t),
                        _ => {}
                    }
                }
            }
        }
        bytes
    }

    #[test]
    fn append_allocates_contiguous_seqs_and_keeps_counters_exact() {
        let db = v6_in_memory();
        make(&db, "s");
        for n in 0..10 {
            run(&db, "s", n);
        }
        let stored = counters(&db, "s");
        assert_eq!(stored.next_seq, 50);
        assert_eq!(stored.ui_count, 20, "the terminal fills its reserved slot");
        assert_eq!(stored.model_count, 10);
        let seqs: Vec<i64> = rows(&db, "s").iter().map(|r| r.0).collect();
        assert_eq!(seqs, (0..50).collect::<Vec<_>>());
        // Checkpoints and metadata are in the total too.
        let state = ContextRow {
            model_from: 0,
            model_len: 1,
            covered_messages: 1,
            source_hash: "h".into(),
            server_usage: None,
        };
        let checkpoint = json!({"through": 1, "sourceHash": "x", "summary": {}});
        write(&db, "s", |tx, c| {
            write_context(
                tx,
                "s",
                c,
                &state,
                &[checkpoint.clone(), checkpoint.clone()],
                false,
            )
        })
        .unwrap();
        let patch = MetadataPatch {
            title: None,
            metadata: Some(serde_json::from_value(json!({"k": "value"})).unwrap()),
        };
        db.update_session_metadata("s", &patch, 1).unwrap();
        let stored = counters(&db, "s");
        let tx = write_tx(db.connection()).unwrap();
        assert_eq!(recount(&tx, "s").unwrap(), stored);
        let expected_bytes: u64 = rows(&db, "s").iter().map(|r| r.2.len() as u64).sum::<u64>()
            + 2 * serde_json::to_string(&checkpoint).unwrap().len() as u64
            + r#"{"k":"value"}"#.len() as u64;
        assert_eq!(stored.total_bytes, expected_bytes);
        drop(tx);
        // Loading a consistent chat repairs nothing.
        let before = counter_repairs();
        let chat = db.load_chat("s").unwrap().unwrap();
        assert_eq!(chat.messages.len(), 20);
        assert_eq!(counter_repairs(), before);
    }

    /// Runs `op` in a fresh write transaction, expects `Limit(cap)`, and checks that the counters,
    /// the rows and the checkpoints are exactly what they were. The transaction is then dropped.
    fn refused(
        db: &LocalStore,
        id: &str,
        cap: Cap,
        op: impl FnOnce(&Connection, &mut Counters) -> Result<(), StorageError>,
    ) {
        let tx = write_tx(db.connection()).unwrap();
        let mut c = read_counters(&tx, id).unwrap();
        let before = c.clone();
        let tally = |tx: &Connection| -> (i64, i64) {
            let events = tx
                .query_row("SELECT count(*) FROM chat_events", [], |r| r.get(0))
                .unwrap();
            let checkpoints = tx
                .query_row("SELECT count(*) FROM chat_checkpoints", [], |r| r.get(0))
                .unwrap();
            (events, checkpoints)
        };
        let rows_before = tally(&tx);
        match op(&tx, &mut c) {
            Err(StorageError::Limit(got)) => assert_eq!(got, cap),
            other => panic!("expected Limit({cap:?}), got {other:?}"),
        }
        assert_eq!(c, before, "{cap:?} changed the counters");
        assert_eq!(tally(&tx), rows_before, "{cap:?} wrote rows");
    }

    #[test]
    fn each_cap_is_refused_with_its_name_and_nothing_is_written() {
        let db = v6_in_memory();
        make(&db, "s");
        run(&db, "s", 0);
        let message = |attachments: Value| {
            let mut m = user_message(1);
            m["attachments"] = attachments;
            m
        };
        let sized =
            |sizes: &[usize]| Value::Array(sizes.iter().map(|s| json!({"sizeBytes": s})).collect());
        // Attachment, AttachmentsPerMessage, AttachmentTotal, and Event on the message rules.
        assert!(matches!(
            validate_ui_message(&message(sized(&[15 * MIB + 1])), "user"),
            Err(StorageError::Limit(Cap::Attachment))
        ));
        assert!(matches!(
            validate_ui_message(&message(sized(&[1; 9])), "user"),
            Err(StorageError::Limit(Cap::AttachmentsPerMessage))
        ));
        assert!(matches!(
            validate_ui_message(&message(sized(&[15 * MIB, 15 * MIB, MIB])), "user"),
            Err(StorageError::Limit(Cap::AttachmentTotal))
        ));
        let mut huge = user_message(1);
        huge["content"] = json!("x".repeat(33 * MIB));
        assert!(matches!(
            validate_ui_message(&huge, "user"),
            Err(StorageError::Limit(Cap::Event))
        ));
        validate_ui_message(&message(sized(&[15 * MIB, 15 * MIB])), "user").unwrap();
        // Event on the append itself, and the same limit's boundary.
        refused(&db, "s", Cap::Event, |tx, c| {
            append(tx, "s", c, Kind::UiUser, &huge).map(drop)
        });
        // SessionTotal: an append, a checkpoint and a metadata write all count toward 48 MiB.
        let real_total = counters(&db, "s").total_bytes;
        db.connection()
            .execute(
                "UPDATE sessions SET total_bytes=?1 WHERE id='s'",
                [MAX_SESSION_BYTES as i64 - 10],
            )
            .unwrap();
        refused(&db, "s", Cap::SessionTotal, |tx, c| {
            append(tx, "s", c, Kind::UiUser, &user_message(2)).map(drop)
        });
        let state = ContextRow {
            model_from: 0,
            model_len: 0,
            covered_messages: 0,
            source_hash: "h".into(),
            server_usage: None,
        };
        refused(&db, "s", Cap::SessionTotal, |tx, c| {
            write_context(tx, "s", c, &state, &[json!({"p": "x".repeat(64)})], false)
        });
        let stale = db.update_session_metadata(
            "s",
            &MetadataPatch {
                title: None,
                metadata: Some(serde_json::from_value(json!({"k": "x".repeat(64)})).unwrap()),
            },
            1,
        );
        assert!(matches!(stale, Err(StorageError::Limit(Cap::SessionTotal))));
        assert_eq!(counters(&db, "s").total_bytes, MAX_SESSION_BYTES - 10);
        db.connection()
            .execute(
                "UPDATE sessions SET total_bytes=?1 WHERE id='s'",
                [real_total as i64],
            )
            .unwrap();
        // UiMessages: a user message needs one slot and a run start the other.
        db.connection()
            .execute(
                "UPDATE sessions SET ui_count=?1 WHERE id='s'",
                [MAX_UI_MESSAGES as i64],
            )
            .unwrap();
        refused(&db, "s", Cap::UiMessages, |tx, c| {
            append_ui_user(tx, "s", c, &user_message(3)).map(drop)
        });
        refused(&db, "s", Cap::UiMessages, |tx, c| {
            append_ui_assistant(
                tx,
                "s",
                c,
                &json!({"id": "a", "role": "assistant", "content": ""}),
            )
            .map(drop)
        });
        refused(&db, "s", Cap::UiMessages, |tx, c| {
            append_run_start(tx, "s", c, "a9", 1, 0).map(drop)
        });
        // A terminal for a run that reserved its slot is not refused at the cap.
        write(&db, "s", |tx, c| {
            append_ui_assistant(
                tx,
                "s",
                c,
                &json!({"run": 1, "content": "x", "status": "complete"}),
            )
            .map(drop)
        })
        .unwrap();
        db.connection()
            .execute("UPDATE sessions SET ui_count=0 WHERE id='s'", [])
            .unwrap();
        // ModelEntries.
        db.connection()
            .execute(
                "UPDATE sessions SET model_count=?1 WHERE id='s'",
                [MAX_MODEL_ENTRIES as i64],
            )
            .unwrap();
        refused(&db, "s", Cap::ModelEntries, |tx, c| {
            append_model(tx, "s", c, &json!({"role": "user", "content": "x"})).map(drop)
        });
        db.connection()
            .execute("UPDATE sessions SET model_count=0 WHERE id='s'", [])
            .unwrap();
        // Checkpoint (size) and Checkpoints (count).
        refused(&db, "s", Cap::Checkpoint, |tx, c| {
            let big = json!({"summary": "x".repeat(MIB)});
            write_context(tx, "s", c, &state, &[big], false)
        });
        let small = json!({"n": 1});
        write(&db, "s", |tx, c| {
            write_context(
                tx,
                "s",
                c,
                &state,
                &vec![small.clone(); MAX_CHECKPOINTS],
                false,
            )
        })
        .unwrap();
        refused(&db, "s", Cap::Checkpoints, |tx, c| {
            write_context(tx, "s", c, &state, std::slice::from_ref(&small), false)
        });
        // Replacing the checkpoints is not a way past the count, and it gives the bytes back.
        let total = counters(&db, "s").total_bytes;
        write(&db, "s", |tx, c| {
            write_context(tx, "s", c, &state, std::slice::from_ref(&small), true)
        })
        .unwrap();
        let after = counters(&db, "s");
        assert_eq!(
            after.total_bytes,
            total - (MAX_CHECKPOINTS as u64 - 1) * small.to_string().len() as u64
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM chat_checkpoints WHERE session_id='s'"
            ),
            1
        );
        // Metadata (64 KiB).
        let big = MetadataPatch {
            title: None,
            metadata: Some(
                serde_json::from_value(json!({"k": "x".repeat(MAX_METADATA_BYTES)})).unwrap(),
            ),
        };
        assert!(matches!(
            db.update_session_metadata("s", &big, 1),
            Err(StorageError::Limit(Cap::Metadata))
        ));
        let at_limit = MetadataPatch {
            title: None,
            metadata: Some(
                serde_json::from_value(json!({"k": "x".repeat(MAX_METADATA_BYTES - 8)})).unwrap(),
            ),
        };
        assert_eq!(db.update_session_metadata("s", &at_limit, 1).unwrap(), 2);
        // Sessions (500), checked inside the create transaction.
        let created = count(&db, "SELECT count(*) FROM sessions");
        for n in created..500 {
            make(&db, &format!("extra{n}"));
        }
        assert!(matches!(
            db.create_session(&new_session("one-too-many")),
            Err(StorageError::Limit(Cap::Sessions))
        ));
        assert_eq!(count(&db, "SELECT count(*) FROM sessions"), 500);
    }

    #[test]
    fn a_tampered_counter_is_repaired_on_load_and_counted() {
        let db = v6_in_memory();
        make(&db, "s");
        run(&db, "s", 0);
        run(&db, "s", 1);
        let good = counters(&db, "s");
        db.connection()
            .execute(
                "UPDATE sessions SET total_bytes=total_bytes+5, ui_count=0, next_seq=1 WHERE id='s'",
                [],
            )
            .unwrap();
        let events = rows(&db, "s");
        let before = counter_repairs();
        let chat = db.load_chat("s").unwrap().unwrap();
        assert_eq!(chat.messages.len(), 4);
        assert_eq!(counter_repairs(), before + 1);
        assert_eq!(counters(&db, "s"), good);
        assert_eq!(rows(&db, "s"), events, "a repair never touches the log");
        db.load_chat("s").unwrap().unwrap();
        assert_eq!(counter_repairs(), before + 1, "the second load is clean");
        // A negative counter is repaired the same way.
        db.connection()
            .execute("UPDATE sessions SET model_count=-3 WHERE id='s'", [])
            .unwrap();
        db.load_json_v6("s").unwrap().unwrap();
        assert_eq!(counters(&db, "s"), good);
    }

    #[test]
    fn metadata_revision_conflict_and_not_found() {
        let db = v6_in_memory();
        make(&db, "s");
        let patch = |title: &str| MetadataPatch {
            title: Some(title.into()),
            metadata: None,
        };
        assert_eq!(
            db.update_session_metadata("s", &patch("Renamed"), 1)
                .unwrap(),
            2
        );
        assert!(matches!(
            db.update_session_metadata("s", &patch("Stale"), 1),
            Err(StorageError::Conflict { current: 2 })
        ));
        assert!(matches!(
            db.update_session_metadata("missing", &patch("x"), 1),
            Err(StorageError::NotFound)
        ));
        assert!(matches!(
            db.update_session_metadata("s", &patch(""), 2),
            Err(StorageError::Invalid(_))
        ));
        let chat = db.load_chat("s").unwrap().unwrap();
        assert_eq!((chat.title.as_str(), chat.revision), ("Renamed", 2));
        // Only a metadata write bumps the revision.
        run(&db, "s", 0);
        assert_eq!(db.load_chat("s").unwrap().unwrap().revision, 2);
        let metadata: Map<String, Value> = serde_json::from_value(json!({"pinned": true})).unwrap();
        let patch = MetadataPatch {
            title: None,
            metadata: Some(metadata.clone()),
        };
        assert_eq!(db.update_session_metadata("s", &patch, 2).unwrap(), 3);
        let chat = db.load_chat("s").unwrap().unwrap();
        assert_eq!((chat.metadata, chat.title.as_str()), (metadata, "Renamed"));
        assert!(matches!(db.load_chat("missing"), Ok(None)));
        assert!(matches!(
            read_counters(db.connection(), "missing"),
            Err(StorageError::NotFound)
        ));
    }

    #[test]
    fn mode_and_workspace_are_set_once() {
        let db = v6_in_memory();
        make(&db, "s");
        let fill = |mode: Option<&str>, workspace: Option<&str>| {
            let tx = write_tx(db.connection()).unwrap();
            fill_mode_workspace(&tx, "s", mode, workspace)?;
            tx.commit()?;
            Ok::<_, StorageError>(())
        };
        let read = || {
            let chat = db.load_chat("s").unwrap().unwrap();
            (chat.mode, chat.workspace)
        };
        assert_eq!(read(), (None, None));
        fill(Some("agent"), None).unwrap();
        assert_eq!(read(), (Some("agent".into()), None));
        fill(Some("chat"), Some(&abs("/work/a"))).unwrap();
        assert_eq!(read(), (Some("agent".into()), Some(abs("/work/a"))));
        fill(Some("chat"), Some(&abs("/work/b"))).unwrap();
        assert_eq!(read(), (Some("agent".into()), Some(abs("/work/a"))));
        assert!(matches!(
            fill(None, Some("relative/path")),
            Err(StorageError::Invalid(_))
        ));
        assert!(matches!(
            fill(None, Some(&abs(&format!("/{}", "w".repeat(2048))))),
            Err(StorageError::Invalid(_))
        ));
        // create_session takes both once, and checks them.
        let mut full = new_session("full");
        full.mode = Some("agent".into());
        full.workspace = Some(abs("/work"));
        db.create_session(&full).unwrap();
        let chat = db.load_chat("full").unwrap().unwrap();
        assert_eq!(
            (chat.mode, chat.workspace),
            (Some("agent".into()), Some(abs("/work")))
        );
        for bad in [
            NewSession {
                title: String::new(),
                ..new_session("a")
            },
            NewSession {
                title: "t".repeat(1001),
                ..new_session("a")
            },
            NewSession {
                workspace: Some("not/absolute".into()),
                ..new_session("a")
            },
            NewSession {
                id: "bad id!".into(),
                ..new_session("a")
            },
            NewSession {
                updated_at: -1,
                ..new_session("a")
            },
        ] {
            assert!(
                matches!(db.create_session(&bad), Err(StorageError::Invalid(_))),
                "{bad:?}"
            );
        }
        assert!(matches!(
            db.create_session(&new_session("s")),
            Err(StorageError::Invalid(_))
        ));
        // An empty row keeps body='' for v5 compatibility.
        assert_eq!(
            count(&db, "SELECT count(*) FROM sessions WHERE body<>''"),
            0
        );
        assert_eq!(
            count(&db, "SELECT count(*) FROM sessions WHERE metadata<>'{}'"),
            0
        );
    }

    #[test]
    fn list_reads_typed_columns_only() {
        let db = v6_in_memory();
        let mut agent = new_session("b");
        agent.mode = Some("agent".into());
        agent.workspace = Some(abs("/w"));
        agent.updated_at = 2_000;
        db.create_session(&agent).unwrap();
        make(&db, "a");
        run(&db, "a", 0);
        run(&db, "b", 0);
        let before = db.list_v6().unwrap();
        assert_eq!(
            before,
            vec![
                json!({"id":"b","title":"Chat b","updatedAt":2000,"messages":[],"mode":"agent","workspace":abs("/w")}),
                json!({"id":"a","title":"Chat a","updatedAt":1000,"messages":[],"mode":"chat","workspace":null}),
            ]
        );
        db.connection()
            .execute("UPDATE chat_events SET body='not json'", [])
            .unwrap();
        assert_eq!(db.list_v6().unwrap(), before);
        // A corrupt log is reported in plain words, with no entry number.
        match db.load_chat("a") {
            Err(StorageError::Invalid(text)) => {
                assert!(!text.chars().any(|c| c.is_ascii_digit()), "{text}")
            }
            Err(StorageError::Json(_)) => {}
            other => panic!("expected a plain error, got {other:?}"),
        }
    }

    #[test]
    fn an_invalid_stored_context_loads_as_invalid_without_touching_rows() {
        let db = v6_in_memory();
        make(&db, "s");
        let user = json!({"role": "user", "content": "hello 0"});
        write(&db, "s", |tx, c| {
            append_ui_user(tx, "s", c, &user_message(0))?;
            append_model(tx, "s", c, &user)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            db.load_chat("s").unwrap().unwrap().context,
            StoredContext::None
        );
        let set = |row: ContextRow| {
            write(&db, "s", |tx, c| {
                write_context(tx, "s", c, &row, &[], false)
            })
            .unwrap()
        };
        let good = ContextRow {
            model_from: 0,
            model_len: 1,
            covered_messages: 1,
            source_hash: fingerprint(std::slice::from_ref(&user)),
            server_usage: None,
        };
        set(good.clone());
        let chat = db.load_chat("s").unwrap().unwrap();
        let state = ContextState {
            transcript: vec![user.clone()],
            checkpoints: vec![],
            covered_messages: 1,
            source_hash: good.source_hash.clone(),
            server_usage: None,
        };
        assert_eq!(chat.context, StoredContext::Valid(state.clone()));
        assert!(!chat.open_run);
        let json = db.load_json_v6("s").unwrap().unwrap();
        assert_eq!(
            json["contextState"],
            json!({"transcript": [user], "checkpoints": [], "coveredMessages": 1,
                   "sourceHash": good.source_hash}),
            "no serverUsage key when none is stored"
        );
        // Each way a context can be wrong loads as Invalid, with the messages intact.
        let events = rows(&db, "s");
        let counted = counters(&db, "s");
        for bad in [
            ContextRow {
                source_hash: "short".into(),
                ..good.clone()
            },
            ContextRow {
                model_len: 5,
                ..good.clone()
            },
            ContextRow {
                server_usage: Some("{not json".into()),
                ..good.clone()
            },
            ContextRow {
                model_from: 9,
                ..good.clone()
            },
        ] {
            set(bad);
            let chat = db.load_chat("s").unwrap().unwrap();
            assert!(
                matches!(chat.context, StoredContext::Invalid { .. }),
                "{:?}",
                chat.context
            );
            assert_eq!(chat.messages.len(), 1);
            let json = db.load_json_v6("s").unwrap().unwrap();
            assert!(json.get("contextState").is_none());
        }
        db.connection()
            .execute("UPDATE chat_context SET model_len=-1", [])
            .unwrap();
        assert!(matches!(
            db.load_chat("s").unwrap().unwrap().context,
            StoredContext::Invalid { .. }
        ));
        assert_eq!(rows(&db, "s"), events);
        assert_eq!(counters(&db, "s"), counted);
        // Metadata keys show at the top level, but never a contextState: only a valid context gives one.
        let patch = MetadataPatch {
            title: None,
            metadata: Some(serde_json::from_value(json!({"pinned": 1})).unwrap()),
        };
        db.update_session_metadata("s", &patch, 1).unwrap();
        let json = db.load_json_v6("s").unwrap().unwrap();
        assert!(json.get("contextState").is_none());
        assert_eq!(json["pinned"], 1);
        set(good.clone());
        let json = db.load_json_v6("s").unwrap().unwrap();
        assert_eq!(
            json["contextState"],
            json!({"transcript": [user], "checkpoints": [], "coveredMessages": 1,
                   "sourceHash": good.source_hash}),
            "no serverUsage key when none is stored"
        );
    }

    #[test]
    fn load_json_v6_is_todays_session_json() {
        let db = v6_in_memory();
        let mut s = new_session("s");
        s.mode = Some("agent".into());
        s.workspace = Some(abs("/w"));
        db.create_session(&s).unwrap();
        run(&db, "s", 0);
        let patch = MetadataPatch {
            title: None,
            metadata: Some(serde_json::from_value(json!({"extra": [1]})).unwrap()),
        };
        db.update_session_metadata("s", &patch, 1).unwrap();
        let json = db.load_json_v6("s").unwrap().unwrap();
        assert_eq!(json["id"], "s");
        assert_eq!(json["title"], "Chat s");
        assert_eq!(json["updatedAt"], 1000);
        assert_eq!(json["mode"], "agent");
        assert_eq!(json["workspace"], abs("/w"));
        assert_eq!(json["extra"], json!([1]));
        assert_eq!(json["messages"].as_array().unwrap().len(), 2);
        assert_eq!(json["messages"][1]["content"], "hi");
        assert!(json.get("contextState").is_none());
        // The shared body works on any connection, in or out of a transaction, and says NotFound.
        let tx = write_tx(db.connection()).unwrap();
        assert_eq!(load_json_v6_from(&tx, "s").unwrap(), json);
        assert!(matches!(
            load_json_v6_from(&tx, "nope"),
            Err(StorageError::NotFound)
        ));
        drop(tx);
        assert!(db.load_json_v6("nope").unwrap().is_none());
        // A session with no mode or workspace emits neither key.
        make(&db, "bare");
        let bare = db.load_json_v6("bare").unwrap().unwrap();
        assert!(bare.get("mode").is_none() && bare.get("workspace").is_none());
    }

    #[test]
    fn metadata_cannot_carry_a_reserved_key() {
        let db = v6_in_memory();
        make(&db, "bare"); // mode and workspace are NULL
        for key in RESERVED_KEYS {
            let patch = MetadataPatch {
                title: None,
                metadata: Some(serde_json::from_value(json!({ key: "x", "ok": 1 })).unwrap()),
            };
            match db.update_session_metadata("bare", &patch, 1) {
                Err(StorageError::Invalid(text)) => assert!(text.contains("reserved"), "{text}"),
                other => panic!("{key}: expected Invalid, got {other:?}"),
            }
        }
        let chat = db.load_chat("bare").unwrap().unwrap();
        assert_eq!(chat.revision, 1, "a refused patch changes nothing");
        assert!(chat.metadata.is_empty());
        // If a reserved key is in the column anyway, the session JSON still ignores it, so it
        // cannot fill a NULL mode or workspace behind set-once and the path check.
        db.connection()
            .execute(
                r#"UPDATE sessions SET metadata='{"mode":"agent","workspace":"/evil","id":"x","title":"x","updatedAt":9,"messages":[1],"contextState":null,"ok":1}' WHERE id='bare'"#,
                [],
            )
            .unwrap();
        let json = db.load_json_v6("bare").unwrap().unwrap();
        assert!(json.get("mode").is_none() && json.get("workspace").is_none());
        // Only the literal null is allowed through, and only with no context row.
        assert_eq!(json.get("contextState"), Some(&Value::Null));
        assert_eq!(json["id"], "bare");
        assert_eq!(json["title"], "Chat bare");
        assert_eq!(json["updatedAt"], 1000);
        assert_eq!(json["messages"], json!([]));
        assert_eq!(json["ok"], 1);
    }

    #[test]
    fn a_null_context_state_survives_through_metadata_until_a_real_context_exists() {
        // A1's `null_context_extra_key`: v5 saved `contextState:null` and two other extra keys.
        let fixture = crate::storage::fixtures::v5_sessions()
            .into_iter()
            .find(|s| s["id"] == "null_context_extra_key")
            .unwrap();
        assert_eq!(fixture["contextState"], Value::Null);
        let metadata: Map<String, Value> = fixture
            .as_object()
            .unwrap()
            .iter()
            .filter(|(key, _)| {
                !["id", "title", "updatedAt", "mode", "workspace", "messages"]
                    .contains(&key.as_str())
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        assert!(metadata.contains_key("contextState") && metadata.contains_key("pinned"));
        let db = v6_in_memory();
        make(&db, "s");
        let patch = MetadataPatch {
            title: None,
            metadata: Some(metadata.clone()),
        };
        db.update_session_metadata("s", &patch, 1).unwrap();
        let json = db.load_json_v6("s").unwrap().unwrap();
        assert_eq!(json["contextState"], Value::Null);
        assert!(json.as_object().unwrap().contains_key("contextState"));
        assert_eq!(json["contextReport"], Value::Null);
        assert_eq!(json["pinned"], true);
        // A non-null contextState is still refused, whatever else the patch holds.
        let mut bad = metadata;
        bad.insert("contextState".into(), json!({"transcript": []}));
        let refused = db.update_session_metadata(
            "s",
            &MetadataPatch {
                title: None,
                metadata: Some(bad),
            },
            2,
        );
        assert!(
            matches!(refused, Err(StorageError::Invalid(_))),
            "{refused:?}"
        );
        assert_eq!(db.load_chat("s").unwrap().unwrap().revision, 2);
        // A real context row wins over the stored null.
        let user = json!({"role": "user", "content": "hello 0"});
        write(&db, "s", |tx, c| {
            append_ui_user(tx, "s", c, &user_message(0))?;
            append_model(tx, "s", c, &user)?;
            let row = ContextRow {
                model_from: 0,
                model_len: 1,
                covered_messages: 1,
                source_hash: fingerprint(std::slice::from_ref(&user)),
                server_usage: None,
            };
            write_context(tx, "s", c, &row, &[], false)
        })
        .unwrap();
        let json = db.load_json_v6("s").unwrap().unwrap();
        assert_eq!(json["contextState"]["transcript"], json!([user]));
    }

    #[test]
    fn appending_a_checkpoint_reads_no_checkpoint_body() {
        let db = v6_in_memory();
        make(&db, "s");
        let row = ContextRow {
            model_from: 0,
            model_len: 0,
            covered_messages: 0,
            source_hash: "h".into(),
            server_usage: None,
        };
        let big = json!({"summary": "x".repeat(200_000)});
        write(&db, "s", |tx, c| {
            write_context(tx, "s", c, &row, &[big.clone(), big.clone()], false)
        })
        .unwrap();
        // The query behind the non-replace path is one primary-key seek, never a scan of bodies.
        let plan: Vec<String> = {
            let mut statement = db
                .connection()
                .prepare(&format!("EXPLAIN QUERY PLAN {NEXT_ORDINAL_SQL}"))
                .unwrap();
            let rows = statement
                .query_map(["s"], |r| r.get::<_, String>(3))
                .unwrap();
            rows.collect::<Result<_, _>>().unwrap()
        };
        let plan = plan.join(" | ");
        assert!(plan.contains("PRIMARY KEY"), "{plan}");
        assert!(!plan.contains("SCAN"), "{plan}");
        // One statement to find the ordinal, one insert, one context upsert: no sum, no delete.
        let before = statements();
        write(&db, "s", |tx, c| {
            write_context(tx, "s", c, &row, std::slice::from_ref(&big), false)
        })
        .unwrap();
        let appended = statements() - before;
        let before = statements();
        write(&db, "s", |tx, c| {
            write_context(tx, "s", c, &row, std::slice::from_ref(&big), true)
        })
        .unwrap();
        let replaced = statements() - before;
        // write(): read_counters + write_counters = 2 around the call.
        assert_eq!(appended, 2 + 3);
        assert_eq!(replaced, 2 + 4, "only a replace sums and deletes");
        assert_eq!(
            count(
                &db,
                "SELECT count(*)*100+max(ordinal) FROM chat_checkpoints WHERE session_id='s'"
            ),
            100,
            "one checkpoint, at ordinal 0"
        );
        // A corrupt (too small) total cannot underflow a replace.
        write(&db, "s", |tx, c| {
            write_context(tx, "s", c, &row, std::slice::from_ref(&big), false)
        })
        .unwrap();
        db.connection()
            .execute("UPDATE sessions SET total_bytes=1 WHERE id='s'", [])
            .unwrap();
        write(&db, "s", |tx, c| {
            write_context(tx, "s", c, &row, std::slice::from_ref(&big), true)
        })
        .unwrap();
        let repaired = db.load_chat("s").unwrap().unwrap();
        assert_eq!(repaired.id, "s");
        let tx = write_tx(db.connection()).unwrap();
        assert_eq!(recount(&tx, "s").unwrap(), read_counters(&tx, "s").unwrap());
    }

    #[test]
    fn context_state_json_is_the_raw_view_value() {
        // A1's `compacted` fixture has three checkpoints and a server usage. Store its context
        // as the upgrade would, then read the session JSON back: the contextState must be
        // identical, `serverUsage` included.
        let source = crate::storage::fixtures::v5_sessions()
            .into_iter()
            .find(|s| s["id"] == "compacted")
            .unwrap();
        let context = source["contextState"].clone();
        assert!(context["serverUsage"].is_object());
        let db = v6_in_memory();
        make(&db, "s");
        let row = ContextRow {
            model_from: 0,
            model_len: context["transcript"].as_array().unwrap().len() as u64,
            covered_messages: context["coveredMessages"].as_u64().unwrap(),
            source_hash: context["sourceHash"].as_str().unwrap().to_owned(),
            server_usage: Some(context["serverUsage"].to_string()),
        };
        write(&db, "s", |tx, c| {
            for entry in context["transcript"].as_array().unwrap() {
                append_model(tx, "s", c, entry)?;
            }
            write_context(
                tx,
                "s",
                c,
                &row,
                context["checkpoints"].as_array().unwrap(),
                false,
            )
        })
        .unwrap();
        let chat = db.load_chat("s").unwrap().unwrap();
        assert!(
            matches!(chat.context, StoredContext::Valid(_)),
            "{:?}",
            chat.context
        );
        let json = db.load_json_v6("s").unwrap().unwrap();
        assert_eq!(json["contextState"], context);
        // The same context with NULL server_usage has no serverUsage key at all.
        db.connection()
            .execute("UPDATE chat_context SET server_usage=NULL", [])
            .unwrap();
        let json = db.load_json_v6("s").unwrap().unwrap();
        let mut without = context.clone();
        without.as_object_mut().unwrap().remove("serverUsage");
        assert_eq!(json["contextState"], without);
        assert!(!json["contextState"]
            .as_object()
            .unwrap()
            .contains_key("serverUsage"));
    }

    #[test]
    fn validate_ui_message_enforces_the_shape() {
        let ok = user_message(0);
        validate_ui_message(&ok, "user").unwrap();
        let bad = |change: &dyn Fn(&mut Value)| {
            let mut m = ok.clone();
            change(&mut m);
            assert!(
                matches!(
                    validate_ui_message(&m, "user"),
                    Err(StorageError::Invalid(_))
                ),
                "{m}"
            );
        };
        assert!(validate_ui_message(&json!("text"), "user").is_err());
        assert!(validate_ui_message(&ok, "assistant").is_err());
        bad(&|m| m["id"] = json!(""));
        bad(&|m| m["id"] = json!("i".repeat(129)));
        bad(&|m| m["id"] = json!(7));
        bad(&|m| m["role"] = json!("assistant"));
        bad(&|m| m["content"] = json!(null));
        bad(&|m| m["attachments"] = json!("none"));
        bad(&|m| {
            m.as_object_mut().unwrap().remove("attachments");
        });
        bad(&|m| m["attachments"] = json!([{"sizeBytes": "big"}]));
        let mut at_limit = ok.clone();
        at_limit["id"] = json!("i".repeat(128));
        at_limit["attachments"] = json!([{"name": "no size"}]);
        validate_ui_message(&at_limit, "user").unwrap();
    }

    #[test]
    fn deleting_a_chat_removes_its_rows_and_scrubs_its_search_terms() {
        let db = v6_in_memory();
        make(&db, "gone");
        make(&db, "kept");
        run(&db, "gone", 0);
        run(&db, "kept", 0);
        let c = db.connection();
        // `run` already indexed its own messages, so these keys use ids and seqs of their own.
        for (rowid, session, term) in [
            (1001, "gone", "zqxjgoneterm"),
            (1002, "kept", "zqxjkeptterm"),
        ] {
            c.execute(
                "INSERT INTO chat_search_keys(id,session_id,seq,message_index,message_id,role) VALUES (?1,?2,?1,0,'m','user')",
                params![rowid, session],
            )
            .unwrap();
            c.execute(
                "INSERT INTO chat_search(rowid,content) VALUES (?1, ?2)",
                params![rowid, format!("alpha {term} omega")],
            )
            .unwrap();
        }
        let contains = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
        assert!(contains(&shadow_bytes(&db), b"zqxjgoneterm"));
        db.delete_v6("gone").unwrap();
        let shadow = shadow_bytes(&db);
        assert!(
            !contains(&shadow, b"zqxjgoneterm"),
            "the deleted chat's term survived"
        );
        assert!(contains(&shadow, b"zqxjkeptterm"));
        assert!(db.load_chat("gone").unwrap().is_none());
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM chat_events WHERE session_id='gone'"
            ),
            0
        );
        assert!(db.load_chat("kept").unwrap().is_some());
        db.delete_v6("gone").unwrap(); // idempotent, as v5's delete is
    }

    #[test]
    fn statements_counts_prepared_chat_store_statements() {
        let db = v6_in_memory();
        make(&db, "s");
        let before = statements();
        run(&db, "s", 0);
        assert!(statements() > before);
        let before = statements();
        db.list_v6().unwrap();
        assert_eq!(statements() - before, 1);
    }

    /// Every SQL string in the chat store that names `chat_events`, split into statements.
    fn chat_events_statements(source: &str) -> Vec<String> {
        let code: String = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut found = vec![];
        let mut from = 0;
        while let Some(at) = code[from..].find("chat_events") {
            let at = from + at;
            let after = code[at + "chat_events".len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
            let before = code[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
            from = at + "chat_events".len();
            if after || before {
                continue;
            }
            // The string literal around it: the nearest unescaped quote on each side.
            let quote = |text: &str, forward: bool| {
                let bytes = text.as_bytes();
                let positions: Vec<usize> = (0..bytes.len())
                    .filter(|&i| bytes[i] == b'"' && (i == 0 || bytes[i - 1] != b'\\'))
                    .collect();
                if forward {
                    positions.first().copied()
                } else {
                    positions.last().copied()
                }
            };
            let start = quote(&code[..at], false).map_or(0, |i| i + 1);
            let end = quote(&code[at..], true).map_or(code.len(), |i| at + i);
            for statement in code[start..end].split(';') {
                if statement
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .any(|t| t == "chat_events")
                {
                    found.push(statement.trim().to_owned());
                }
            }
        }
        found
    }
    /// Only `INSERT`, `SELECT` and the table's own `CREATE` may name `chat_events`.
    fn is_add_only(statement: &str) -> bool {
        let tokens: Vec<String> = statement
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .filter(|t| !t.is_empty())
            .map(str::to_ascii_uppercase)
            .collect();
        match tokens.first().map(String::as_str) {
            Some("CREATE") => true,
            Some("INSERT" | "SELECT") => !tokens.iter().any(|t| {
                matches!(
                    t.as_str(),
                    "UPDATE" | "DELETE" | "REPLACE" | "CONFLICT" | "DROP" | "ALTER"
                )
            }),
            _ => false,
        }
    }

    #[test]
    fn chat_events_sql_is_insert_or_select_only() {
        // The scanner itself: it flags what it should.
        let sample = "x(\"UPDATE chat_events SET body=''\"); y(\"SELECT 1 FROM chat_events WHERE seq>1\");\n// \"DELETE FROM chat_events\"\nz(\"INSERT OR REPLACE INTO chat_events(a) VALUES (1)\"); w(\"DELETE FROM chat_events\")";
        let found = chat_events_statements(sample);
        assert_eq!(found.len(), 4, "{found:?}");
        assert_eq!(
            found.iter().map(|s| is_add_only(s)).collect::<Vec<_>>(),
            [false, true, false, false]
        );
        // The real sources, test modules and test-only files excluded.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = vec![root.join("storage.rs")];
        for entry in fs::read_dir(root.join("storage")).unwrap() {
            files.push(entry.unwrap().path());
        }
        let (mut inserts, mut selects) = (0, 0);
        for path in files
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        {
            let name = path.file_name().unwrap().to_string_lossy();
            if name == "fixtures.rs" || name.ends_with("_tests.rs") {
                continue;
            }
            let text = fs::read_to_string(path).unwrap();
            let production = text.split("\nmod tests {").next().unwrap();
            for statement in chat_events_statements(production) {
                assert!(is_add_only(&statement), "{name}: {statement}");
                inserts += statement.starts_with("INSERT") as usize;
                selects += statement.starts_with("SELECT") as usize;
            }
        }
        assert!(
            inserts >= 1 && selects >= 1,
            "the scan found {inserts} inserts, {selects} selects"
        );
    }

    #[test]
    fn two_connections_appending_keep_contiguous_seqs_and_exact_counters() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-a5-{}", rand::random::<u64>()));
        let main = LocalStore::open_at(&directory, 5).unwrap();
        let tx = main.connection().unchecked_transaction().unwrap();
        apply_v6_schema(&tx).unwrap();
        tx.commit().unwrap();
        make(&main, "s");
        let path = directory.join(crate::storage::DATABASE_FILE);
        let workers: Vec<_> = (0..2)
            .map(|worker| {
                let path = path.clone();
                thread::spawn(move || {
                    let connection = Connection::open(path).unwrap();
                    configure(&connection, Duration::from_millis(20)).unwrap();
                    let store = LocalStore { connection };
                    for n in 0..200 {
                        let entry = json!({"role": "user", "content": format!("{worker}-{n}")});
                        loop {
                            match write(&store, "s", |tx, c| {
                                append_model(tx, "s", c, &entry).map(drop)
                            }) {
                                Ok(()) => break,
                                Err(error) if is_busy(&error) => thread::yield_now(),
                                Err(error) => panic!("{error:?}"),
                            }
                        }
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let events = rows(&main, "s");
        assert_eq!(
            events.iter().map(|r| r.0).collect::<Vec<_>>(),
            (0..400).collect::<Vec<_>>()
        );
        let mut bodies: Vec<_> = events.iter().map(|r| r.2.clone()).collect();
        bodies.sort();
        bodies.dedup();
        assert_eq!(bodies.len(), 400, "every append landed once");
        let stored = counters(&main, "s");
        let tx = write_tx(main.connection()).unwrap();
        assert_eq!(recount(&tx, "s").unwrap(), stored);
        assert_eq!((stored.next_seq, stored.model_count), (400, 400));
        drop(tx);
        drop(main);
        let _ = fs::remove_dir_all(&directory);
    }
}
