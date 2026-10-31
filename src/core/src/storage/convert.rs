//! Converts a v5 chat (a whole `sessions.body` JSON) to v6 rows, inside the caller's transaction.
//!
//! Each value the v5 body holds twice becomes a reference (see `view`). The chat is then rebuilt
//! through the load path and must equal the v5 body; if it does not, the conversion fails and the
//! caller rolls everything back.

use super::chats::{append, read_chat, write_context, write_tx};
use super::view::{accumulate, ContextRow, Kind};
use super::{search, validate_session, LocalStore, StorageError, MAX_SESSIONS};
use crate::{model::message_form, protocol::ChatMessage};
use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

pub(crate) const NOT_CONVERTED: &str =
    "Couldn't upgrade a saved conversation, so nothing was changed. Your existing data has been left in place.";

/// Converts the chat whose `sessions` row has id `session["id"]` and no rows yet. Its v5 body is
/// emptied.
pub(crate) fn convert_session(tx: &Connection, session: &Value) -> Result<(), StorageError> {
    if !write_chat(tx, session, vec![], vec![])? {
        eprintln!(
            "warn: chat {} did not rebuild to its saved copy",
            session["id"].as_str().unwrap_or_default()
        );
        return Err(StorageError::Invalid(NOT_CONVERTED));
    }
    Ok(())
}

/// Saves a whole session JSON over its chat. When the stored messages and transcript begin the new
/// ones, only the rest is appended and the context replaced; otherwise the chat is rewritten.
pub(crate) fn save(tx: &Connection, session: &Value) -> Result<(), StorageError> {
    let (id, title, updated_at, _) = validate_session(session)?;
    let Some((stored, ui)) = read_chat(tx, id)? else {
        let count: i64 = tx.query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))?;
        if count >= MAX_SESSIONS {
            return Err(StorageError::Invalid("Your history has reached 500 conversations. Export or delete an older conversation before starting another."));
        }
        tx.execute(
            "INSERT INTO sessions(id,title,updated_at,body) VALUES (?1,?2,?3,'')",
            params![id, title, updated_at],
        )?;
        return convert_session(tx, session);
    };
    let old = transcript(&stored);
    if messages(session).starts_with(messages(&stored)) && transcript(session).starts_with(old) {
        let model = tx
            .prepare(
                "SELECT seq FROM chat_events WHERE session_id=?1 AND kind='model'
                 AND seq >= (SELECT model_from FROM chat_context WHERE session_id=?1)
                 ORDER BY seq LIMIT ?2",
            )?
            .query_map(params![id, old.len() as i64], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        if write_chat(tx, session, ui, model)? {
            return Ok(());
        }
    }
    tx.execute("DELETE FROM chat_events WHERE session_id=?1", [id])?;
    tx.execute("DELETE FROM chat_context WHERE session_id=?1", [id])?;
    convert_session(tx, session)
}

fn messages(session: &Value) -> &[Value] {
    session["messages"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
}

fn transcript(session: &Value) -> &[Value] {
    session
        .get("contextState")
        .and_then(|context| context["transcript"].as_array())
        .map_or(&[][..], Vec::as_slice)
}

/// Writes `session` over its chat: the `sessions` columns, the messages and transcript entries
/// after the first `ui.len()` and `model.len()`, which are stored at those seqs, and the context.
/// Returns whether the chat then rebuilds to `session`.
fn write_chat(
    tx: &Connection,
    session: &Value,
    mut ui: Vec<i64>,
    mut model: Vec<i64>,
) -> Result<bool, StorageError> {
    let (id, title, updated_at, _) = validate_session(session)?;
    let text = |key: &str| session.get(key).and_then(Value::as_str);
    // Every key no column or row holds stays in `metadata`, a literal `contextState: null` too.
    let metadata: Map<String, Value> = session
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, value)| match key.as_str() {
            "id" | "title" | "updatedAt" | "messages" => false,
            "mode" | "workspace" => !value.is_string(),
            "contextState" => value.is_null(),
            _ => true,
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    tx.execute(
        "UPDATE sessions SET title=?2,updated_at=?3,body='',mode=?4,workspace=?5,metadata=?6
         WHERE id=?1",
        params![
            id,
            title,
            updated_at,
            text("mode"),
            text("workspace"),
            serde_json::to_string(&metadata)?
        ],
    )?;
    let messages = messages(session);
    let transcript = transcript(session);
    let (old_ui, old_model) = (ui.len(), model.len());
    let mut next: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq)+1,0) FROM chat_events WHERE session_id=?1",
        [id],
        |row| row.get(0),
    )?;
    for (seqs, len) in [(&mut ui, messages.len()), (&mut model, transcript.len())] {
        while seqs.len() < len {
            seqs.push(next);
            next += 1;
        }
    }
    let (mut ui_bodies, mut model_bodies) = (messages.to_vec(), transcript.to_vec());
    add_references(
        messages,
        transcript,
        (&ui, &model),
        &mut ui_bodies,
        &mut model_bodies,
    );
    for (message, body) in messages.iter().zip(&ui_bodies).skip(old_ui) {
        let kind = if body["role"] == "user" {
            Kind::UiUser
        } else {
            Kind::UiAssistant
        };
        let row = append(tx, id, kind, body)?;
        search::index(tx, row, message)?;
    }
    for body in model_bodies.iter().skip(old_model) {
        append(tx, id, Kind::Model, body)?;
    }
    if let Some(context) = session["contextState"].as_object() {
        let mut state = context.clone();
        state.remove("transcript");
        write_context(
            tx,
            id,
            &ContextRow {
                model_from: model.first().copied().unwrap_or(next),
                model_len: transcript.len(),
                state,
            },
        )?;
    }
    match read_chat(tx, id) {
        Ok(Some((chat, _))) => Ok(chat == *session),
        Ok(None) | Err(StorageError::Invalid(_) | StorageError::Json(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

/// The browser's legacy import on a v6 store, once and all or nothing, as v5 runs it. Ids already
/// stored are skipped.
pub(crate) fn migrate_v6(store: &LocalStore, sessions: &[Value]) -> Result<(), StorageError> {
    if store.setting("legacy_sessions_migrated")?.is_some() {
        return Ok(());
    }
    if sessions.len() > 30 {
        return Err(StorageError::Invalid(
            "The old history contains too many conversations to migrate safely.",
        ));
    }
    let tx = write_tx(&store.connection)?;
    for session in sessions {
        // Only the backend writes model rows: a webview-supplied transcript is never stored.
        let mut session = session.clone();
        if let Some(object) = session.as_object_mut() {
            object.remove("contextState");
        }
        let (id, title, updated_at, _) = validate_session(&session)?;
        let added = tx.execute(
            "INSERT OR IGNORE INTO sessions(id,title,updated_at,body) VALUES (?1,?2,?3,'')",
            params![id, title, updated_at],
        )?;
        if added == 1 {
            convert_session(&tx, &session)?;
        }
    }
    tx.execute(
        "INSERT INTO settings(key,value) VALUES ('legacy_sessions_migrated','true')",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

// ---- references ----

fn role(entry: &Value) -> Option<&str> {
    entry.get("role")?.as_str()
}

fn json_len(value: &Value) -> usize {
    serde_json::to_string(value).map_or(0, |text| text.len())
}

/// Writes each value the transcript repeats as a reference into `ui` and `model`, where that is
/// smaller. `seqs` holds each message's and each transcript entry's seq.
fn add_references(
    messages: &[Value],
    transcript: &[Value],
    (ui_seq, model_seq): (&[i64], &[i64]),
    ui: &mut [Value],
    model: &mut [Value],
) {
    // User refs: each user entry to the first user message at or after the last match whose
    // model form it is, else to the first one anywhere.
    let mut forms: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        if message["role"] != "user" {
            continue;
        }
        if let Ok(chat) = serde_json::from_value::<ChatMessage>(message.clone()) {
            let key = message_form(&chat, &[]).to_string();
            forms.entry(key).or_default().push(index);
        }
    }
    let mut next = 0;
    let mut last_ref = HashMap::new();
    for (row, entry) in transcript.iter().enumerate() {
        let Some(candidates) = forms.get(&entry.to_string()) else {
            continue;
        };
        let at = candidates.partition_point(|&index| index < next);
        let Some(&message) = candidates.get(at).or(candidates.first()) else {
            continue;
        };
        model[row] = json!({ "ref": ui_seq[message] });
        last_ref.insert(message, row);
        next = message + 1;
    }

    // Each assistant's turns: the transcript's assistant rows after its user's last ref, up to
    // the next user row or ref.
    let mut last_user = None;
    for (index, message) in messages.iter().enumerate() {
        if message["role"] == "user" {
            last_user = Some(index);
            continue;
        }
        let Some(&start) = last_user.and_then(|user| last_ref.get(&user)) else {
            continue;
        };
        let end = (start + 1..transcript.len())
            .find(|&r| model[r].get("ref").is_some() || role(&transcript[r]) == Some("user"))
            .unwrap_or(transcript.len());
        let mut turns: Vec<usize> = (start + 1..end)
            .filter(|&r| role(&transcript[r]) == Some("assistant"))
            .collect();
        let content = message["content"].as_str().unwrap_or_default();
        let (tail, echo) = content_form(transcript, &mut turns, content);
        if let Some(echo) = echo {
            model[echo] = json!({ "ref": ui_seq[index] });
        }
        if let Some(tail) = tail {
            let mut form =
                json!({ "turns": turns.iter().map(|&t| model_seq[t]).collect::<Vec<_>>() });
            if !tail.is_empty() {
                form["tail"] = json!(tail);
            }
            if json_len(&form) < json_len(&message["content"]) {
                ui[index]["content"] = form;
            }
        }
        let tools = message["tools"].as_array().map_or(&[][..], Vec::as_slice);
        for (position, tool) in tools.iter().enumerate() {
            let Some((turn, call, result)) = tool_source(transcript, &turns, end, tool) else {
                continue;
            };
            let function = &transcript[turn]["tool_calls"][call]["function"];
            let output = result.map(|row| &transcript[row]["content"]);
            if tool.get("name") != Some(&function["name"])
                || tool.get("arguments") != Some(&function["arguments"])
                || tool.get("output") != output
            {
                continue;
            }
            let mut residual = tool.as_object().cloned().unwrap_or_default();
            residual.remove("name");
            residual.remove("arguments");
            if result.is_some() {
                residual.remove("output");
            }
            residual.insert("call".into(), json!([model_seq[turn], call]));
            residual.insert("result".into(), json!(result.map(|row| model_seq[row])));
            let residual = Value::Object(residual);
            if json_len(&residual) < json_len(tool) {
                ui[index]["tools"][position] = residual;
            }
        }
    }
}

/// How an assistant's content is written over `turns`: `Some(tail)` when it is their joined text
/// plus `tail`. It also finds the echo, the final copy of the message the agent appends to the
/// transcript, which is then left out of `turns`.
fn content_form(
    transcript: &[Value],
    turns: &mut Vec<usize>,
    content: &str,
) -> (Option<String>, Option<usize>) {
    let Some(mut texts) = turns
        .iter()
        .map(|&row| transcript[row]["content"].as_str())
        .collect::<Option<Vec<_>>>()
    else {
        return (None, None);
    };
    if texts.is_empty() {
        return (None, None);
    }
    if accumulate(texts.iter().copied()) == content {
        return (Some(String::new()), None);
    }
    let mut echo = None;
    if let Some(&last) = turns.last() {
        if transcript[last] == json!({"role": "assistant", "content": content}) {
            echo = turns.pop();
            texts.pop();
            if turns.is_empty() {
                return (None, echo);
            }
        }
    }
    let covered = accumulate(texts.iter().copied());
    match content.strip_prefix(covered.as_str()) {
        Some(tail) => (Some(tail.to_owned()), echo),
        None => (None, echo),
    }
}

/// Where a UI tool came from: its id `{turn}_{index}_{call id}` names call `index` of the run's
/// turn `turn`, whose result is the first matching tool row after that turn and before `end`.
/// Returns the turn row, the call index and the result row.
fn tool_source(
    transcript: &[Value],
    turns: &[usize],
    end: usize,
    tool: &Value,
) -> Option<(usize, usize, Option<usize>)> {
    let (turn, rest) = tool.get("id")?.as_str()?.split_once('_')?;
    let (index, call_id) = rest.split_once('_')?;
    let digits = |text: &str| text.bytes().all(|b| b.is_ascii_digit());
    if !digits(turn) || !digits(index) {
        return None;
    }
    let turn = *turns.get(turn.parse::<usize>().ok()?)?;
    let index = index.parse::<usize>().ok()?;
    let call = transcript[turn].get("tool_calls")?.get(index)?;
    if call.get("id")?.as_str()? != call_id {
        return None;
    }
    let result = (turn + 1..end).find(|&row| {
        role(&transcript[row]) == Some("tool")
            && transcript[row].get("tool_call_id") == call.get("id")
    });
    Some((turn, index, result))
}
