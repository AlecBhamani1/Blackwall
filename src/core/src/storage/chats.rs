//! The v6 chat event-log schema and the caps that guard it.
//!
//! Dormant: `LocalStore::open` still opens v5. Nothing here runs outside tests until the upgrade lands.
#[cfg(test)]
use super::LocalStore;
use super::StorageError;
use rusqlite::Connection;

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
    use crate::storage::configure;
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
        inside.execute_batch("BEGIN;").unwrap();
        assert!(configure(&inside, Duration::from_millis(1)).is_err());
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
}
