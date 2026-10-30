//! Per-message search for the v6 store: a contentless FTS5 index and snippets made at query time.
//!
//! The index holds terms only. A hit's text is rebuilt from the chat's events, so a message is
//! never stored a second time. Dormant: `LocalStore::open` still opens v5 and v5 recall is as it was.
#![cfg_attr(not(test), allow(dead_code))] // the upgrade and the run recorder that call it land later

use super::chats::{stmt, Counters};
use super::recall::{
    cap_results, match_expression, RecallHit, RecallQuery, RecallResults, MESSAGE_TEXT_BYTES,
    SESSION_TEXT_BYTES,
};
use super::{LocalStore, StorageError};
use crate::memory::MemoryClient;
use rusqlite::{params, Connection};
use serde_json::Value;
use std::collections::HashMap;

/// How many bytes of `text` the index keeps: at most 64 KiB, and at most what is left of the
/// session's 512 KiB after `consumed` bytes, cut at a character boundary.
fn indexed_len(text: &str, consumed: u64) -> usize {
    let room = SESSION_TEXT_BYTES.saturating_sub(usize::try_from(consumed).unwrap_or(usize::MAX));
    let mut end = text.len().min(MESSAGE_TEXT_BYTES).min(room);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// What the index holds for each UI message: its position and its clipped text. Only a user or
/// assistant message with text counts, and each one uses up part of the session's allowance.
/// `skip` is a message that is not indexed yet (a run still open).
pub(super) fn indexed_texts(messages: &[Value], skip: Option<u64>) -> Vec<(usize, &str)> {
    let mut consumed = 0u64;
    let mut texts = vec![];
    for (position, message) in messages.iter().enumerate() {
        if skip == Some(position as u64)
            || !matches!(message["role"].as_str(), Some("user" | "assistant"))
        {
            continue;
        }
        let text = message["content"].as_str().unwrap_or("");
        let end = indexed_len(text, consumed);
        if end == 0 {
            continue;
        }
        consumed += end as u64;
        texts.push((position, &text[..end]));
    }
    texts
}

/// What `search_bytes` should say for these messages.
pub(super) fn derived_bytes(messages: &[Value], skip: Option<u64>) -> u64 {
    indexed_texts(messages, skip)
        .iter()
        .map(|(_, text)| text.len() as u64)
        .sum()
}

/// Adds one UI message to the search index, inside the caller's write transaction. `seq` is the
/// event that holds it and `message_index` its position among the chat's UI messages. A message
/// with no text, or past the session's allowance, gets no entry.
pub(crate) fn index_message(
    tx: &Connection,
    id: &str,
    c: &mut Counters,
    seq: i64,
    message_index: u64,
    message: &Value,
) -> Result<(), StorageError> {
    let role = message["role"].as_str().unwrap_or("");
    if !matches!(role, "user" | "assistant") {
        return Ok(());
    }
    let text = message["content"].as_str().unwrap_or("");
    let end = indexed_len(text, c.search_bytes);
    if end == 0 {
        return Ok(());
    }
    let fallback = format!("message_{message_index}");
    let message_id = message["id"]
        .as_str()
        .filter(|id| id.len() <= 128)
        .unwrap_or(&fallback);
    stmt(
        tx,
        "INSERT INTO chat_search_keys(session_id,seq,message_index,message_id,role) VALUES (?1,?2,?3,?4,?5)",
    )?
    .execute(params![
        id,
        seq,
        i64::try_from(message_index).unwrap_or(i64::MAX),
        message_id,
        role
    ])?;
    let key = tx.last_insert_rowid();
    stmt(tx, "INSERT INTO chat_search(rowid,content) VALUES (?1,?2)")?
        .execute(params![key, &text[..end]])?;
    c.search_bytes += end as u64;
    Ok(())
}

/// The excerpt for each text, made with the same FTS5 `snippet()` call v5 ran against its stored
/// copy. The texts go into a temporary table that lives only for this call.
fn snippets(
    connection: &Connection,
    hits: &[(i64, String)],
    expression: &str,
) -> Result<HashMap<i64, String>, StorageError> {
    connection.execute_batch(
        "DROP TABLE IF EXISTS temp.snippet_text;
         CREATE VIRTUAL TABLE temp.snippet_text USING fts5(content);",
    )?;
    let made = (|| -> Result<HashMap<i64, String>, StorageError> {
        let mut insert =
            connection.prepare("INSERT INTO temp.snippet_text(rowid,content) VALUES (?1,?2)")?;
        for (key, text) in hits {
            insert.execute(params![key, text])?;
        }
        let mut select = connection.prepare(
            "SELECT rowid, snippet(snippet_text,0,'','',' … ',32) FROM temp.snippet_text WHERE snippet_text MATCH ?1",
        )?;
        let rows = select.query_map([expression], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    })();
    connection.execute_batch("DROP TABLE IF EXISTS temp.snippet_text")?;
    made
}

/// Today's recall over the v6 index. Filters, ranking, caps and the 14,000-byte limit are v5's.
pub(crate) fn search_v6(
    store: &LocalStore,
    client: MemoryClient,
    query: &RecallQuery,
) -> Result<RecallResults, StorageError> {
    let expression = match_expression(client, query)?;
    struct Row {
        key: i64,
        session_id: String,
        message_id: String,
        message_index: u32,
        title: String,
        mode: String,
        project: Option<String>,
        updated_at: i64,
        role: String,
    }
    let rows: Vec<Row> = {
        let mut statement = stmt(
            store.connection(),
            "SELECT k.id, s.id, k.message_id, k.message_index, s.title,
                CASE WHEN s.mode='agent' THEN 'agent'
                     WHEN s.mode IS NULL AND substr(s.id,1,4)='cli_' THEN 'agent'
                     ELSE 'chat' END,
                s.workspace, s.updated_at, k.role
             FROM chat_search
             JOIN chat_search_keys k ON k.id = chat_search.rowid
             JOIN sessions s ON s.id = k.session_id
             WHERE chat_search MATCH ?1 AND (?2 IS NULL OR s.workspace = ?2)
             AND (?3 IS NULL OR s.updated_at >= ?3) AND (?4 IS NULL OR s.updated_at <= ?4)
             ORDER BY bm25(chat_search), s.updated_at DESC, k.message_index LIMIT ?5",
        )?;
        let rows = statement.query_map(
            params![
                expression,
                query.project,
                query.after,
                query.before,
                query.limit.unwrap_or(5) as i64
            ],
            |row| {
                Ok(Row {
                    key: row.get(0)?,
                    session_id: row.get(1)?,
                    message_id: row.get(2)?,
                    message_index: row.get(3)?,
                    title: row.get(4)?,
                    mode: row.get(5)?,
                    project: row.get(6)?,
                    updated_at: row.get(7)?,
                    role: row.get(8)?,
                })
            },
        )?;
        rows.collect::<Result<_, _>>()?
    };
    // Only the matching messages are rebuilt, each chat once.
    let mut chats: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    let mut texts = vec![];
    for row in &rows {
        if !chats.contains_key(&row.session_id) {
            // A chat that cannot be rebuilt loses its own hits, not everyone's.
            let indexed = match store.load_for_search(&row.session_id) {
                Ok(Some((messages, open))) => indexed_texts(&messages, open)
                    .into_iter()
                    .map(|(position, text)| (position, text.to_owned()))
                    .collect(),
                Ok(None) => vec![],
                Err(error) => {
                    eprintln!("warn: recall skipped chat {}: {error}", row.session_id);
                    vec![]
                }
            };
            chats.insert(row.session_id.clone(), indexed);
        }
        let text = chats[&row.session_id]
            .iter()
            .find(|(position, _)| *position == row.message_index as usize)
            .map(|(_, text)| text.clone());
        if let Some(text) = text {
            texts.push((row.key, text));
        }
    }
    if texts.is_empty() {
        return Ok(RecallResults {
            results: vec![],
            remaining_sessions: 0,
        });
    }
    let mut excerpts = snippets(store.connection(), &texts, &expression)?;
    let hits = rows.into_iter().filter_map(|row| {
        let excerpt = excerpts.remove(&row.key)?;
        Some(Ok(RecallHit {
            session_id: row.session_id,
            message_id: row.message_id,
            message_index: row.message_index,
            title: row.title.chars().take(120).collect(),
            mode: row.mode,
            project: row.project,
            updated_at: row.updated_at,
            role: row.role,
            excerpt: excerpt.chars().take(600).collect(),
        }))
    });
    Ok(RecallResults {
        results: cap_results(hits)?,
        remaining_sessions: 0,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::storage::chats::{
        append_model, append_run_start, append_ui_assistant, append_ui_user, read_counters,
        recount, v6_in_memory, write_counters, write_tx, NewSession,
    };
    use crate::storage::fixtures::{abs, golden, platform_neutral, v5_sessions, RECALL_QUERIES};
    use serde_json::{json, Map};

    /// Stores a chat as the writers will: one UI message per event, each indexed in the same
    /// transaction. Messages are user or assistant, in order, so a message's position is its
    /// UI index.
    fn store(
        db: &LocalStore,
        id: &str,
        title: &str,
        mode: Option<&str>,
        workspace: Option<&str>,
        updated_at: i64,
        messages: &[Value],
    ) {
        db.create_session(&NewSession {
            id: id.into(),
            title: title.into(),
            mode: mode.map(Into::into),
            // Written POSIX-style; stored absolute for this platform, as v6 requires.
            workspace: workspace.map(abs),
            updated_at,
        })
        .unwrap();
        let tx = write_tx(db.connection()).unwrap();
        let mut c = read_counters(&tx, id).unwrap();
        for (position, message) in messages.iter().enumerate() {
            let seq = match message["role"].as_str() {
                Some("user") => append_ui_user(&tx, id, &mut c, message).unwrap(),
                Some("assistant") => append_ui_assistant(&tx, id, &mut c, message).unwrap(),
                other => panic!("a {other:?} message has no UI index"),
            };
            index_message(&tx, id, &mut c, seq, position as u64, message).unwrap();
        }
        write_counters(&tx, id, &c, updated_at).unwrap();
        tx.commit().unwrap();
    }

    /// The `recall.rs` test session: hidden text in attachments, tools and approvals.
    fn session(id: &str, text: &str, project: &str, date: i64) -> (Vec<Value>, i64, String) {
        let messages = vec![
            json!({"id":"user_one","role":"user","content":text,"attachments":[{"textContent":"attachmentsecret"}]}),
            json!({"id":"assistant_one","role":"assistant","content":"historical response","attachments":[],
                "tools":[{"name":"exec","arguments":"toolsecret","status":"completed"}],"approval":{"detail":"approvalsecret"}}),
        ];
        (messages, date, project.to_owned() + id)
    }
    fn put(db: &LocalStore, id: &str, text: &str, project: &str, date: i64, mode: Option<&str>) {
        let (messages, _, _) = session(id, text, project, date);
        store(
            db,
            id,
            "Saved conversation",
            mode,
            Some(project),
            date,
            &messages,
        );
    }
    fn find(db: &LocalStore, text: &str) -> RecallResults {
        search_v6(
            db,
            MemoryClient::Cli,
            &RecallQuery {
                query: text.into(),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn v6_search_indexes_ui_content_only() {
        let db = v6_in_memory();
        put(
            &db,
            "cli_one",
            "needle original",
            "/project",
            100,
            Some("agent"),
        );
        put(
            &db,
            "desktop",
            "needle desktop",
            "/other",
            200,
            Some("chat"),
        );
        let hits = find(&db, "needle");
        assert_eq!(hits.results.len(), 2);
        assert_eq!(hits.remaining_sessions, 0);
        assert!(hits
            .results
            .iter()
            .any(|h| h.mode == "chat" && h.message_id == "user_one" && h.role == "user"));
        assert!(hits
            .results
            .iter()
            .any(|h| h.mode == "agent" && h.message_index == 0));
        for hidden in ["attachmentsecret", "toolsecret", "approvalsecret"] {
            assert!(find(&db, hidden).results.is_empty(), "{hidden}");
        }
        // The assistant's hit carries its UI id and index.
        let mut reply = search_v6(
            &db,
            MemoryClient::Cli,
            &RecallQuery {
                query: "historical".into(),
                project: Some(abs("/project")),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(reply.results.len(), 1);
        let reply = reply.results.remove(0);
        assert_eq!(
            (
                reply.message_id.as_str(),
                reply.message_index,
                reply.role.as_str()
            ),
            ("assistant_one", 1, "assistant")
        );
        assert_eq!(reply.excerpt, "historical response");
        assert_eq!(reply.title, "Saved conversation");
        assert_eq!(reply.project, Some(abs("/project")));
        // Deleting a chat takes its hits with it.
        db.delete_v6("desktop").unwrap();
        assert_eq!(find(&db, "needle").results.len(), 1);
    }

    #[test]
    fn filters_dates_and_projects_and_treats_query_as_literal_words() {
        let db = v6_in_memory();
        for (id, project, date) in [
            ("a", "/first", 100),
            ("b", "/first", 200),
            ("c", "/second", 200),
        ] {
            put(&db, id, "saved needle", project, date, Some("agent"));
        }
        let mut query = RecallQuery {
            query: "saved needle".into(),
            project: Some(abs("/first")),
            after: Some(200),
            before: Some(200),
            limit: Some(1),
        };
        assert_eq!(
            search_v6(&db, MemoryClient::Desktop, &query)
                .unwrap()
                .results[0]
                .session_id,
            "b"
        );
        query.project = Some("' OR 1=1 --".into());
        assert!(search_v6(&db, MemoryClient::Desktop, &query)
            .unwrap()
            .results
            .is_empty());
        assert!(find(&db, "needle OR absent").results.is_empty());
        query.after = Some(201);
        assert!(search_v6(&db, MemoryClient::Cli, &query).is_err());
        query.after = Some(200);
        query.limit = Some(11);
        assert!(search_v6(&db, MemoryClient::Cli, &query).is_err());
        assert!(search_v6(&db, MemoryClient::Guest, &query).is_err());
    }

    #[test]
    fn mode_falls_back_as_v5_did() {
        let db = v6_in_memory();
        put(&db, "cli_old", "legacy needle", "/old", 100, None);
        put(&db, "desktop_old", "legacy needle", "/old", 100, None);
        put(&db, "other", "legacy needle", "/old", 100, Some("weird"));
        let modes: Vec<(String, String)> = find(&db, "legacy")
            .results
            .into_iter()
            .map(|h| (h.session_id, h.mode))
            .collect();
        for (id, mode) in [
            ("cli_old", "agent"),
            ("desktop_old", "chat"),
            ("other", "chat"),
        ] {
            assert!(modes.contains(&(id.into(), mode.into())), "{id} {modes:?}");
        }
    }

    #[test]
    fn missing_ids_and_empty_text_are_handled_as_v5_did() {
        let db = v6_in_memory();
        let long = "x".repeat(129);
        store(
            &db,
            "s",
            "T",
            None,
            None,
            1,
            &[
                json!({"role":"user","content":"first needle","attachments":[]}),
                json!({"id":long,"role":"assistant","content":"second needle"}),
                json!({"id":"empty","role":"user","content":"","attachments":[]}),
                json!({"id":"fine","role":"assistant","content":"third needle"}),
            ],
        );
        let mut ids: Vec<(u32, String)> = find(&db, "needle")
            .results
            .into_iter()
            .map(|h| (h.message_index, h.message_id))
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            [
                (0, "message_0".to_owned()),
                (1, "message_1".to_owned()),
                (3, "fine".to_owned())
            ]
        );
        // The empty message has no entry at all.
        let keys: i64 = db
            .connection()
            .query_row("SELECT count(*) FROM chat_search_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(keys, 3);
    }

    #[test]
    fn v6_search_text_limits_match_v5() {
        let db = v6_in_memory();
        let text = format!("{} overflowword", "é🦀".repeat(12000));
        let message = json!({"id":"user_one","role":"user","content":text,"attachments":[]});
        store(&db, "large", "T", None, None, 1, &vec![message; 10]);
        assert!(find(&db, "overflowword").results.is_empty());
        // What the index holds per message, rebuilt the way a search rebuilds it.
        let chat = db.load_chat("large").unwrap().unwrap();
        let held = indexed_texts(&chat.messages, None);
        let bytes: usize = held.iter().map(|(_, t)| t.len()).sum();
        assert!(bytes <= SESSION_TEXT_BYTES, "{bytes}");
        assert!(held.iter().all(|(_, t)| t.len() <= MESSAGE_TEXT_BYTES));
        // Eight messages of 65,534 bytes (a cut never splits a character), then what is left.
        assert_eq!(bytes, SESSION_TEXT_BYTES);
        assert_eq!(held.len(), 10);
        assert_eq!(held[8].1.len(), 14);
        assert_eq!(held[9].1, "é");
        // The stored counter says the same, and no cut splits a character.
        let c = read_counters(db.connection(), "large").unwrap();
        assert_eq!(c.search_bytes, bytes as u64);
        assert!(held
            .iter()
            .all(|(_, t)| std::str::from_utf8(t.as_bytes()).is_ok()));
        let tx = write_tx(db.connection()).unwrap();
        assert_eq!(recount(&tx, "large").unwrap(), c);
    }

    #[test]
    fn bounds_unicode_output() {
        let db = v6_in_memory();
        for i in 0..20 {
            put(
                &db,
                &format!("s_{i}"),
                &format!("needle {}", "é🦀".repeat(20000)),
                "/project",
                100,
                Some("agent"),
            );
        }
        let query = RecallQuery {
            query: "needle".into(),
            limit: Some(10),
            ..Default::default()
        };
        let result = search_v6(&db, MemoryClient::Desktop, &query).unwrap();
        assert!(!result.results.is_empty() && result.results.len() <= 10);
        assert!(result
            .results
            .iter()
            .all(|r| r.excerpt.chars().count() <= 600));
        assert!(serde_json::to_vec(&result).unwrap().len() <= 16000);
    }

    #[test]
    fn index_message_clips_to_the_message_and_session_allowance() {
        let db = v6_in_memory();
        db.create_session(&NewSession {
            id: "s".into(),
            title: "T".into(),
            mode: None,
            workspace: None,
            updated_at: 1,
        })
        .unwrap();
        let tx = write_tx(db.connection()).unwrap();
        let mut c = read_counters(&tx, "s").unwrap();
        let big = json!({"id":"m","role":"user","content":"a".repeat(100_000)});
        index_message(&tx, "s", &mut c, 0, 0, &big).unwrap();
        assert_eq!(c.search_bytes, MESSAGE_TEXT_BYTES as u64);
        // Only 3 bytes of the session's allowance left: a 4-byte character does not fit.
        c.search_bytes = SESSION_TEXT_BYTES as u64 - 3;
        let before = c.clone();
        index_message(
            &tx,
            "s",
            &mut c,
            1,
            1,
            &json!({"id":"m2","role":"user","content":"🦀"}),
        )
        .unwrap();
        assert_eq!(c, before);
        index_message(
            &tx,
            "s",
            &mut c,
            2,
            2,
            &json!({"id":"m3","role":"user","content":"abcdef"}),
        )
        .unwrap();
        assert_eq!(c.search_bytes, SESSION_TEXT_BYTES as u64);
        // Spent: nothing more is indexed and nothing is refused.
        index_message(
            &tx,
            "s",
            &mut c,
            3,
            3,
            &json!({"id":"m4","role":"user","content":"more"}),
        )
        .unwrap();
        assert_eq!(c.search_bytes, SESSION_TEXT_BYTES as u64);
        // A role that v5 never indexed, and a missing content, add nothing.
        index_message(
            &tx,
            "s",
            &mut c,
            4,
            4,
            &json!({"id":"m5","role":"tool","content":"x"}),
        )
        .unwrap();
        index_message(&tx, "s", &mut c, 5, 5, &json!({"id":"m6","role":"user"})).unwrap();
        let keys: i64 = tx
            .query_row("SELECT count(*) FROM chat_search_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(keys, 2);
    }

    #[test]
    fn the_index_holds_terms_only() {
        let db = v6_in_memory();
        put(&db, "s", "zqxjstoredterm", "/p", 1, Some("chat"));
        let tables: Vec<String> = {
            let mut statement = db
                .connection()
                .prepare("SELECT name FROM sqlite_master WHERE name LIKE 'chat_search%'")
                .unwrap();
            let rows = statement.query_map([], |r| r.get(0)).unwrap();
            rows.collect::<Result<_, _>>().unwrap()
        };
        assert!(
            !tables.iter().any(|t| t == "chat_search_content"),
            "{tables:?}"
        );
        // Reading the indexed column back gives nothing: there is no text to give.
        let stored: Option<String> = db
            .connection()
            .query_row("SELECT content FROM chat_search LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, None);
        // The temporary table a search used is gone.
        find(&db, "zqxjstoredterm");
        let leftover: i64 = db
            .connection()
            .query_row(
                "SELECT count(*) FROM sqlite_temp_master WHERE name LIKE 'snippet_text%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leftover, 0);
    }

    #[test]
    fn query_time_snippets_equal_stored_content_snippets() {
        let connection = Connection::open_in_memory().unwrap();
        let texts = [
            "The quick brown fox jumps over the lazy dog near the river bank while the sun sets slowly behind the distant hills and the evening air grows cool and quiet.".to_owned(),
            "Rust ownership rules say each value has one owner; borrowing lets code read a value without taking it, and the borrow checker enforces these rules at compile time.".to_owned(),
            format!("{} needle in the middle {}", "alpha beta gamma ".repeat(40), "delta epsilon ".repeat(40)),
            "Ünïcödé tëxt with émojis 🦀 and the word crab appearing next to cafe latte and naïve résumé.".to_owned(),
            "short".to_owned(),
            "needle needle needle haystack haystack".to_owned(),
            "Checkpoint summaries keep long chats small: the checkpoint covers earlier messages.".to_owned(),
            format!("{} tail target", "word ".repeat(300)),
        ];
        connection
            .execute_batch("CREATE VIRTUAL TABLE reference USING fts5(content)")
            .unwrap();
        let mut hits = vec![];
        for (i, text) in texts.iter().enumerate() {
            let key = 10 + i as i64;
            connection
                .execute(
                    "INSERT INTO reference(rowid,content) VALUES (?1,?2)",
                    params![key, text],
                )
                .unwrap();
            hits.push((key, text.clone()));
        }
        let queries = [
            "fox",
            "river",
            "quick brown",
            "ownership",
            "borrow",
            "needle",
            "middle",
            "alpha",
            "delta epsilon",
            "crab",
            "cafe",
            "résumé",
            "short",
            "haystack",
            "checkpoint",
            "checkpoint messages",
            "word",
            "target",
            "tail target",
            "the",
        ];
        assert_eq!(queries.len(), 20);
        for query in queries {
            let expression = match_expression(
                MemoryClient::Cli,
                &RecallQuery {
                    query: query.into(),
                    ..Default::default()
                },
            )
            .unwrap();
            let made = snippets(&connection, &hits, &expression).unwrap();
            let mut statement = connection
                .prepare("SELECT rowid, snippet(reference,0,'','',' … ',32) FROM reference WHERE reference MATCH ?1")
                .unwrap();
            let stored: HashMap<i64, String> = statement
                .query_map([&expression], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(made, stored, "query {query:?}");
        }
        // The helper table does not outlive a call, matches or not.
        let none = snippets(&connection, &hits, "\"absentword\"").unwrap();
        assert!(none.is_empty());
        assert!(
            connection
                .query_row(
                    "SELECT count(*) FROM sqlite_temp_master WHERE name LIKE 'snippet_text%'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap()
                == 0
        );
    }

    /// Every v5 fixture stored as v6 UI messages (the context is not part of recall) answers the
    /// recall queries exactly as the v5 golden says: ids, indexes, roles, excerpts, order.
    #[test]
    fn v6_recall_equals_the_v5_golden_for_every_fixture() {
        let db = v6_in_memory();
        for fixture in v5_sessions() {
            if fixture["id"] == "bad_role" {
                continue; // v5 refuses it
            }
            let messages = fixture["messages"].as_array().unwrap();
            store(
                &db,
                fixture["id"].as_str().unwrap(),
                fixture["title"].as_str().unwrap(),
                fixture["mode"].as_str(),
                fixture["workspace"].as_str(),
                fixture["updatedAt"].as_i64().unwrap(),
                messages,
            );
        }
        let mut results = Map::new();
        for query in RECALL_QUERIES {
            let hits = search_v6(
                &db,
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
        assert_eq!(
            platform_neutral(Value::Object(results)),
            golden("v5_recall.json")
        );
    }

    #[test]
    fn deleting_a_chat_removes_its_terms() {
        let db = v6_in_memory();
        put(&db, "gone", "zqxjgoneterm first", "/p", 1, Some("chat"));
        put(&db, "kept", "zqxjkeptterm second", "/p", 2, Some("chat"));
        let shadow = |db: &LocalStore| -> Vec<u8> {
            let mut bytes = vec![];
            let names: Vec<String> = {
                let mut statement = db
                    .connection()
                    .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'chat_search\\_%' ESCAPE '\\' AND name NOT LIKE 'chat_search\\_keys' ESCAPE '\\'")
                    .unwrap();
                let rows = statement.query_map([], |r| r.get(0)).unwrap();
                rows.collect::<Result<_, _>>().unwrap()
            };
            for table in names {
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
        let has =
            |hay: &[u8], needle: &str| hay.windows(needle.len()).any(|w| w == needle.as_bytes());
        assert!(has(&shadow(&db), "zqxjgoneterm"));
        assert_eq!(find(&db, "zqxjgoneterm").results.len(), 1);
        db.delete_v6("gone").unwrap();
        let after = shadow(&db);
        assert!(!has(&after, "zqxjgoneterm"), "a deleted term survived");
        assert!(has(&after, "zqxjkeptterm"));
        assert!(find(&db, "zqxjgoneterm").results.is_empty());
        assert_eq!(find(&db, "zqxjkeptterm").results.len(), 1);
    }

    #[test]
    fn search_bytes_is_derived_and_verified_by_recount_and_load() {
        let db = v6_in_memory();
        put(&db, "s", "needle one", "/p", 1, Some("chat"));
        let good = read_counters(db.connection(), "s").unwrap();
        assert_eq!(
            good.search_bytes,
            ("needle one".len() + "historical response".len()) as u64
        );
        let tx = write_tx(db.connection()).unwrap();
        assert_eq!(recount(&tx, "s").unwrap(), good);
        drop(tx);
        let before = crate::storage::chats::counter_repairs();
        db.load_chat("s").unwrap().unwrap();
        assert_eq!(crate::storage::chats::counter_repairs(), before);
        // A wrong stored allowance is repaired from the messages on the next load.
        db.connection()
            .execute("UPDATE sessions SET search_bytes=7 WHERE id='s'", [])
            .unwrap();
        db.load_chat("s").unwrap().unwrap();
        assert_eq!(crate::storage::chats::counter_repairs(), before + 1);
        assert_eq!(read_counters(db.connection(), "s").unwrap(), good);
    }

    #[test]
    fn a_run_still_open_is_not_indexed_or_counted_until_its_terminal() {
        let db = v6_in_memory();
        db.create_session(&NewSession {
            id: "s".into(),
            title: "T".into(),
            mode: None,
            workspace: None,
            updated_at: 1,
        })
        .unwrap();
        let tx = write_tx(db.connection()).unwrap();
        let mut c = read_counters(&tx, "s").unwrap();
        let user_message = json!({"id":"u","role":"user","content":"question text","attachments":[],"status":"complete","createdAt":1});
        let user = append_ui_user(&tx, "s", &mut c, &user_message).unwrap();
        index_message(&tx, "s", &mut c, user, 0, &user_message).unwrap();
        let start = append_run_start(&tx, "s", &mut c, "a", 2, user).unwrap();
        append_model(
            &tx,
            "s",
            &mut c,
            &json!({"role":"assistant","content":"partial answer"}),
        )
        .unwrap();
        write_counters(&tx, "s", &c, 1).unwrap();
        tx.commit().unwrap();
        // The folded message has text, but nothing indexes it yet and nothing counts it.
        let chat = db.load_chat("s").unwrap().unwrap();
        assert!(chat.open_run);
        assert_eq!(chat.messages[1]["content"], "partial answer");
        let mut c = read_counters(db.connection(), "s").unwrap();
        assert_eq!(c.search_bytes, "question text".len() as u64);
        let before = crate::storage::chats::counter_repairs();
        db.load_chat("s").unwrap().unwrap();
        assert_eq!(crate::storage::chats::counter_repairs(), before);
        assert!(find(&db, "partial").results.is_empty());
        // The terminal indexes it.
        let tx = write_tx(db.connection()).unwrap();
        let end = append_ui_assistant(
            &tx,
            "s",
            &mut c,
            &json!({"run": start, "content": "final answer", "status": "complete"}),
        )
        .unwrap();
        let reply = json!({"id":"a","role":"assistant","content":"final answer"});
        index_message(&tx, "s", &mut c, end, 1, &reply).unwrap();
        write_counters(&tx, "s", &c, 2).unwrap();
        tx.commit().unwrap();
        let before = crate::storage::chats::counter_repairs();
        db.load_chat("s").unwrap().unwrap();
        assert_eq!(crate::storage::chats::counter_repairs(), before);
        assert_eq!(find(&db, "final").results[0].message_id, "a");
    }

    // ---- v5 on the same data ----

    type Chat<'a> = (&'a str, Option<&'a str>, i64, Vec<Value>);

    /// The same chats saved by the real v5 `save_session`, to compare recall with.
    fn v5_store(chats: &[Chat]) -> LocalStore {
        let v5 = LocalStore::initialize(Connection::open_in_memory().unwrap()).unwrap();
        for (id, mode, updated_at, messages) in chats {
            let mut session = json!({"id": id, "title": "T", "updatedAt": updated_at,
                "workspace": abs("/p"), "messages": messages});
            if let Some(mode) = mode {
                session["mode"] = json!(mode);
            }
            v5.save_session(&session).unwrap();
        }
        v5
    }
    fn v6_store(chats: &[Chat]) -> LocalStore {
        let v6 = v6_in_memory();
        for (id, mode, updated_at, messages) in chats {
            store(&v6, id, "T", *mode, Some("/p"), *updated_at, messages);
        }
        v6
    }
    /// Both stores must answer every query with the same JSON, hit for hit.
    fn assert_same_as_v5(chats: &[Chat], queries: &[&str]) -> Vec<Value> {
        let (v5, v6) = (v5_store(chats), v6_store(chats));
        let mut all = vec![];
        for query in queries {
            let query = RecallQuery {
                query: (*query).into(),
                limit: Some(10),
                ..Default::default()
            };
            let old = v5.search_messages(MemoryClient::Cli, &query).unwrap();
            let new = search_v6(&v6, MemoryClient::Cli, &query).unwrap();
            let (old, new) = (
                serde_json::to_value(old).unwrap(),
                serde_json::to_value(new).unwrap(),
            );
            assert_eq!(new, old, "query {:?}", query.query);
            all.push(new);
        }
        all
    }
    fn user(id: &str, text: &str) -> Value {
        json!({"id": id, "role": "user", "content": text, "attachments": []})
    }
    fn reply(id: &str, text: &str) -> Value {
        json!({"id": id, "role": "assistant", "content": text, "attachments": []})
    }

    #[test]
    fn the_64_kib_clip_decides_the_excerpt_as_in_v5() {
        // Both words sit inside the 64 KiB cut but far apart, and again side by side beyond it.
        // snippet() prefers a window holding both words, so over the whole text it would show
        // the pair near the end; over what v5 stored it shows the early, single word.
        let text = format!(
            "zqxjone head {}zqxjtwo mid {}zqxjone zqxjtwo tail",
            "pad ".repeat(100),
            "pad ".repeat(20_000)
        );
        assert!(text.len() > MESSAGE_TEXT_BYTES + 1000);
        let chats = [("s", Some("chat"), 1, vec![user("u", &text)])];
        let found = assert_same_as_v5(&chats, &["zqxjone zqxjtwo", "tail", "pad"]);
        let hit = &found[0]["results"][0];
        assert!(hit["excerpt"].as_str().unwrap().contains("head"));
        assert!(!hit["excerpt"].as_str().unwrap().contains("tail"));
        // A word only past the cut is not found at all.
        assert!(found[1]["results"].as_array().unwrap().is_empty());
        // And the excerpt is what snippet() gives on the clipped text, made by hand.
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE VIRTUAL TABLE r USING fts5(content)")
            .unwrap();
        let end = indexed_len(&text, 0);
        assert_eq!(end, MESSAGE_TEXT_BYTES);
        connection
            .execute("INSERT INTO r(content) VALUES (?1)", [&text[..end]])
            .unwrap();
        let by_hand: String = connection
            .query_row(
                "SELECT snippet(r,0,'','',' … ',32) FROM r WHERE r MATCH '\"zqxjone\" AND \"zqxjtwo\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hit["excerpt"], by_hand.as_str());
        // The whole text would have given another one.
        connection
            .execute("UPDATE r SET content=?1", [&text])
            .unwrap();
        let whole: String = connection
            .query_row(
                "SELECT snippet(r,0,'','',' … ',32) FROM r WHERE r MATCH '\"zqxjone\" AND \"zqxjtwo\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(whole, by_hand);
    }

    #[test]
    fn the_512_kib_session_allowance_decides_the_excerpt_as_in_v5() {
        // Eight messages of 60,000 bytes use 480,000 of the 524,288. The ninth has 44,288 left, so
        // the words side by side at about 50,000 are cut off; the tenth has nothing left.
        let filler = |n: usize| "word ".repeat(n / 5);
        let mut messages = vec![];
        for i in 0..8 {
            let text = filler(60_000);
            messages.push(if i % 2 == 0 {
                user(&format!("m{i}"), &text)
            } else {
                reply(&format!("m{i}"), &text)
            });
        }
        let ninth = format!(
            "zqxjone head {}zqxjtwo mid {}zqxjone zqxjtwo tail {}",
            filler(500),
            filler(50_000),
            filler(10_000)
        );
        assert!(ninth.len() > 60_000);
        messages.push(user("m8", &ninth));
        messages.push(reply("m9", "zqxjlate zqxjone zqxjtwo"));
        let chats = [("s", Some("chat"), 1, messages)];
        let found = assert_same_as_v5(&chats, &["zqxjone zqxjtwo", "zqxjlate", "tail"]);
        let hits = found[0]["results"].as_array().unwrap();
        assert_eq!(
            hits.len(),
            1,
            "only the ninth message has room for its terms"
        );
        assert_eq!(hits[0]["messageId"], "m8");
        assert!(hits[0]["excerpt"].as_str().unwrap().contains("head"));
        assert!(!hits[0]["excerpt"].as_str().unwrap().contains("tail"));
        assert!(found[1]["results"].as_array().unwrap().is_empty());
        assert!(found[2]["results"].as_array().unwrap().is_empty());
    }

    #[test]
    fn mode_prefix_is_case_sensitive_as_in_v5() {
        let chats = [
            ("cli_lower", None, 3, vec![user("a", "caseword one")]),
            ("CLI_upper", None, 2, vec![user("b", "caseword two")]),
            ("Cli_mixed", None, 1, vec![user("c", "caseword three")]),
        ];
        let found = assert_same_as_v5(&chats, &["caseword"]);
        let modes: Vec<_> = found[0]["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                (
                    h["sessionId"].as_str().unwrap(),
                    h["mode"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            modes,
            [
                ("cli_lower", "agent"),
                ("CLI_upper", "chat"),
                ("Cli_mixed", "chat")
            ]
        );
    }

    #[test]
    fn a_chat_that_cannot_be_rebuilt_loses_only_its_own_hits() {
        let db = v6_in_memory();
        put(&db, "bad", "sharedterm bad", "/p", 2, Some("chat"));
        put(&db, "good", "sharedterm good", "/p", 1, Some("chat"));
        assert_eq!(find(&db, "sharedterm").results.len(), 2);
        db.connection()
            .execute(
                "UPDATE chat_events SET body='not json' WHERE session_id='bad' AND seq=0",
                [],
            )
            .unwrap();
        let hits = find(&db, "sharedterm").results;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "good");
        assert_eq!(hits[0].excerpt, "sharedterm good");
    }

    #[test]
    fn search_skips_the_open_run_wherever_it_sits() {
        // A run left open on the first user message folds right after it, so it is not the last
        // message. Eight users of 64 KiB would fill the session if the folded message counted.
        let db = v6_in_memory();
        db.create_session(&NewSession {
            id: "s".into(),
            title: "T".into(),
            mode: None,
            workspace: None,
            updated_at: 1,
        })
        .unwrap();
        let tx = write_tx(db.connection()).unwrap();
        let mut c = read_counters(&tx, "s").unwrap();
        let mut first = 0;
        for i in 0..8usize {
            let text = if i == 7 {
                format!("zqxjlast {}", "a ".repeat(32_000))
            } else {
                "a ".repeat(32_768)
            };
            let message = user(&format!("u{i}"), &text);
            let seq = append_ui_user(&tx, "s", &mut c, &message).unwrap();
            if i == 0 {
                first = seq;
            }
            // The open run's message sits at index 1, so user i is at i + 1 from the second on.
            let at = if i == 0 { 0 } else { i as u64 + 1 };
            index_message(&tx, "s", &mut c, seq, at, &message).unwrap();
        }
        append_run_start(&tx, "s", &mut c, "open", 2, first).unwrap();
        append_model(
            &tx,
            "s",
            &mut c,
            &json!({"role":"assistant","content":"b ".repeat(32_768)}),
        )
        .unwrap();
        write_counters(&tx, "s", &c, 1).unwrap();
        tx.commit().unwrap();
        let (messages, open) = db.load_for_search("s").unwrap().unwrap();
        assert_eq!((messages.len(), open), (9, Some(1)));
        assert_eq!(messages[1]["content"].as_str().unwrap().len(), 65_536);
        let before = crate::storage::chats::counter_repairs();
        let hits = find(&db, "zqxjlast").results;
        assert_eq!(crate::storage::chats::counter_repairs(), before);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            (hits[0].message_index, hits[0].message_id.as_str()),
            (8, "u7")
        );
        assert!(hits[0].excerpt.starts_with("zqxjlast"));
    }

    #[test]
    fn no_hits_make_no_temporary_table() {
        let db = v6_in_memory();
        put(&db, "s", "something", "/p", 1, Some("chat"));
        let before: i64 = db
            .connection()
            .query_row("SELECT total_changes()", [], |r| r.get(0))
            .unwrap();
        assert!(find(&db, "absentword").results.is_empty());
        let after: i64 = db
            .connection()
            .query_row("SELECT total_changes()", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after);
        let temp: i64 = db
            .connection()
            .query_row("SELECT count(*) FROM sqlite_temp_master", [], |r| r.get(0))
            .unwrap();
        assert_eq!(temp, 0, "a search with no hits never created one");
    }
}
