//! Search over v6 chats: a contentless FTS5 index keyed by `chat_events.id`, so a message's text
//! is never stored a second time. A hit's text and excerpt are rebuilt from the chat at query time.

use super::chats::read_chat;
use super::recall::{
    cap_results, match_expression, RecallHit, RecallQuery, RecallResults, MESSAGE_TEXT_BYTES,
};
use super::{LocalStore, StorageError};
use crate::memory::MemoryClient;
use rusqlite::{params, Connection};
use serde_json::Value;
use std::collections::HashMap;

/// A matching row: its `chat_events` id, chat id and seq, then the chat's title, mode, workspace
/// and date.
type Hit = (i64, String, i64, String, String, Option<String>, i64);

/// What the index holds for a UI message: the first 64 KiB of a user or assistant message's text.
fn indexed_text(message: &Value) -> &str {
    if !matches!(message["role"].as_str(), Some("user" | "assistant")) {
        return "";
    }
    let text = message["content"].as_str().unwrap_or_default();
    let mut end = text.len().min(MESSAGE_TEXT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Indexes one UI message under its row's `chat_events.id`, inside the caller's transaction.
pub(crate) fn index(tx: &Connection, row: i64, message: &Value) -> Result<(), StorageError> {
    let text = indexed_text(message);
    if !text.is_empty() {
        tx.prepare_cached("INSERT INTO chat_search(rowid,content) VALUES (?1,?2)")?
            .execute(params![row, text])?;
    }
    Ok(())
}

/// The excerpt for each text, made with the same FTS5 `snippet()` call v5 ran on its stored copy.
/// The texts go into a temporary table that lives only for this call.
fn snippets(
    connection: &Connection,
    texts: &[(i64, &str)],
    expression: &str,
) -> Result<HashMap<i64, String>, StorageError> {
    connection.execute_batch(
        "DROP TABLE IF EXISTS temp.snippet_text;
         CREATE VIRTUAL TABLE temp.snippet_text USING fts5(content);",
    )?;
    let made = (|| -> Result<HashMap<i64, String>, StorageError> {
        let mut insert =
            connection.prepare("INSERT INTO temp.snippet_text(rowid,content) VALUES (?1,?2)")?;
        for (key, text) in texts {
            insert.execute(params![key, text])?;
        }
        let mut select = connection.prepare(
            "SELECT rowid, snippet(snippet_text,0,'','',' … ',32) FROM temp.snippet_text
             WHERE snippet_text MATCH ?1",
        )?;
        let rows = select.query_map([expression], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    })();
    connection.execute_batch("DROP TABLE IF EXISTS temp.snippet_text")?;
    made
}

impl LocalStore {
    /// Recall over v6 chats. The filters, ranking, limits and hit shape are v5's.
    pub fn search_chats(
        &self,
        client: MemoryClient,
        query: &RecallQuery,
    ) -> Result<RecallResults, StorageError> {
        let expression = match_expression(client, query)?;
        let read = self.connection.unchecked_transaction()?;
        let rows: Vec<Hit> = read
            .prepare(
                "SELECT e.id, s.id, e.seq, s.title,
                    CASE WHEN s.mode='agent' THEN 'agent'
                         WHEN s.mode IS NULL AND substr(s.id,1,4)='cli_' THEN 'agent'
                         ELSE 'chat' END,
                    s.workspace, s.updated_at
                 FROM chat_search
                 JOIN chat_events e ON e.id = chat_search.rowid
                 JOIN sessions s ON s.id = e.session_id
                 WHERE chat_search MATCH ?1 AND (?2 IS NULL OR s.workspace = ?2)
                 AND (?3 IS NULL OR s.updated_at >= ?3) AND (?4 IS NULL OR s.updated_at <= ?4)
                 ORDER BY bm25(chat_search), s.updated_at DESC, e.seq LIMIT ?5",
            )?
            .query_map(
                params![
                    expression,
                    query.project,
                    query.after,
                    query.before,
                    query.limit.unwrap_or(5) as i64
                ],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )?
            .collect::<Result<_, _>>()?;
        // Each chat with a hit is rebuilt once: its messages, and their seqs.
        let mut chats: HashMap<String, (Value, Vec<i64>)> = HashMap::new();
        for (_, session_id, ..) in &rows {
            if !chats.contains_key(session_id) {
                if let Some(chat) = read_chat(&read, session_id)? {
                    chats.insert(session_id.clone(), chat);
                }
            }
        }
        read.commit()?;
        // Each hit's message: its position among the UI messages, and the message.
        let found: HashMap<i64, (usize, &Value)> = rows
            .iter()
            .filter_map(|(key, session_id, seq, ..)| {
                let (chat, seqs) = chats.get(session_id)?;
                let position = seqs.iter().position(|s| s == seq)?;
                Some((*key, (position, chat["messages"].get(position)?)))
            })
            .collect();
        let texts: Vec<(i64, &str)> = found
            .iter()
            .map(|(key, (_, message))| (*key, indexed_text(message)))
            .collect();
        let mut excerpts = snippets(&self.connection, &texts, &expression)?;
        let hits = rows.iter().filter_map(|row| {
            let (key, session_id, _, title, mode, project, updated_at) = row;
            let (position, message) = found.get(key)?;
            Some(Ok(RecallHit {
                session_id: session_id.clone(),
                // v5 recall reported `message_{index}` for a message without a usable id.
                message_id: message["id"]
                    .as_str()
                    .filter(|id| id.len() <= 128)
                    .map_or_else(|| format!("message_{position}"), str::to_owned),
                message_index: u32::try_from(*position).unwrap_or(u32::MAX),
                title: title.chars().take(120).collect(),
                mode: mode.clone(),
                project: project.clone(),
                updated_at: *updated_at,
                role: message["role"].as_str().unwrap_or_default().to_owned(),
                excerpt: excerpts.remove(key)?.chars().take(600).collect(),
            }))
        });
        Ok(RecallResults {
            results: cap_results(hits)?,
            remaining_sessions: 0,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::chats::tests::{message, new_chat, v6_store};
    use super::*;
    use serde_json::json;

    fn query(text: &str) -> RecallQuery {
        RecallQuery {
            query: text.into(),
            project: None,
            after: None,
            before: None,
            limit: None,
        }
    }

    #[test]
    fn search_finds_each_matching_message_with_v5_hit_fields() {
        let store = v6_store();
        new_chat(&store, "c1");
        new_chat(&store, "c2");
        store
            .add_message("c1", &message("q", "user", "How do pelicans fly?"), 1)
            .unwrap();
        store
            .add_message(
                "c1",
                &message("a", "assistant", "Pelicans glide on thermals."),
                2,
            )
            .unwrap();
        store
            .add_message(
                "c2",
                &json!({"role": "user", "content": "unrelated", "attachments": []}),
                3,
            )
            .unwrap();
        let results = store
            .search_chats(MemoryClient::Desktop, &query("pelicans"))
            .unwrap();
        let mut hits: Vec<_> = results
            .results
            .iter()
            .map(|hit| {
                (
                    hit.message_id.as_str(),
                    hit.message_index,
                    hit.role.as_str(),
                )
            })
            .collect();
        hits.sort();
        assert_eq!(hits, [("a", 1, "assistant"), ("q", 0, "user")]);
        let hit = &results.results[0];
        assert_eq!(
            (
                hit.session_id.as_str(),
                hit.title.as_str(),
                hit.mode.as_str()
            ),
            ("c1", "Chat c1", "agent")
        );
        assert_eq!(hit.project.as_deref(), Some("/work/project"));
        assert!(
            hit.excerpt.to_lowercase().contains("pelicans"),
            "{}",
            hit.excerpt
        );

        // A message without an id is reported by its position, as v5 did.
        let unnamed = store
            .search_chats(MemoryClient::Desktop, &query("unrelated"))
            .unwrap();
        assert_eq!(unnamed.results.len(), 1);
        assert_eq!(unnamed.results[0].message_id, "message_0");

        let mut elsewhere = query("pelicans");
        elsewhere.project = Some("/other".into());
        assert!(store
            .search_chats(MemoryClient::Desktop, &elsewhere)
            .unwrap()
            .results
            .is_empty());
        assert!(store
            .search_chats(MemoryClient::Guest, &query("pelicans"))
            .is_err());

        store.delete_chat("c1").unwrap();
        assert!(store
            .search_chats(MemoryClient::Desktop, &query("pelicans"))
            .unwrap()
            .results
            .is_empty());
    }
}
