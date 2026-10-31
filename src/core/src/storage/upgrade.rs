//! Opening at v6. An older database is backed up, then converted in one transaction: every chat
//! converts, or nothing changes.
//!
//! The backup is `backups/blackwall-backup-1.sqlite3`, then `-2`, then `-3`, made with SQLite's
//! backup API. Blackwall never deletes one. With all three there, the upgrade does not run and
//! the open fails with [`StorageError::BackupsFull`] until the owner removes one.

use super::chats::{apply_v6_schema, write_tx};
use super::convert::convert_session;
use super::{LocalStore, StorageError, DATABASE_FILE};
use rusqlite::{backup::NO_PROGRESS, Connection, OpenFlags, MAIN_DB};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

/// The schema version of the v6 chat store.
pub(crate) const V6: i64 = 6;
const BACKUPS_DIR: &str = "backups";
const MAX_BACKUPS: u32 = 3;

fn user_version(connection: &Connection) -> Result<i64, StorageError> {
    Ok(connection.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

pub(super) fn open_v6(
    directory: &Path,
    connection: Connection,
) -> Result<LocalStore, StorageError> {
    connection.execute_batch(
        "PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON; PRAGMA synchronous=FULL;",
    )?;
    if user_version(&connection)? != V6 {
        // Re-read under the writer lock: another window may have upgraded while this one waited.
        let tx = write_tx(&connection)?;
        let version = user_version(&tx)?;
        if version > V6 {
            return Err(StorageError::Invalid(
                "This data was created by a newer Blackwall version. Update the app to open it.",
            ));
        }
        if version < V6 {
            let has_data: bool = tx.query_row(
                "SELECT ?1 > 0 OR EXISTS(SELECT 1 FROM sqlite_master WHERE name='sessions')",
                [version],
                |row| row.get(0),
            )?;
            if has_data {
                back_up(directory)?;
            }
            LocalStore::legacy_schema(&tx, version)?;
            apply_v6_schema(&tx)?;
            let ids: Vec<String> = tx
                .prepare("SELECT id FROM sessions")?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            for id in ids {
                let body: String =
                    tx.query_row("SELECT body FROM sessions WHERE id=?1", [&id], |row| {
                        row.get(0)
                    })?;
                convert_session(&tx, &serde_json::from_str::<Value>(&body)?)?;
            }
            tx.pragma_update(None, "user_version", V6)?;
        }
        tx.commit()?;
    }
    Ok(LocalStore { connection })
}

/// Copies the database to the first free backup slot. The caller holds the writer lock, which
/// SQLite's backup cannot read through, so a second, read-only connection makes the copy.
fn back_up(directory: &Path) -> Result<PathBuf, StorageError> {
    let folder = directory.join(BACKUPS_DIR);
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&folder)?;
    let target = (1..=MAX_BACKUPS)
        .map(|n| folder.join(format!("blackwall-backup-{n}.sqlite3")))
        .find(|path| fs::symlink_metadata(path).is_err())
        .ok_or(StorageError::BackupsFull)?;
    let source = Connection::open_with_flags(
        directory.join(DATABASE_FILE),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    source.backup(MAIN_DB, &target, NO_PROGRESS)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
    }
    Ok(target)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;
    use crate::memory::MemoryClient;
    use crate::storage::RecallQuery;
    use serde_json::json;
    use std::time::{Duration, Instant};

    /// The v5 chats: what v5's real agent loop and desktop store saved, one chat per line.
    pub(crate) fn v5_chats() -> Vec<Value> {
        include_str!("fixtures/v5_chats.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn folder(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "blackwall-v6-{name}-{:016x}",
            rand::random::<u64>()
        ))
    }

    /// A v5 data folder, saved through the v5 store itself.
    fn v5_folder(name: &str, chats: &[Value]) -> PathBuf {
        let directory = folder(name);
        let store = LocalStore::open(&directory).unwrap();
        for chat in chats {
            store.save_session(chat).unwrap();
        }
        directory
    }

    fn version(path: &Path) -> i64 {
        user_version(&Connection::open(path).unwrap()).unwrap()
    }

    #[test]
    fn every_v5_chat_loads_back_unchanged_and_is_stored_once() {
        let chats = v5_chats();
        let directory = v5_folder("convert", &chats);
        let store = LocalStore::open_at(&directory, V6).unwrap();
        for chat in &chats {
            let id = chat["id"].as_str().unwrap();
            assert_eq!(store.load_chat(id).unwrap().as_ref(), Some(chat), "{id}");
        }
        assert_eq!(store.list_chats().unwrap().len(), chats.len());
        // The repeated values became references, and no v5 body is left.
        let count = |pattern: &str| -> i64 {
            store
                .connection()
                .query_row(
                    "SELECT count(*) FROM chat_events WHERE body LIKE ?1",
                    [pattern],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(
            count("{\"ref\":%"),
            28,
            "model rows that refer to a UI message"
        );
        assert_eq!(
            count("%\"content\":{\"turns\"%"),
            21,
            "contents built from model turns"
        );
        assert_eq!(
            count("%\"call\":[%"),
            8,
            "assistants with a tool built from model rows"
        );
        let bodies: i64 = store
            .connection()
            .query_row(
                "SELECT count(*) FROM sessions WHERE body <> ''",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bodies, 0);
        let hits = store
            .search_chats(
                MemoryClient::Desktop,
                &RecallQuery {
                    query: "needle haystack".into(),
                    ..RecallQuery::default()
                },
            )
            .unwrap();
        assert!(!hits.results.is_empty());
        // A second open finds v6 and changes nothing.
        drop(store);
        let again = LocalStore::open_at(&directory, V6).unwrap();
        assert_eq!(again.list_chats().unwrap().len(), chats.len());
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn backups_fill_slots_one_to_three_then_the_upgrade_refuses() {
        let chat = &v5_chats()[0];
        let directory = v5_folder("backups", std::slice::from_ref(chat));
        let database = directory.join(DATABASE_FILE);
        let backup = |n: u32| {
            directory
                .join(BACKUPS_DIR)
                .join(format!("blackwall-backup-{n}.sqlite3"))
        };
        for n in 1..=3 {
            LocalStore::open_at(&directory, V6).unwrap();
            assert_eq!(version(&database), V6);
            // Each backup is the whole v5 database.
            assert_eq!(version(&backup(n)), 5);
            assert!(!backup(n + 1).exists());
            let restored = LocalStore::open(&directory);
            assert!(restored.is_err(), "v6 is newer than v5");
            // The owner restores the backup, so the next open upgrades again.
            fs::copy(backup(n), &database).unwrap();
            assert_eq!(
                LocalStore::open(&directory)
                    .unwrap()
                    .load_session(chat["id"].as_str().unwrap())
                    .unwrap()
                    .as_ref(),
                Some(chat)
            );
        }
        assert!(matches!(
            LocalStore::open_at(&directory, V6),
            Err(StorageError::BackupsFull)
        ));
        assert_eq!(version(&database), 5, "nothing was converted");
        assert!(!backup(4).exists());
        // Once the owner removes one, its slot is used again.
        fs::remove_file(backup(2)).unwrap();
        LocalStore::open_at(&directory, V6).unwrap();
        assert_eq!(version(&database), V6);
        assert_eq!(version(&backup(2)), 5);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn one_chat_that_does_not_convert_leaves_every_chat_as_it_was() {
        let mut bad = v5_chats()[0].clone();
        bad["id"] = json!("bad");
        // A v5 entry that the v6 rows would read as a reference.
        bad["contextState"]["transcript"][0]["ref"] = json!(0);
        let chats = [v5_chats()[1].clone(), bad];
        let directory = v5_folder("rollback", &chats);
        assert!(matches!(
            LocalStore::open_at(&directory, V6),
            Err(StorageError::Invalid(message)) if message == super::super::convert::NOT_CONVERTED
        ));
        assert_eq!(version(&directory.join(DATABASE_FILE)), 5);
        let store = LocalStore::open(&directory).unwrap();
        for chat in &chats {
            let id = chat["id"].as_str().unwrap();
            assert_eq!(store.load_session(id).unwrap().as_ref(), Some(chat));
        }
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn the_browser_import_on_v6_converts_once_without_the_webview_context() {
        let directory = folder("import");
        let mut store = LocalStore::open_at(&directory, V6).unwrap();
        let chats = v5_chats();
        store.migrate_sessions(&chats[..3]).unwrap();
        for chat in &chats[..3] {
            let mut expected = chat.clone();
            expected.as_object_mut().unwrap().remove("contextState");
            let id = chat["id"].as_str().unwrap();
            assert_eq!(store.load_chat(id).unwrap(), Some(expected));
        }
        store.delete_chat(chats[0]["id"].as_str().unwrap()).unwrap();
        store.migrate_sessions(&chats[..3]).unwrap();
        assert_eq!(store.list_chats().unwrap().len(), 2, "runs once");
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_new_database_opens_at_v6_without_a_backup() {
        let directory = folder("fresh");
        let store = LocalStore::open_at(&directory, V6).unwrap();
        assert_eq!(version(&directory.join(DATABASE_FILE)), V6);
        assert!(!directory.join(BACKUPS_DIR).exists());
        assert!(store.list_chats().unwrap().is_empty());
        fs::remove_dir_all(&directory).unwrap();
    }

    /// A v5 agent chat of `exchanges` user/assistant pairs, saved the way v5 saves one: every
    /// third answer runs a tool, and the model transcript holds every turn.
    fn long_chat(exchanges: usize) -> Value {
        let (mut messages, mut transcript) = (vec![], vec![]);
        for i in 0..exchanges {
            let question = format!("Question {i}: what does module {i} do with its inputs?");
            messages.push(
                json!({"id": format!("u{i}"), "role": "user", "content": question,
                "attachments": [], "createdAt": i}),
            );
            transcript.push(json!({"role": "user", "content": question}));
            let answer = format!(
                "Module {i} validates its inputs, then {}.",
                "stores them ".repeat(20)
            );
            let mut reply = json!({"id": format!("a{i}"), "role": "assistant", "content": answer,
                "attachments": [], "status": "complete", "createdAt": i});
            if i % 3 == 0 {
                let call = format!("call{i}");
                let arguments = format!("{{\"path\":\"src/module_{i}.rs\"}}");
                let output = format!("pub fn module_{i}() {{}}\n").repeat(30);
                transcript.push(json!({"role": "assistant", "content": "Reading it.",
                    "tool_calls": [{"id": call, "type": "function",
                        "function": {"name": "read_file", "arguments": arguments}}]}));
                transcript.push(json!({"role": "tool", "tool_call_id": call, "content": output}));
                reply["content"] = json!(format!("Reading it.\n\n{answer}"));
                reply["tools"] = json!([{"id": format!("0_0_{call}"), "name": "read_file",
                    "arguments": arguments, "output": output, "status": "complete",
                    "contentOffset": 11}]);
            }
            transcript.push(json!({"role": "assistant", "content": answer}));
            messages.push(reply);
        }
        json!({"id": "long", "title": "Long chat", "updatedAt": 1, "mode": "agent",
            "messages": messages, "contextState": {"transcript": transcript, "checkpoints": [],
            "coveredMessages": messages.len(),
            "sourceHash": crate::context::fingerprint(&transcript)}})
    }

    /// The budget for opening a chat: 50 ms for 300 messages (R4-6).
    #[test]
    fn a_300_message_chat_loads_within_50_ms() {
        let chat = long_chat(150);
        let directory = v5_folder("speed", std::slice::from_ref(&chat));
        let store = LocalStore::open_at(&directory, V6).unwrap();
        assert_eq!(store.load_chat("long").unwrap().as_ref(), Some(&chat));
        let references: i64 = store
            .connection()
            .query_row(
                "SELECT count(*) FROM chat_events WHERE body LIKE '{\"ref\":%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(references, 150, "each user turn is stored once");
        let mut times: Vec<Duration> = (0..9)
            .map(|_| {
                let started = Instant::now();
                store.load_chat("long").unwrap();
                started.elapsed()
            })
            .collect();
        times.sort();
        let median = times[times.len() / 2];
        eprintln!("load of a 300-message chat: {median:?}");
        assert!(median <= Duration::from_millis(50), "{median:?}");
        fs::remove_dir_all(&directory).unwrap();
    }
}
