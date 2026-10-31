//! The v6 chat store: each chat is a `sessions` row plus its add-only `chat_events` rows, and its
//! model context in `chat_context`. A chat is rebuilt from those rows on load (see `view`).
//!
//! Dormant: `LocalStore::open` still opens v5. `open_at(directory, 6)` upgrades (see `upgrade`).

use super::view::{build_view, ContextRow, EventRow, Kind};
use super::{search, validate_id, validate_message, LocalStore, StorageError};
use super::{MAX_SESSIONS, MAX_SESSION_BYTES};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Map, Value};

/// Turns a v5 database into the v6 shape, inside the caller's transaction. The v5 `sessions.body`
/// column stays; v6 rows leave it empty. The v5 search tables go: `chat_search` replaces them.
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
        CREATE TABLE chat_events (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            seq INTEGER NOT NULL,
            kind TEXT NOT NULL CHECK (kind IN ('ui_user','ui_assistant','model')),
            body TEXT NOT NULL,
            UNIQUE (session_id, seq));
        CREATE TABLE chat_context (
            session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
            model_from INTEGER NOT NULL,
            model_len INTEGER NOT NULL,
            state TEXT NOT NULL);
        CREATE VIRTUAL TABLE chat_search USING fts5(content, content='', contentless_delete=1);
        INSERT INTO chat_search(chat_search, rank) VALUES ('secure-delete', 1);
        CREATE TRIGGER chat_events_search_ad AFTER DELETE ON chat_events WHEN old.kind <> 'model'
            BEGIN DELETE FROM chat_search WHERE rowid = old.id; END;",
    )?;
    Ok(())
}

/// A new chat's columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSession {
    pub id: String,
    pub title: String,
    pub mode: Option<String>,
    pub workspace: Option<String>,
    pub updated_at: i64,
}

/// A chat write: it takes the writer lock up front.
pub(crate) fn write_tx(connection: &Connection) -> Result<Transaction<'_>, StorageError> {
    Ok(Transaction::new_unchecked(
        connection,
        TransactionBehavior::Immediate,
    )?)
}

/// Adds one row after the chat's last, and returns its `chat_events.id`.
pub(crate) fn append(
    tx: &Connection,
    id: &str,
    kind: Kind,
    body: &Value,
) -> Result<i64, StorageError> {
    tx.prepare_cached(
        "INSERT INTO chat_events(session_id,seq,kind,body)
         SELECT ?1, COALESCE(MAX(seq)+1,0), ?2, ?3 FROM chat_events WHERE session_id=?1",
    )?
    .execute(params![id, kind.name(), serde_json::to_string(body)?])?;
    Ok(tx.last_insert_rowid())
}

pub(crate) fn write_context(
    tx: &Connection,
    id: &str,
    context: &ContextRow,
) -> Result<(), StorageError> {
    tx.execute(
        "INSERT INTO chat_context(session_id,model_from,model_len,state) VALUES (?1,?2,?3,?4)
         ON CONFLICT(session_id) DO UPDATE SET model_from=excluded.model_from,
         model_len=excluded.model_len,state=excluded.state",
        params![
            id,
            context.model_from,
            i64::try_from(context.model_len).unwrap_or(i64::MAX),
            serde_json::to_string(&context.state)?
        ],
    )?;
    Ok(())
}

/// A chat as today's session JSON, and its UI messages with their seqs. It starts no
/// transaction, so the caller owns the snapshot.
pub(crate) fn read_chat(
    conn: &Connection,
    id: &str,
) -> Result<Option<(Value, Vec<i64>)>, StorageError> {
    type Columns = (String, i64, Option<String>, Option<String>, String);
    let columns: Option<Columns> = conn
        .prepare_cached(
            "SELECT title,updated_at,mode,workspace,metadata FROM sessions WHERE id=?1",
        )?
        .query_row([id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .optional()?;
    let Some((title, updated_at, mode, workspace, metadata)) = columns else {
        return Ok(None);
    };
    let mut events = vec![];
    let mut statement = conn
        .prepare_cached("SELECT seq,kind,body FROM chat_events WHERE session_id=?1 ORDER BY seq")?;
    let mut rows = statement.query([id])?;
    while let Some(row) = rows.next()? {
        events.push(EventRow {
            seq: row.get(0)?,
            kind: Kind::parse(&row.get::<_, String>(1)?)?,
            body: serde_json::from_str(&row.get::<_, String>(2)?)?,
        });
    }
    let context = conn
        .prepare_cached("SELECT model_from,model_len,state FROM chat_context WHERE session_id=?1")?
        .query_row([id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .optional()?
        .map(|(model_from, model_len, state)| {
            Ok::<_, StorageError>(ContextRow {
                model_from,
                model_len: usize::try_from(model_len).unwrap_or(usize::MAX),
                state: serde_json::from_str(&state)?,
            })
        })
        .transpose()?;
    let (messages, context) = build_view(&events, context)?;
    // Metadata keys first, so a column or the log always wins over a key of the same name.
    let mut chat: Map<String, Value> = serde_json::from_str(&metadata)?;
    chat.insert("id".into(), json!(id));
    chat.insert("title".into(), json!(title));
    chat.insert("updatedAt".into(), json!(updated_at));
    for (key, value) in [("mode", mode), ("workspace", workspace)] {
        if let Some(value) = value {
            chat.insert(key.into(), json!(value));
        }
    }
    let (seqs, messages): (Vec<i64>, Vec<Value>) = messages.into_iter().unzip();
    chat.insert("messages".into(), Value::Array(messages));
    if let Some(context) = context {
        chat.insert("contextState".into(), context);
    }
    Ok(Some((Value::Object(chat), seqs)))
}

impl LocalStore {
    /// Adds an empty chat. There may be at most 500.
    pub fn create_chat(&self, chat: &NewSession) -> Result<(), StorageError> {
        validate_id(&chat.id)?;
        if chat.title.is_empty() || chat.title.len() > 1000 {
            return Err(StorageError::Invalid("Invalid conversation title."));
        }
        let tx = write_tx(&self.connection)?;
        let count: i64 = tx.query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))?;
        if count >= MAX_SESSIONS {
            return Err(StorageError::Invalid("Your history has reached 500 conversations. Export or delete an older conversation before starting another."));
        }
        let added = tx.execute(
            "INSERT OR IGNORE INTO sessions(id,title,updated_at,body,mode,workspace)
             VALUES (?1,?2,?3,'',?4,?5)",
            params![
                chat.id,
                chat.title,
                chat.updated_at,
                chat.mode,
                chat.workspace
            ],
        )?;
        if added == 0 {
            return Err(StorageError::Invalid("This conversation already exists."));
        }
        tx.commit()?;
        Ok(())
    }

    /// Adds a user or assistant message at the end of a chat and indexes it for search.
    pub fn add_message(
        &self,
        id: &str,
        message: &Value,
        updated_at: i64,
    ) -> Result<(), StorageError> {
        validate_id(id)?;
        validate_message(message)?;
        if serde_json::to_string(message)?.len() > MAX_SESSION_BYTES {
            return Err(StorageError::Invalid(
                "This conversation is too large to save. Export it before closing Blackwall.",
            ));
        }
        let kind = if message["role"] == "user" {
            Kind::UiUser
        } else {
            Kind::UiAssistant
        };
        let tx = write_tx(&self.connection)?;
        let changed = tx.execute(
            "UPDATE sessions SET updated_at=max(updated_at,?2) WHERE id=?1",
            params![id, updated_at],
        )?;
        if changed == 0 {
            return Err(StorageError::NotFound);
        }
        let row = append(&tx, id, kind, message)?;
        search::index(&tx, row, message)?;
        tx.commit()?;
        Ok(())
    }

    /// A chat as today's session JSON, or `None`.
    pub fn load_chat(&self, id: &str) -> Result<Option<Value>, StorageError> {
        validate_id(id)?;
        let read = self.connection.unchecked_transaction()?;
        let chat = read_chat(&read, id)?;
        read.commit()?;
        Ok(chat.map(|(chat, _)| chat))
    }

    /// The newest 500 chats, from their `sessions` columns only.
    pub fn list_chats(&self) -> Result<Vec<Value>, StorageError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id,title,updated_at,mode,workspace FROM sessions
             ORDER BY updated_at DESC LIMIT 500",
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

    /// Deletes a chat; its rows, context and search entries go with it.
    pub fn delete_chat(&self, id: &str) -> Result<(), StorageError> {
        validate_id(id)?;
        self.connection
            .execute("DELETE FROM sessions WHERE id=?1", [id])?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;

    /// An in-memory v6 store.
    pub(crate) fn v6_store() -> LocalStore {
        let store = LocalStore::initialize(Connection::open_in_memory().unwrap()).unwrap();
        apply_v6_schema(store.connection()).unwrap();
        store
    }

    pub(crate) fn new_chat(store: &LocalStore, id: &str) {
        store
            .create_chat(&NewSession {
                id: id.into(),
                title: format!("Chat {id}"),
                mode: Some("agent".into()),
                workspace: Some("/work/project".into()),
                updated_at: 100,
            })
            .unwrap();
    }

    pub(crate) fn message(id: &str, role: &str, content: &str) -> Value {
        json!({"id": id, "role": role, "content": content, "attachments": []})
    }

    fn count(store: &LocalStore, sql: &str) -> i64 {
        store
            .connection()
            .query_row(sql, [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn added_messages_load_back_in_order() {
        let store = v6_store();
        new_chat(&store, "c1");
        let user = message("m1", "user", "Hello");
        let reply = json!({"id": "m2", "role": "assistant", "content": "Hi there",
            "attachments": [], "status": "complete", "tools": [{"id": "t", "name": "x"}]});
        store.add_message("c1", &user, 200).unwrap();
        store.add_message("c1", &reply, 150).unwrap();
        assert_eq!(
            store.load_chat("c1").unwrap().unwrap(),
            json!({"id": "c1", "title": "Chat c1", "updatedAt": 200, "mode": "agent",
                "workspace": "/work/project", "messages": [user, reply]})
        );
        assert_eq!(count(&store, "SELECT count(*) FROM chat_events"), 2);
        assert_eq!(store.load_chat("missing").unwrap(), None);
    }

    #[test]
    fn add_refuses_a_missing_chat_or_an_invalid_message() {
        let store = v6_store();
        new_chat(&store, "c1");
        assert!(matches!(
            store.add_message("nope", &message("m", "user", "x"), 1),
            Err(StorageError::NotFound)
        ));
        for bad in [
            json!({"role": "system", "content": "x", "attachments": []}),
            json!({"role": "user", "content": 3, "attachments": []}),
            json!({"role": "user", "content": "x"}),
        ] {
            assert!(store.add_message("c1", &bad, 1).is_err(), "{bad}");
        }
        assert_eq!(count(&store, "SELECT count(*) FROM chat_events"), 0);
        assert!(store
            .create_chat(&NewSession {
                id: "c1".into(),
                title: "Again".into(),
                mode: None,
                workspace: None,
                updated_at: 1,
            })
            .is_err());
    }

    #[test]
    fn list_is_newest_first_and_reads_no_events() {
        let store = v6_store();
        new_chat(&store, "old");
        store
            .create_chat(&NewSession {
                id: "new".into(),
                title: "Newer".into(),
                mode: None,
                workspace: None,
                updated_at: 300,
            })
            .unwrap();
        store
            .add_message("old", &message("m", "user", "x"), 50)
            .unwrap();
        assert_eq!(
            store.list_chats().unwrap(),
            [
                json!({"id": "new", "title": "Newer", "updatedAt": 300, "messages": [],
                    "mode": "chat", "workspace": null}),
                json!({"id": "old", "title": "Chat old", "updatedAt": 100, "messages": [],
                    "mode": "agent", "workspace": "/work/project"}),
            ]
        );
    }

    #[test]
    fn delete_removes_the_chat_and_everything_keyed_to_it() {
        let store = v6_store();
        new_chat(&store, "gone");
        new_chat(&store, "kept");
        store
            .add_message("gone", &message("a", "user", "secretword here"), 1)
            .unwrap();
        store
            .add_message("kept", &message("b", "user", "other words"), 1)
            .unwrap();
        write_context(
            store.connection(),
            "gone",
            &ContextRow {
                model_from: 0,
                model_len: 0,
                state: Map::new(),
            },
        )
        .unwrap();
        store.delete_chat("gone").unwrap();
        assert_eq!(store.load_chat("gone").unwrap(), None);
        assert_eq!(count(&store, "SELECT count(*) FROM chat_events"), 1);
        assert_eq!(count(&store, "SELECT count(*) FROM chat_context"), 0);
        let indexed = |word: &str| {
            count(
                &store,
                &format!("SELECT count(*) FROM chat_search WHERE chat_search MATCH '{word}'"),
            )
        };
        assert_eq!(indexed("secretword"), 0);
        assert_eq!(indexed("other"), 1);
    }
}
