//! Search only saved message text. Tool calls, attachments and approval records stay inert.
use super::{LocalStore, StorageError};
use crate::memory::MemoryClient;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(super) const MESSAGE_TEXT_BYTES: usize = 64 * 1024;

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

/// The `recall` rules both stores share: guests are refused, the query is checked, and its words
/// become a quoted `AND` expression. The FTS query language is never exposed to callers or
/// transcripts.
pub(super) fn match_expression(
    client: MemoryClient,
    query: &RecallQuery,
) -> Result<String, StorageError> {
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
    // Literal words, joined with AND.
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
    Ok(words.join(" AND "))
}

/// Keeps hits, in order, until their JSON would pass 14,000 bytes.
pub(super) fn cap_results(
    hits: impl IntoIterator<Item = rusqlite::Result<RecallHit>>,
) -> Result<Vec<RecallHit>, StorageError> {
    let mut results = Vec::new();
    let mut bytes = 0;
    for hit in hits {
        let hit = hit?;
        bytes += serde_json::to_vec(&hit)?.len();
        if bytes > 14_000 {
            break;
        }
        results.push(hit);
    }
    Ok(results)
}

impl LocalStore {
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
        self.search_chats(client, query)
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
            .query_row("SELECT COUNT(*) FROM chat_search", [], |r| r.get(0))
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
    fn upgrade_indexes_every_old_chat_in_one_open_and_preserves_history() {
        let chats: Vec<_> = (0..12)
            .map(|i| {
                let text = format!("migration needle word{i}");
                session(&format!("old_{i}"), &text, "/project", 100)
            })
            .collect();
        let directory = super::super::upgrade::tests::v5_folder("v3", &chats);
        Connection::open(directory.join(super::super::DATABASE_FILE))
            .unwrap()
            .pragma_update(None, "user_version", 3)
            .unwrap();
        let db = LocalStore::open(&directory).unwrap();
        // One open converts and indexes every old chat: nothing is left for a later open.
        assert_eq!(search(&db, "needle").remaining_sessions, 0);
        for i in 0..12 {
            let hits = search(&db, &format!("word{i}")).results;
            assert_eq!(hits.len(), 1, "old_{i}");
            assert_eq!(hits[0].session_id, format!("old_{i}"));
        }
        db.delete_session("old_9").unwrap(); // Deletion must never resurrect text.
        assert!(search(&db, "word9").results.is_empty());
        assert!(db.load_session("old_9").unwrap().is_none());
        let indexed = |db: &LocalStore| -> i64 {
            db.connection
                .query_row("SELECT COUNT(*) FROM chat_search", [], |r| r.get(0))
                .unwrap()
        };
        let before = indexed(&db);
        drop(db);
        let db = LocalStore::open(&directory).unwrap();
        assert_eq!((before, indexed(&db)), (22, 22));
        assert_eq!(
            db.load_session("old_0").unwrap().unwrap()["messages"][1]["tools"][0]["arguments"],
            "toolsecret"
        );
        drop(db);
        std::fs::remove_dir_all(directory).unwrap();
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
        // `word` ends 6 bytes before 64 KiB, the next character crosses it, and a word follows.
        let text = |word: &str| format!("{} {word} 🦀🦀 overflowword", "é🦀".repeat(10920));
        let edge = text("edgeword");
        assert_eq!(MESSAGE_TEXT_BYTES, 64 * 1024);
        assert_eq!(edge.find("edgeword").unwrap() + "edgeword".len(), 65_529);
        assert!(edge.find(" overflowword").unwrap() > MESSAGE_TEXT_BYTES);
        let mut value = session("large", &edge, "/project", 100);
        let message = value["messages"][0].clone();
        let mut messages = vec![message.clone(); 9];
        // The tenth message lies past 512 KiB of the chat's text: each message has its own cap.
        let mut last = message;
        last["content"] = json!(text("lastword"));
        messages.push(last);
        value["messages"] = Value::Array(messages);
        db.save_session(&value).unwrap();
        assert!(search(&db, "overflowword").results.is_empty());
        assert!(!search(&db, "edgeword").results.is_empty());
        let last = search(&db, "lastword").results;
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].message_index, 9);
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
