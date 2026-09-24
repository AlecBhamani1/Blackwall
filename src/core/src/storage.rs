//! Versioned local SQLite persistence. All values are bounded and writes are transactional.
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs, path::Path, time::Duration};
use thiserror::Error;

const MAX_SESSION_BYTES: usize = 48 * 1024 * 1024;
const MAX_SESSIONS: i64 = 500;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("Blackwall could not access its local data folder. Check its permissions and available disk space.")]
    Io(#[from] std::io::Error),
    #[error("Blackwall could not read or save its local database. Your existing data has been left in place.")]
    Database(#[from] rusqlite::Error),
    #[error("The saved data is not valid. Your existing data has been left in place.")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(&'static str),
}

pub struct LocalStore {
    connection: Connection,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub updated_at: i64,
}

impl LocalStore {
    pub fn open(directory: &Path) -> Result<Self, StorageError> {
        if fs::symlink_metadata(directory).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(StorageError::Invalid(
                "The local data folder must not be a symbolic link.",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)?;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
            let path = directory.join("blackwall.sqlite3");
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
            {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        #[cfg(not(unix))]
        fs::create_dir_all(directory)?;
        let path = directory.join("blackwall.sqlite3");
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(StorageError::Invalid(
                "The local database must not be a symbolic link.",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        Self::initialize(connection)
    }
    fn initialize(connection: Connection) -> Result<Self, StorageError> {
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > 2 {
            return Err(StorageError::Invalid(
                "This data was created by a newer Blackwall version. Update the app to open it.",
            ));
        }
        connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON; PRAGMA synchronous=FULL;
            BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, title TEXT NOT NULL, updated_at INTEGER NOT NULL, body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS memory (id TEXT PRIMARY KEY, content TEXT NOT NULL, updated_at INTEGER NOT NULL);
            CREATE VIRTUAL TABLE IF NOT EXISTS memory_search USING fts5(id UNINDEXED, content);
            CREATE TABLE IF NOT EXISTS paired_devices (id TEXT PRIMARY KEY, body TEXT NOT NULL);
            PRAGMA user_version=2; COMMIT;")?;
        Ok(Self { connection })
    }
    pub fn paired_devices(&self) -> Result<Vec<crate::pairing::PairedDevice>, StorageError> {
        let mut query = self
            .connection
            .prepare("SELECT body FROM paired_devices ORDER BY id")?;
        let bodies = query
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        bodies
            .into_iter()
            .map(|body| {
                let device: crate::pairing::PairedDevice = serde_json::from_str(&body)?;
                device
                    .validate()
                    .map_err(|_| StorageError::Invalid("A paired device record is invalid."))?;
                Ok(device)
            })
            .collect()
    }
    pub fn save_paired_device(
        &self,
        device: &crate::pairing::PairedDevice,
    ) -> Result<(), StorageError> {
        device
            .validate()
            .map_err(|_| StorageError::Invalid("Invalid paired device."))?;
        let transaction = self.connection.unchecked_transaction()?;
        let previous: Option<String> = transaction
            .query_row(
                "SELECT body FROM paired_devices WHERE id=?1",
                [&device.id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            let previous: crate::pairing::PairedDevice = serde_json::from_str(&previous)?;
            // Recheck inside the write transaction: another process may have removed
            // the device after a native command read its earlier pending state.
            let regresses = match previous.state.as_str() {
                "revoked" => device.state != "revoked",
                "revoking" => !matches!(device.state.as_str(), "revoking" | "revoked"),
                "active" => device.state == "pending",
                _ => false,
            };
            if regresses {
                return Err(StorageError::Invalid("This paired computer changed or was removed. Refresh its status before trying again."));
            }
        }
        let count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM paired_devices WHERE id != ?1",
            [&device.id],
            |row| row.get(0),
        )?;
        if count >= 20 {
            return Err(StorageError::Invalid(
                "Remove an older paired computer before adding another.",
            ));
        }
        transaction.execute("INSERT INTO paired_devices(id,body) VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body", params![device.id, serde_json::to_string(device)?])?;
        transaction.commit()?;
        Ok(())
    }
    pub fn delete_paired_device(&self, id: &str) -> Result<(), StorageError> {
        self.connection
            .execute("DELETE FROM paired_devices WHERE id=?1", [id])?;
        Ok(())
    }
    pub fn list_sessions(&self) -> Result<Vec<Value>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT id,title,updated_at FROM sessions ORDER BY updated_at DESC LIMIT 500",
        )?;
        let rows = statement.query_map([], |row| Ok(json!({"id":row.get::<_,String>(0)?,"title":row.get::<_,String>(1)?,"updatedAt":row.get::<_,i64>(2)?,"messages":[]})))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
    pub fn load_session(&self, id: &str) -> Result<Option<Value>, StorageError> {
        validate_id(id)?;
        let body: Option<String> = self
            .connection
            .query_row("SELECT body FROM sessions WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        body.map(|body| serde_json::from_str(&body).map_err(StorageError::from))
            .transpose()
    }
    pub fn save_session(&self, session: &Value) -> Result<(), StorageError> {
        let (id, title, updated, body) = validate_session(session)?;
        let exists: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
            [id],
            |r| r.get(0),
        )?;
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
        if !exists && count >= MAX_SESSIONS {
            return Err(StorageError::Invalid("Your history has reached 500 conversations. Export or delete an older conversation before starting another."));
        }
        self.connection.execute("INSERT INTO sessions(id,title,updated_at,body) VALUES (?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET title=excluded.title,updated_at=excluded.updated_at,body=excluded.body", params![id, title, updated, body])?;
        Ok(())
    }
    pub fn delete_session(&self, id: &str) -> Result<(), StorageError> {
        validate_id(id)?;
        self.connection
            .execute("DELETE FROM sessions WHERE id=?1", [id])?;
        Ok(())
    }
    pub fn migrate_sessions(&mut self, sessions: &[Value]) -> Result<(), StorageError> {
        if self.setting("legacy_sessions_migrated")?.is_some() {
            return Ok(());
        }
        if sessions.len() > 30 {
            return Err(StorageError::Invalid(
                "The old history contains too many conversations to migrate safely.",
            ));
        }
        // Validate everything before starting, and keep migration all-or-nothing.
        let validated: Vec<_> = sessions
            .iter()
            .map(validate_session)
            .collect::<Result<_, _>>()?;
        let transaction = self.connection.transaction()?;
        for (id, title, updated, body) in validated {
            transaction.execute(
                "INSERT OR IGNORE INTO sessions(id,title,updated_at,body) VALUES (?1,?2,?3,?4)",
                params![id, title, updated, body],
            )?;
        }
        transaction.execute(
            "INSERT INTO settings(key,value) VALUES ('legacy_sessions_migrated','true')",
            [],
        )?;
        transaction.commit()?;
        Ok(())
    }
    pub fn setting(&self, key: &str) -> Result<Option<String>, StorageError> {
        Ok(self
            .connection
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }
    /// Applies a settings patch under a write transaction so independent UI panels cannot lose each other's updates.
    pub fn patch_preferences(&mut self, patch: &Value) -> Result<(), StorageError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let saved: Option<String> = transaction
            .query_row(
                "SELECT value FROM settings WHERE key='preferences'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let mut merged: Value = saved
            .map(|text| serde_json::from_str(&text))
            .transpose()?
            .unwrap_or_else(|| json!({}));
        let object = merged
            .as_object_mut()
            .ok_or(StorageError::Invalid("The saved preferences are invalid."))?;
        object.extend(
            patch
                .as_object()
                .ok_or(StorageError::Invalid("Invalid preferences."))?
                .clone(),
        );
        let body = serde_json::to_string(&merged)?;
        if body.len() > 65536 {
            return Err(StorageError::Invalid(
                "The preferences exceed their size limit.",
            ));
        }
        transaction.execute("INSERT INTO settings(key,value) VALUES ('preferences',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [body])?;
        transaction.commit()?;
        Ok(())
    }
    pub fn save_setting(&self, key: &str, value: &str) -> Result<(), StorageError> {
        if key.len() > 80 || value.len() > 64 * 1024 {
            return Err(StorageError::Invalid("This setting is too large."));
        }
        self.connection.execute("INSERT INTO settings(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [key,value])?;
        Ok(())
    }
    pub fn memories(&self, query: &str) -> Result<Vec<MemoryEntry>, StorageError> {
        let query = query.trim();
        if query.len() > 1024 {
            return Err(StorageError::Invalid("Use a shorter memory search."));
        }
        let (sql, parameter) = if query.is_empty() {
            ("SELECT id,content,updated_at FROM memory WHERE ?1='' ORDER BY updated_at DESC LIMIT 200", String::new())
        } else {
            // Quote search terms to avoid treating user text as FTS operators.
            let phrase = query
                .split_whitespace()
                .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" AND ");
            ("SELECT memory.id,memory.content,memory.updated_at FROM memory JOIN memory_search ON memory.id=memory_search.id WHERE memory_search MATCH ?1 ORDER BY rank LIMIT 50", phrase)
        };
        let mut statement = self.connection.prepare(sql)?;
        let rows = statement.query_map([parameter], |r| {
            Ok(MemoryEntry {
                id: r.get(0)?,
                content: r.get(1)?,
                updated_at: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
    pub fn save_memory(&mut self, entry: &MemoryEntry) -> Result<(), StorageError> {
        validate_id(&entry.id)?;
        if entry.content.trim().is_empty() || entry.content.len() > 8000 || entry.updated_at < 0 {
            return Err(StorageError::Invalid(
                "A memory must contain between 1 and 8,000 bytes of text.",
            ));
        }
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM memory WHERE id != ?1",
            [&entry.id],
            |r| r.get(0),
        )?;
        if count >= 2000 {
            return Err(StorageError::Invalid(
                "Memory is full. Remove an older memory first.",
            ));
        }
        let transaction = self.connection.transaction()?;
        transaction.execute("INSERT INTO memory(id,content,updated_at) VALUES (?1,?2,?3) ON CONFLICT(id) DO UPDATE SET content=excluded.content,updated_at=excluded.updated_at", params![entry.id,entry.content,entry.updated_at])?;
        transaction.execute("DELETE FROM memory_search WHERE id=?1", [&entry.id])?;
        transaction.execute(
            "INSERT INTO memory_search(id,content) VALUES (?1,?2)",
            [&entry.id, &entry.content],
        )?;
        transaction.commit()?;
        Ok(())
    }
    pub fn delete_memory(&mut self, id: &str) -> Result<(), StorageError> {
        validate_id(id)?;
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM memory_search WHERE id=?1", [id])?;
        transaction.execute("DELETE FROM memory WHERE id=?1", [id])?;
        transaction.commit()?;
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<(), StorageError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(StorageError::Invalid("Invalid saved item identifier."));
    }
    Ok(())
}
fn validate_session(session: &Value) -> Result<(&str, &str, i64, String), StorageError> {
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
    let body = serde_json::to_string(session)?;
    if body.len() > MAX_SESSION_BYTES {
        return Err(StorageError::Invalid(
            "This conversation is too large to save. Export it before closing Blackwall.",
        ));
    }
    Ok((id, title, updated as i64, body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    fn store() -> LocalStore {
        LocalStore::initialize(Connection::open_in_memory().unwrap()).unwrap()
    }
    fn session(id: &str) -> Value {
        json!({"id":id,"title":"A conversation","updatedAt":123,"messages":[{"role":"user","content":"Read this","attachments":[{"textContent":"retained bytes"}]}]})
    }
    #[test]
    fn stale_pairing_writes_cannot_reverse_committed_removal_or_activation() {
        use crate::pairing::*;
        let directory =
            std::env::temp_dir().join(format!("blackwall-pair-order-{}", secret("bwd_")));
        let (salt, hash) = digest(&secret("bw1_"));
        let mut device = PairedDevice {
            id: secret("bws_"),
            role: "host".into(),
            name: "Test".into(),
            relay_url: "https://pairing-tests.invalid".into(),
            model: "test".into(),
            upstream: Some("http://127.0.0.1:11434/v1".into()),
            salt,
            hash,
            state: "pending".into(),
        };
        let writer = LocalStore::open(&directory).unwrap();
        let other_process = LocalStore::open(&directory).unwrap();
        writer.save_paired_device(&device).unwrap();
        let stale = device.clone();
        device.state = "active".into();
        other_process.save_paired_device(&device).unwrap();
        assert!(writer.save_paired_device(&stale).is_err());
        let stale_active = device.clone();
        for removed in ["revoking", "revoked"] {
            device.state = removed.into();
            other_process.save_paired_device(&device).unwrap();
            assert!(writer.save_paired_device(&stale).is_err());
            assert!(writer.save_paired_device(&stale_active).is_err());
            assert_eq!(writer.paired_devices().unwrap()[0].state, removed);
        }
        drop(writer);
        drop(other_process);
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn pairing_schema_upgrade_preserves_existing_conversations_and_rejects_future_data() {
        let db = store();
        let conversation = session("before_pairing");
        db.save_session(&conversation).unwrap();
        db.connection
            .execute_batch("DROP TABLE paired_devices; PRAGMA user_version=1;")
            .unwrap();
        let upgraded = LocalStore::initialize(db.connection).unwrap();
        assert_eq!(
            upgraded.load_session("before_pairing").unwrap(),
            Some(conversation)
        );
        assert!(upgraded.paired_devices().unwrap().is_empty());
        let version: i64 = upgraded
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        upgraded
            .connection
            .pragma_update(None, "user_version", 3)
            .unwrap();
        assert!(LocalStore::initialize(upgraded.connection).is_err());
    }
    #[test]
    fn session_lifecycle_retains_attachment_contents() {
        let db = store();
        let value = session("session_one");
        db.save_session(&value).unwrap();
        assert_eq!(db.load_session("session_one").unwrap(), Some(value));
        assert_eq!(db.list_sessions().unwrap()[0]["messages"], json!([]));
        db.delete_session("session_one").unwrap();
        assert!(db.list_sessions().unwrap().is_empty());
    }
    #[test]
    fn migration_is_atomic_and_does_not_resurrect_deleted_legacy_sessions() {
        let mut db = store();
        assert!(db
            .migrate_sessions(&[session("valid"), json!({"id":"bad"})])
            .is_err());
        assert!(db.list_sessions().unwrap().is_empty());
        db.migrate_sessions(&[session("valid")]).unwrap();
        db.delete_session("valid").unwrap();
        db.migrate_sessions(&[session("valid")]).unwrap();
        assert!(db.list_sessions().unwrap().is_empty());
    }
    #[test]
    fn memory_search_tracks_edits_and_deletions() {
        let mut db = store();
        let mut entry = MemoryEntry {
            id: "memory_one".into(),
            content: "I prefer concise answers".into(),
            updated_at: 1,
        };
        db.save_memory(&entry).unwrap();
        assert_eq!(db.memories("concise").unwrap().len(), 1);
        entry.content = "I prefer detailed answers".into();
        db.save_memory(&entry).unwrap();
        assert!(db.memories("concise").unwrap().is_empty());
        assert_eq!(db.memories("detailed").unwrap().len(), 1);
        db.delete_memory(&entry.id).unwrap();
        assert!(db.memories("detailed").unwrap().is_empty());
    }
}
