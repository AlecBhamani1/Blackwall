//! Search only saved message text. Tool calls, attachments and approval records stay inert.
use super::{LocalStore, StorageError};
use crate::memory::MemoryClient;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MIGRATION_BATCH: i64 = 5;
const SESSION_TEXT_BYTES: usize = 512 * 1024;
const MESSAGE_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecallQuery {
    pub query: String,
    pub project: Option<String>,
    /// Inclusive Unix timestamps in milliseconds, using the saved session's updated date.
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub limit: Option<usize>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecallHit {
    pub session_id: String,
    pub message_id: String,
    pub message_index: u32,
    pub title: String,
    pub mode: String,
    pub project: Option<String>,
    pub updated_at: i64,
    pub role: String,
    pub excerpt: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecallResults {
    pub results: Vec<RecallHit>,
    pub remaining_sessions: u32,
}

pub(super) fn schema(db: &Connection) -> Result<(), StorageError> {
    db.execute_batch("
        CREATE TABLE IF NOT EXISTS indexed_sessions (
            session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
            project TEXT, mode TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS session_messages (
            id INTEGER PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            message_id TEXT NOT NULL, message_index INTEGER NOT NULL, role TEXT NOT NULL,
            content TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS session_messages_session ON session_messages(session_id);
        CREATE VIRTUAL TABLE IF NOT EXISTS message_search USING fts5(content, content='session_messages', content_rowid='id');
        INSERT INTO message_search(message_search, rank) VALUES ('secure-delete', 1);
        CREATE TRIGGER IF NOT EXISTS message_insert AFTER INSERT ON session_messages BEGIN
            INSERT INTO message_search(rowid,content) VALUES (new.id,new.content); END;
        CREATE TRIGGER IF NOT EXISTS message_delete AFTER DELETE ON session_messages BEGIN
            INSERT INTO message_search(message_search,rowid,content) VALUES ('delete',old.id,old.content); END;
        PRAGMA user_version=4;
    ")?;
    Ok(())
}

pub(super) fn index(db: &Connection, session: &Value) -> Result<(), StorageError> {
    let id = session["id"]
        .as_str()
        .ok_or(StorageError::Invalid("Missing conversation identifier."))?;
    db.execute("DELETE FROM session_messages WHERE session_id=?1", [id])?;
    let mut remaining = SESSION_TEXT_BYTES;
    if let Some(messages) = session["messages"].as_array() {
        for (position, message) in messages.iter().take(2000).enumerate() {
            let role = message["role"].as_str().unwrap_or("");
            if !matches!(role, "user" | "assistant") {
                continue;
            }
            let text = message["content"].as_str().unwrap_or("");
            let mut end = text.len().min(MESSAGE_TEXT_BYTES).min(remaining);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if end == 0 {
                continue;
            }
            remaining -= end;
            let fallback = format!("message_{position}");
            let message_id = message["id"]
                .as_str()
                .filter(|id| id.len() <= 128)
                .unwrap_or(&fallback);
            db.execute("INSERT INTO session_messages(session_id,message_id,message_index,role,content) VALUES (?1,?2,?3,?4,?5)", params![id,message_id,position as i64,role,&text[..end]])?;
        }
    }
    let project = session["workspace"]
        .as_str()
        .filter(|path| path.len() <= 2048);
    let mode = match session["mode"].as_str() {
        Some("agent") => "agent",
        None if id.starts_with("cli_") => "agent",
        _ => "chat",
    };
    db.execute(
        "INSERT INTO indexed_sessions(session_id,project,mode) VALUES (?1,?2,?3) ON CONFLICT(session_id) DO UPDATE SET project=excluded.project,mode=excluded.mode",
        params![id,project,mode],
    )?;
    Ok(())
}

impl LocalStore {
    pub(super) fn message_index_is_current(&self) -> Result<bool, StorageError> {
        Ok(self.connection.query_row(
            "SELECT NOT EXISTS(
                SELECT 1 FROM sessions
                WHERE id NOT IN (SELECT session_id FROM indexed_sessions)
                LIMIT 1
            )",
            [],
            |row| row.get(0),
        )?)
    }

    /// Each database open backfills at most five existing sessions, in one transaction.
    /// New saves are indexed synchronously; no background worker retains deleted text.
    pub(super) fn migrate_message_index(&self) -> Result<(), StorageError> {
        let sessions = {
            let mut statement = self.connection.prepare("SELECT id, body FROM sessions WHERE id NOT IN (SELECT session_id FROM indexed_sessions) ORDER BY updated_at DESC LIMIT ?1")?;
            let rows = statement.query_map([MIGRATION_BATCH], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut sessions = Vec::new();
            for row in rows {
                let (id, body) = row?;
                if body.len() > super::MAX_SESSION_BYTES {
                    return Err(StorageError::Invalid(
                        "A saved conversation exceeds the migration limit.",
                    ));
                }
                let session: Value = serde_json::from_str(&body)?;
                sessions.push((id, session));
            }
            sessions
        };

        let transaction = self.connection.unchecked_transaction()?;
        for (id, session) in sessions {
            let should_index: bool = transaction.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sessions
                    WHERE id=?1 AND id NOT IN (SELECT session_id FROM indexed_sessions)
                )",
                [&id],
                |row| row.get(0),
            )?;
            if should_index {
                index(&transaction, &session)?;
            }
        }
        transaction.commit()?;
        Ok(())
    }
    pub fn recall_enabled(&self, client: MemoryClient) -> Result<bool, StorageError> {
        let keys: &[&str] = match client {
            MemoryClient::Guest => return Ok(false),
            MemoryClient::Desktop => &["preferences"],
            MemoryClient::Cli => &["cli-settings", "preferences"],
        };
        for key in keys {
            if let Some(body) = self.setting(key)? {
                let value: Value = serde_json::from_str(&body)?;
                return Ok(value["recallEnabled"].as_bool() == Some(true));
            }
        }
        Ok(false)
    }
    /// Manual owner search is available even when the model recall tool is disabled.
    pub fn search_messages(
        &self,
        client: MemoryClient,
        query: &RecallQuery,
    ) -> Result<RecallResults, StorageError> {
        if matches!(client, MemoryClient::Guest) {
            return Err(StorageError::Invalid(
                "Conversation recall is unavailable to guests.",
            ));
        }
        if query.query.trim().is_empty()
            || query.query.len() > 512
            || query.project.as_ref().is_some_and(|p| p.len() > 2048)
            || query.after.is_some_and(|n| n < 0)
            || query.before.is_some_and(|n| n < 0)
            || matches!((query.after, query.before), (Some(a),Some(b)) if a > b)
            || query.limit.is_some_and(|n| !(1..=10).contains(&n))
        {
            return Err(StorageError::Invalid(
                "Use a search of 1–512 bytes, valid date range, and a limit of 1–10.",
            ));
        }
        // Literal words, joined with AND. Never expose the FTS query language to transcripts or callers.
        let words: Vec<_> = query
            .query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .take(32)
            .map(|s| format!("\"{s}\""))
            .collect();
        if words.is_empty() {
            return Err(StorageError::Invalid("Enter at least one word to search."));
        }
        let mut statement = self.connection.prepare("SELECT s.id,m.message_id,m.message_index,s.title,
            i.mode,i.project,s.updated_at,m.role,snippet(message_search,0,'','',' … ',32)
            FROM message_search JOIN session_messages m ON m.id=message_search.rowid JOIN sessions s ON s.id=m.session_id JOIN indexed_sessions i ON i.session_id=s.id
            WHERE message_search MATCH ?1 AND (?2 IS NULL OR i.project=?2)
            AND (?3 IS NULL OR s.updated_at>=?3) AND (?4 IS NULL OR s.updated_at<=?4)
            ORDER BY bm25(message_search),s.updated_at DESC,m.message_index LIMIT ?5")?;
        let rows = statement.query_map(
            params![
                words.join(" AND "),
                query.project,
                query.after,
                query.before,
                query.limit.unwrap_or(5) as i64
            ],
            |row| {
                let excerpt: String = row.get(8)?;
                let title: String = row.get(3)?;
                Ok(RecallHit {
                    session_id: row.get(0)?,
                    message_id: row.get(1)?,
                    message_index: row.get(2)?,
                    title: title.chars().take(120).collect(),
                    mode: row.get(4)?,
                    project: row.get(5)?,
                    updated_at: row.get(6)?,
                    role: row.get(7)?,
                    excerpt: excerpt.chars().take(600).collect(),
                })
            },
        )?;
        let mut results = Vec::new();
        let mut bytes = 0;
        for row in rows {
            let hit = row?;
            bytes += serde_json::to_vec(&hit)?.len();
            if bytes > 14_000 {
                break;
            }
            results.push(hit);
        }
        let remaining_sessions = self.connection.query_row("SELECT COUNT(*) FROM sessions WHERE id NOT IN (SELECT session_id FROM indexed_sessions)", [], |row| row.get(0))?;
        Ok(RecallResults {
            results,
            remaining_sessions,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;
    fn store() -> LocalStore {
        LocalStore::initialize(Connection::open_in_memory().unwrap()).unwrap()
    }
    fn session(id: &str, text: &str, project: &str, date: i64) -> Value {
        json!({"id":id,"title":"Saved conversation","updatedAt":date,"workspace":project,"mode":"agent","messages":[
            {"id":"user_one","role":"user","content":text,"attachments":[{"textContent":"attachmentsecret"}]},
            {"id":"assistant_one","role":"assistant","content":"historical response","attachments":[],
                "tools":[{"name":"exec","arguments":"toolsecret","status":"completed"}],"approval":{"detail":"approvalsecret"}}
        ]})
    }
    fn search(db: &LocalStore, text: &str) -> RecallResults {
        db.search_messages(
            MemoryClient::Cli,
            &RecallQuery {
                query: text.into(),
                ..Default::default()
            },
        )
        .unwrap()
    }
    #[test]
    fn indexes_both_adapters_and_updates_edits_deletes_atomically() {
        let db = store();
        let mut cli = session("cli_one", "needle original", "/project", 100);
        let mut desktop = session("desktop", "needle desktop", "", 200);
        desktop["mode"] = json!("chat");
        db.save_session(&cli).unwrap();
        db.save_session(&desktop).unwrap();
        let hits = search(&db, "needle");
        assert_eq!(hits.results.len(), 2);
        assert!(hits
            .results
            .iter()
            .any(|h| h.mode == "chat" && h.message_id == "user_one"));
        assert!(hits
            .results
            .iter()
            .any(|h| h.mode == "agent" && h.message_index == 0));
        for hidden in ["attachmentsecret", "toolsecret", "approvalsecret"] {
            assert!(search(&db, hidden).results.is_empty());
        }
        cli["messages"][0]["content"] = json!("replacement needle");
        db.save_session(&cli).unwrap();
        assert!(search(&db, "original").results.is_empty());
        assert_eq!(search(&db, "replacement").results.len(), 1);
        cli["messages"] = json!([]);
        db.save_session(&cli).unwrap();
        assert!(search(&db, "replacement").results.is_empty());
        db.delete_session("desktop").unwrap();
        assert!(search(&db, "needle").results.is_empty());
        let count: i64 = db
            .connection
            .query_row("SELECT COUNT(*) FROM message_search", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
    #[test]
    fn filters_dates_and_projects_and_treats_query_as_literal_words() {
        let db = store();
        for (id, project, date) in [
            ("a", "/first", 100),
            ("b", "/first", 200),
            ("c", "/second", 200),
        ] {
            db.save_session(&session(id, "saved needle", project, date))
                .unwrap();
        }
        let mut query = RecallQuery {
            query: "saved needle".into(),
            project: Some("/first".into()),
            after: Some(200),
            before: Some(200),
            limit: Some(1),
        };
        assert_eq!(
            db.search_messages(MemoryClient::Desktop, &query)
                .unwrap()
                .results[0]
                .session_id,
            "b"
        );
        query.project = Some("' OR 1=1 --".into());
        assert!(db
            .search_messages(MemoryClient::Desktop, &query)
            .unwrap()
            .results
            .is_empty());
        assert!(search(&db, "needle OR absent").results.is_empty());
        query.after = Some(201);
        assert!(db.search_messages(MemoryClient::Cli, &query).is_err());
        query.after = Some(200);
        query.limit = Some(11);
        assert!(db.search_messages(MemoryClient::Cli, &query).is_err());
    }
    #[test]
    fn migration_is_bounded_repeatable_and_preserves_history() {
        let db = store();
        for i in 0..12 {
            let value = session(&format!("old_{i}"), "migration needle", "/project", 100);
            db.connection
                .execute(
                    "INSERT INTO sessions(id,title,updated_at,body) VALUES (?1,'Old',100,?2)",
                    params![value["id"].as_str(), value.to_string()],
                )
                .unwrap();
        }
        db.connection
            .pragma_update(None, "user_version", 3)
            .unwrap();
        let db = LocalStore::initialize(db.connection).unwrap();
        // The schema-migration open releases its writer lock without starting the backfill.
        assert_eq!(search(&db, "needle").remaining_sessions, 12);
        db.delete_session("old_9").unwrap(); // Deletion before backfill must never resurrect text.
        let db = LocalStore::initialize(db.connection).unwrap();
        assert_eq!(search(&db, "needle").remaining_sessions, 6);
        let db = LocalStore::initialize(db.connection).unwrap();
        assert_eq!(search(&db, "needle").remaining_sessions, 1);
        let db = LocalStore::initialize(db.connection).unwrap();
        assert_eq!(search(&db, "needle").remaining_sessions, 0);
        assert!(db.load_session("old_9").unwrap().is_none());
        let before: i64 = db
            .connection
            .query_row("SELECT COUNT(*) FROM session_messages", [], |r| r.get(0))
            .unwrap();
        let db = LocalStore::initialize(db.connection).unwrap();
        let after: i64 = db
            .connection
            .query_row("SELECT COUNT(*) FROM session_messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!((before, after), (22, 22));
        assert_eq!(
            db.load_session("old_0").unwrap().unwrap()["messages"][1]["tools"][0]["arguments"],
            "toolsecret"
        );
    }
    #[test]
    fn legacy_import_and_missing_message_ids_are_searchable() {
        let mut db = store();
        let mut old = session("cli_old", "legacy needle", "/old", 100);
        old["messages"][0].as_object_mut().unwrap().remove("id");
        old.as_object_mut().unwrap().remove("mode");
        db.migrate_sessions(&[old.clone()]).unwrap();
        let hit = search(&db, "needle").results.remove(0);
        assert_eq!(hit.message_id, "message_0");
        assert_eq!(hit.mode, "agent");
        assert_eq!(db.load_session("cli_old").unwrap(), Some(old));
    }
    #[test]
    fn bounds_unicode_output_and_guest_access_and_agent_opt_in() {
        let db = store();
        for i in 0..20 {
            db.save_session(&session(
                &format!("s_{i}"),
                &format!("needle {}", "é🦀".repeat(20000)),
                "/project",
                100,
            ))
            .unwrap();
        }
        let query = RecallQuery {
            query: "needle".into(),
            limit: Some(10),
            ..Default::default()
        };
        let result = db.search_messages(MemoryClient::Desktop, &query).unwrap();
        assert!(result.results.len() <= 10);
        assert!(result
            .results
            .iter()
            .all(|r| r.excerpt.chars().count() <= 600));
        assert!(serde_json::to_vec(&result).unwrap().len() <= 16000);
        assert!(db.search_messages(MemoryClient::Guest, &query).is_err());
        assert!(!db.recall_enabled(MemoryClient::Cli).unwrap());
        db.save_setting("preferences", r#"{"recallEnabled":true}"#)
            .unwrap();
        assert!(db.recall_enabled(MemoryClient::Desktop).unwrap());
        assert!(db.recall_enabled(MemoryClient::Cli).unwrap());
        assert!(!db.recall_enabled(MemoryClient::Guest).unwrap());
        db.save_setting("cli-settings", r#"{"recallEnabled":false}"#)
            .unwrap();
        assert!(!db.recall_enabled(MemoryClient::Cli).unwrap());
    }
    #[test]
    fn text_limits_do_not_split_unicode_or_index_later_overflow() {
        let db = store();
        let text = format!("{} overflowword", "é🦀".repeat(12000));
        let mut value = session("large", &text, "/project", 100);
        let message = value["messages"][0].clone();
        value["messages"] = Value::Array(vec![message; 10]);
        db.save_session(&value).unwrap();
        assert!(search(&db, "overflowword").results.is_empty());
        let bytes: i64 = db
            .connection
            .query_row(
                "SELECT SUM(length(CAST(content AS BLOB))) FROM session_messages",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(bytes <= SESSION_TEXT_BYTES as i64);
        let texts: Vec<String> = db
            .connection
            .prepare("SELECT content FROM session_messages")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(texts.iter().all(|text| text.len() <= MESSAGE_TEXT_BYTES));
    }
    #[test]
    fn rejects_invalid_save_without_removing_existing_excerpts() {
        let db = store();
        let mut value = session("a", "needle", "/project", 100);
        db.save_session(&value).unwrap();
        value["messages"][0]["role"] = json!("tool");
        assert!(db.save_session(&value).is_err());
        assert_eq!(search(&db, "needle").results.len(), 1);
    }
}
