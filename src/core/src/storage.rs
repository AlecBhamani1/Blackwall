//! Versioned local SQLite persistence. All values are bounded and writes are transactional.
use rusqlite::{config::DbConfig, params, Connection, ErrorCode, OptionalExtension};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use thiserror::Error;

/// The local database file in the data folder.
pub const DATABASE_FILE: &str = "blackwall.sqlite3";
/// Empty, non-secret file in the data folder: an app lock exists, even when the keyring cannot say so.
pub const LOCK_MARKER: &str = "app-lock-enabled";
/// Empty, non-secret file in the data folder: this data has no app lock.
pub const NO_LOCK_MARKER: &str = "app-lock-disabled";
/// Most bytes any one `chat_events` body may hold, serialized.
pub const MAX_EVENT_BYTES: usize = 32 << 20;
/// Most bytes one context checkpoint may hold.
pub const MAX_CHECKPOINT_BYTES: usize = 1 << 20;
/// Most checkpoints one session may hold.
pub const MAX_CHECKPOINTS: usize = 100;
/// Most UI messages one session may hold; reserved assistant slots count.
pub const MAX_UI_MESSAGES: u64 = 2000;
/// Most `model` rows one session may hold.
pub const MAX_MODEL_ENTRIES: u64 = 20_000;
/// Most bytes one session may hold: event bodies, checkpoint bodies and metadata.
pub const MAX_SESSION_BYTES: u64 = 48 << 20;
/// Most bytes one session's metadata may hold.
pub const MAX_METADATA_BYTES: usize = 64 << 10;
const MAX_SESSIONS: i64 = 500;
/// How long the v5 to v6 upgrade waits for another window's writer lock.
const UPGRADE_BUSY_TIMEOUT: Duration = Duration::from_secs(120);
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const BACKFILL_BUSY_TIMEOUT: Duration = Duration::from_millis(50);
const SCHEMA_VERSION: i64 = 5;
/// The schema version `open` upgrades to. It stays at v5 until the v6 event log goes live.
const OPEN_TARGET: i64 = 5;
const NEWER_VERSION: &str =
    "This data was created by a newer Blackwall version. Update the app to open it.";

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
    #[error("This conversation changed elsewhere. Reload it and try again.")]
    Conflict { current: i64 },
    #[error("{}", .0.message())]
    Limit(Cap),
    #[error("This conversation could not be found.")]
    NotFound,
    #[error(
        "This conversation is running in another window. Wait for it to finish, then try again."
    )]
    RunInProgress,
    #[error("Blackwall is still upgrading your chats in another window. Try again in a moment.")]
    UpgradeBusy,
    #[error("Couldn't back up your chats before upgrading them, so nothing was changed. Check free disk space and folder permissions, then try again.")]
    BackupFailed,
    /// The backup is `None` when a fresh database failed to create its schema: nothing existed to
    /// copy. `source` is the typed error that stopped the upgrade (P2-2).
    #[error("{}", upgrade_text(.backup))]
    UpgradeFailed {
        backup: Option<PathBuf>,
        source: Box<StorageError>,
    },
    #[error("{}", UPGRADE_FAILED_BACKED_UP)]
    UpgradeStopped(PathBuf),
}

/// What the owner reads when an upgrade with a backup fails or stops. It never holds the backup's
/// path, which would cross IPC as error text (security L6): the path is logged where the error is
/// made, and the error and the notices carry it as data.
const UPGRADE_FAILED_BACKED_UP: &str = "Couldn't upgrade your chats; your data is safe, and a \
     backup of it is in the backups folder inside Blackwall's data folder.";

fn upgrade_text(backup: &Option<PathBuf>) -> &'static str {
    match backup {
        Some(_) => UPGRADE_FAILED_BACKED_UP,
        None => "Couldn't upgrade your chats; your existing data has been left in place.",
    }
}

pub struct LocalStore {
    connection: Connection,
}
mod backup;
mod chats;
mod convert;
#[cfg(test)]
pub(crate) mod fixtures;
mod memory;
#[cfg(test)]
mod perf_tests;
#[cfg(test)]
mod property_tests;
mod recall;
mod search;
mod upgrade;
mod view;
pub use chats::{Cap, MetadataPatch, NewSession, StoredChat, StoredContext};
/// Moved to `convert`; re-exported here for the v5 save path until B6.
pub(crate) use convert::validate_session;
pub use memory::{MemoryEntry, MemoryProposal, MemoryScope, MemorySource, ProposalInput};
pub use recall::{RecallHit, RecallQuery, RecallResults};
pub use upgrade::{StorageNotice, UpgradeStatus};

impl LocalStore {
    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }
    pub fn open(directory: &Path) -> Result<Self, StorageError> {
        Self::open_at(directory, OPEN_TARGET)
    }
    /// Opens the database at schema version `target`: 5, or 6, which upgrades an older database
    /// first (see `upgrade`). `open` stays at v5 until the switch-over.
    pub fn open_at(directory: &Path, target: i64) -> Result<Self, StorageError> {
        Self::open_with(directory, target, false)
    }
    /// [`Self::open_at`]; `retry` is the Retry button, which lets a stopped v6 upgrade try again.
    fn open_with(directory: &Path, target: i64, retry: bool) -> Result<Self, StorageError> {
        if target != SCHEMA_VERSION && target != upgrade::V6 {
            return Err(StorageError::Invalid(
                "This version of Blackwall cannot open that storage format yet.",
            ));
        }
        if fs::symlink_metadata(directory).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(StorageError::Invalid(
                "The local data folder must not be a symbolic link.",
            ));
        }
        let path = directory.join(DATABASE_FILE);
        #[cfg(unix)]
        let created = {
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)?;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
                Err(error) => return Err(error.into()),
            }
        };
        #[cfg(not(unix))]
        let created = {
            fs::create_dir_all(directory)?;
            !path.try_exists().unwrap_or(true)
        };
        // A brand-new database has nothing to protect, so it may open without a keyring.
        // Best effort: without the marker, a keyring-less launch waits for the keyring.
        if created && !directory.join(LOCK_MARKER).try_exists().unwrap_or(true) {
            let _ = fs::write(directory.join(NO_LOCK_MARKER), b"");
        }
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
        if target == upgrade::V6 {
            return upgrade::open_v6(directory, connection, retry);
        }
        Self::initialize(connection)
    }
    fn initialize(connection: Connection) -> Result<Self, StorageError> {
        configure(&connection, BUSY_TIMEOUT)?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StorageError::Invalid(NEWER_VERSION));
        }
        if version == SCHEMA_VERSION {
            let store = Self { connection };
            if store.message_index_is_current()? {
                return Ok(store);
            }
            store.connection.busy_timeout(BACKFILL_BUSY_TIMEOUT)?;
            let result = store.migrate_message_index();
            store.connection.busy_timeout(BUSY_TIMEOUT)?;
            return match result {
                Err(error) if is_busy(&error) => Ok(store),
                result => result.map(|()| store),
            };
        }

        // Only schema creation and upgrades wait for the writer lock. Re-read the version after
        // acquiring it because another opener may have completed the upgrade while this one waited.
        connection.execute_batch("BEGIN IMMEDIATE;")?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StorageError::Invalid(NEWER_VERSION));
        }
        if version == SCHEMA_VERSION {
            connection.execute_batch("ROLLBACK;")?;
            return Ok(Self { connection });
        }
        Self::legacy_schema(&connection, version)?;
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        connection.execute_batch("COMMIT;")?;
        // Defer legacy message indexing until a later open so the migration winner releases the
        // writer lock directly to any other openers waiting to observe the new schema.
        Ok(Self { connection })
    }
    /// Brings a database below v5 to the v5 tables, inside the caller's write transaction.
    fn legacy_schema(connection: &Connection, version: i64) -> Result<(), StorageError> {
        connection.execute_batch("
            CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, title TEXT NOT NULL, updated_at INTEGER NOT NULL, body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS memory (id TEXT PRIMARY KEY, content TEXT NOT NULL, updated_at INTEGER NOT NULL);
            CREATE VIRTUAL TABLE IF NOT EXISTS memory_search USING fts5(id UNINDEXED, content);
            CREATE TABLE IF NOT EXISTS paired_devices (id TEXT PRIMARY KEY, body TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS sessions_updated_at ON sessions(updated_at DESC);
            ")?;
        if version < 3 {
            connection.execute_batch(
                "
                ALTER TABLE memory ADD COLUMN scope TEXT NOT NULL DEFAULT 'user';
                ALTER TABLE memory ADD COLUMN workspace TEXT NOT NULL DEFAULT '';
                ALTER TABLE memory ADD COLUMN fact_key TEXT NOT NULL DEFAULT '';
                ALTER TABLE memory ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;
                ALTER TABLE memory ADD COLUMN source TEXT;
                UPDATE memory SET fact_key=id;
                CREATE UNIQUE INDEX memory_fact ON memory(scope,workspace,fact_key);
                CREATE TABLE memory_proposals (id TEXT PRIMARY KEY, body TEXT NOT NULL);
                PRAGMA user_version=3;",
            )?;
        }
        recall::schema(connection)?;
        Ok(())
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
            "SELECT id, title, updated_at,
                    json_extract(body, '$.mode'), json_extract(body, '$.workspace')
             FROM (
                 SELECT id, title, updated_at, body
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
            return Err(StorageError::Invalid(Cap::Sessions.message()));
        }
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute("INSERT INTO sessions(id,title,updated_at,body) VALUES (?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET title=excluded.title,updated_at=excluded.updated_at,body=excluded.body", params![id, title, updated, body])?;
        recall::index(&transaction, session)?;
        transaction.commit()?;
        Ok(())
    }
    pub fn delete_session(&self, id: &str) -> Result<(), StorageError> {
        validate_id(id)?;
        self.connection
            .execute("DELETE FROM sessions WHERE id=?1", [id])?;
        Ok(())
    }
    pub fn migrate_sessions(&mut self, sessions: &[Value]) -> Result<(), StorageError> {
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= upgrade::V6 {
            return upgrade::migrate(self, sessions);
        }
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
        self.migrate_message_index()?;
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
}

pub(crate) fn validate_id(id: &str) -> Result<(), StorageError> {
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
/// What `configure` reports when `foreign_keys` would not turn on.
pub(crate) const FOREIGN_KEYS_OFF: &str = "Blackwall could not turn on its database safety checks. Your existing data has been left in place.";

/// Every connection starts here: foreign keys, secure delete and full syncs, with `busy` as its
/// lock wait. `foreign_keys` is a silent no-op inside a transaction, so it is set and read back
/// first, before `synchronous` (which errors inside one) can mask the failure.
///
/// It also hardens the connection against a tampered file (security L8):
/// - `SQLITE_DBCONFIG_DEFENSIVE`: no `writable_schema`, no direct writes to the FTS5 shadow
///   tables, nothing that can corrupt the file on purpose.
/// - `trusted_schema=OFF`: a trigger or view in the file may only call functions and virtual
///   tables marked innocuous. FTS5 is, so the search triggers and the cascade into `chat_search`
///   still run.
pub(crate) fn configure(connection: &Connection, busy: Duration) -> Result<(), StorageError> {
    connection.busy_timeout(busy)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF;")?;
    let on: i64 = connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    if on != 1 {
        return Err(StorageError::Invalid(FOREIGN_KEYS_OFF));
    }
    connection.execute_batch("PRAGMA secure_delete=ON; PRAGMA synchronous=FULL;")?;
    Ok(())
}

fn is_busy(error: &StorageError) -> bool {
    matches!(
        error,
        StorageError::Database(rusqlite::Error::SqliteFailure(error, _))
            if matches!(error.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::{
        path::{Path, PathBuf},
        sync::{Arc, Barrier},
        thread,
        time::{Duration, Instant},
    };

    fn store() -> LocalStore {
        LocalStore::initialize(Connection::open_in_memory().unwrap()).unwrap()
    }
    fn session(id: &str) -> Value {
        json!({"id":id,"title":"A conversation","updatedAt":123,"messages":[{"role":"user","content":"Read this","attachments":[{"textContent":"retained bytes"}]}]})
    }

    struct SyntheticStore {
        directory: PathBuf,
    }

    impl Drop for SyntheticStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn perf_scale(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|value| *value > 0)
            .unwrap_or(default)
    }

    fn synthetic_text(session_index: usize, message_index: usize) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 ";
        let mut state = ((session_index as u64 + 1) << 32) | (message_index as u64 + 1);
        let mut text = String::with_capacity(1536);
        for _ in 0..1536 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            text.push(ALPHABET[state as usize % ALPHABET.len()] as char);
        }
        text
    }

    fn synthetic_session(session_index: usize, message_count: usize) -> Value {
        let messages: Vec<_> = (0..message_count)
            .map(|message_index| {
                json!({
                    "id": format!("message_{message_index}"),
                    "role": if message_index % 2 == 0 { "user" } else { "assistant" },
                    "content": synthetic_text(session_index, message_index),
                    "attachments": []
                })
            })
            .collect();
        let transcript: Vec<_> = messages
            .iter()
            .map(|message| {
                json!({
                    "role": message["role"],
                    "content": message["content"]
                })
            })
            .collect();
        let context = crate::context::ContextState::restore(&transcript, None).unwrap();
        json!({
            "id": format!("perf_session_{session_index:04}"),
            "title": format!("Synthetic conversation {session_index}"),
            "updatedAt": 1_700_000_000_000_u64 + session_index as u64,
            "mode": if session_index.is_multiple_of(2) { "chat" } else { "agent" },
            "workspace": format!("/synthetic/project_{}", session_index % 8),
            "messages": messages,
            "contextState": context
        })
    }

    /// Builds a valid current-format store, then makes its search index look like an imported
    /// pre-index database so that the next `LocalStore::open` runs the real backfill path.
    fn build_synthetic_store(session_count: usize, message_count: usize) -> SyntheticStore {
        let directory = std::env::temp_dir().join(format!(
            "blackwall-large-history-{:016x}",
            rand::random::<u64>()
        ));
        let store = LocalStore::open(&directory).unwrap();
        store
            .save_setting("preferences", r#"{"model":"synthetic"}"#)
            .unwrap();

        let public_session_count = session_count.min(MAX_SESSIONS as usize);
        for session_index in 0..public_session_count {
            let value = synthetic_session(session_index, message_count);
            store.save_session(&value).unwrap_or_else(|error| {
                panic!("failed to seed synthetic session {session_index}: {error:?}")
            });
        }
        // Public saves intentionally cap newly-created history at 500. Large imported databases
        // can exceed that, so only overflow fixture rows use direct SQL, grouped in one
        // transaction.
        let transaction = store.connection.unchecked_transaction().unwrap();
        for session_index in public_session_count..session_count {
            let value = synthetic_session(session_index, message_count);
            let (id, title, updated_at, body) = validate_session(&value).unwrap();
            transaction
                .execute(
                    "INSERT INTO sessions(id,title,updated_at,body) VALUES (?1,?2,?3,?4)",
                    params![id, title, updated_at, body],
                )
                .unwrap();
        }
        transaction
            .execute("DELETE FROM indexed_sessions", [])
            .unwrap();
        transaction.commit().unwrap();
        drop(store);
        SyntheticStore { directory }
    }

    fn load_preferences(directory: &Path) -> Result<Value, StorageError> {
        LocalStore::open(directory)?
            .setting("preferences")?
            .map(|text| serde_json::from_str(&text).map_err(StorageError::from))
            .transpose()
            .map(|value| value.unwrap_or_else(|| json!({})))
    }

    #[test]
    fn synthetic_large_history_helper_builds_a_tiny_store() {
        let fixture = build_synthetic_store(3, 4);
        let store = LocalStore::open(&fixture.directory).unwrap();
        let summaries = store.list_sessions().unwrap();
        assert_eq!(summaries.len(), 3);
        assert_eq!(
            store.load_session("perf_session_0002").unwrap().unwrap()["messages"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            load_preferences(&fixture.directory).unwrap()["model"],
            "synthetic"
        );
    }

    #[test]
    fn current_database_opens_and_lists_while_another_connection_holds_a_write_lock() {
        let fixture = build_synthetic_store(3, 2);
        let database = fixture.directory.join(DATABASE_FILE);
        let writer = Connection::open(database).unwrap();
        let indexed_before: i64 = writer
            .query_row("SELECT COUNT(*) FROM indexed_sessions", [], |row| {
                row.get(0)
            })
            .unwrap();
        writer.execute_batch("BEGIN IMMEDIATE;").unwrap();

        let started = Instant::now();
        let store = LocalStore::open(&fixture.directory).unwrap();
        assert_eq!(store.list_sessions().unwrap().len(), 3);
        assert_eq!(
            store.setting("preferences").unwrap().as_deref(),
            Some(r#"{"model":"synthetic"}"#)
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "opening a current database waited for an unrelated writer"
        );
        let indexed_while_locked: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM indexed_sessions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(indexed_while_locked, indexed_before);

        writer.execute_batch("COMMIT;").unwrap();
        drop(store);
        let reopened = LocalStore::open(&fixture.directory).unwrap();
        let indexed_after: i64 = reopened
            .connection
            .query_row("SELECT COUNT(*) FROM indexed_sessions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(indexed_after > indexed_before);
    }

    #[test]
    fn version_four_database_without_session_index_upgrades_and_lists_in_order() {
        let fixture = build_synthetic_store(3, 1);
        let database = fixture.directory.join(DATABASE_FILE);
        let connection = Connection::open(database).unwrap();
        connection
            .execute_batch("DROP INDEX sessions_updated_at; PRAGMA user_version=4;")
            .unwrap();
        drop(connection);

        let upgraded = LocalStore::open(&fixture.directory).unwrap();
        let version: i64 = upgraded
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let has_index: bool = upgraded
            .connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sqlite_master
                    WHERE type='index' AND name='sessions_updated_at'
                )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let ids: Vec<_> = upgraded
            .list_sessions()
            .unwrap()
            .into_iter()
            .map(|session| session["id"].as_str().unwrap().to_owned())
            .collect();

        assert_eq!(version, SCHEMA_VERSION);
        assert!(has_index);
        assert_eq!(
            ids,
            [
                "perf_session_0002",
                "perf_session_0001",
                "perf_session_0000"
            ]
        );
    }

    /// Reported 800 x 300 baseline. Reproduce at full scale with
    /// BLACKWALL_PERF_SESSIONS=800 and BLACKWALL_PERF_MESSAGES=300.
    #[test]
    fn large_history_concurrent_startup_is_lock_free_and_list_is_bounded() {
        let session_count = perf_scale("BLACKWALL_PERF_SESSIONS", 12);
        let message_count = perf_scale("BLACKWALL_PERF_MESSAGES", 8);
        let list_budget =
            Duration::from_millis(perf_scale("BLACKWALL_PERF_LIST_BUDGET_MS", 2_000) as u64);
        let fixture = build_synthetic_store(session_count, message_count);
        let directory = Arc::new(fixture.directory.clone());
        let thread_count = 8;
        let barrier = Arc::new(Barrier::new(thread_count));
        let workers: Vec<_> = (0..thread_count)
            .map(|worker| {
                let directory = Arc::clone(&directory);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let result = if worker % 2 == 0 {
                        LocalStore::open(&directory)
                            .and_then(|store| store.list_sessions())
                            .map(|_| ())
                    } else {
                        load_preferences(&directory).map(|_| ())
                    };
                    result.map_err(|error| format!("{error:?}"))
                })
            })
            .collect();

        let failures: Vec<_> = workers
            .into_iter()
            .enumerate()
            .filter_map(|(worker, handle)| match handle.join() {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(format!("worker {worker}: {error}")),
                Err(_) => Some(format!("worker {worker}: panicked")),
            })
            .collect();
        let store = LocalStore::open(&directory).unwrap();
        let started = Instant::now();
        let summaries = store.list_sessions().unwrap();
        let elapsed = started.elapsed();

        assert!(
            failures.is_empty(),
            "concurrent startup storage calls failed (busy/locked errors reproduce the startup bug): {failures:#?}"
        );
        assert_eq!(summaries.len(), session_count.min(MAX_SESSIONS as usize));
        assert!(
            elapsed < list_budget,
            "list_sessions took {elapsed:?} for {session_count} sessions x {message_count} messages; budget is {list_budget:?}"
        );
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
            .execute_batch("DROP TABLE paired_devices; DROP TABLE memory; DROP TABLE memory_proposals; CREATE TABLE memory (id TEXT PRIMARY KEY, content TEXT NOT NULL, updated_at INTEGER NOT NULL); PRAGMA user_version=1;")
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
        assert_eq!(version, SCHEMA_VERSION);
        upgraded
            .connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        assert!(LocalStore::initialize(upgraded.connection).is_err());
    }
    #[test]
    fn a_new_database_is_marked_as_having_no_app_lock() {
        let root = std::env::temp_dir().join(format!(
            "blackwall-lock-marker-{}",
            crate::pairing::secret("bwd_")
        ));
        let fresh = root.join("fresh");
        drop(LocalStore::open(&fresh).unwrap());
        assert!(fresh.join(NO_LOCK_MARKER).exists());
        // An existing database is left as it is.
        fs::remove_file(fresh.join(NO_LOCK_MARKER)).unwrap();
        drop(LocalStore::open(&fresh).unwrap());
        assert!(!fresh.join(NO_LOCK_MARKER).exists());
        // A folder recorded as locked is never marked as unlocked.
        let locked = root.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join(LOCK_MARKER), b"").unwrap();
        drop(LocalStore::open(&locked).unwrap());
        assert!(!locked.join(NO_LOCK_MARKER).exists());
        fs::remove_dir_all(root).unwrap();
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
    fn session_summaries_retain_mode_and_project_without_loading_messages() {
        let db = store();
        let mut agent = session("agent");
        agent["mode"] = json!("agent");
        agent["workspace"] = json!("/projects/first");
        db.save_session(&agent).unwrap();
        db.save_session(&session("legacy")).unwrap();
        let summaries = db.list_sessions().unwrap();
        let saved_agent = summaries.iter().find(|item| item["id"] == "agent").unwrap();
        assert_eq!(saved_agent["mode"], "agent");
        assert_eq!(saved_agent["workspace"], "/projects/first");
        assert_eq!(saved_agent["messages"], json!([]));
        let legacy = summaries
            .iter()
            .find(|item| item["id"] == "legacy")
            .unwrap();
        assert_eq!(legacy["mode"], "chat");
        assert_eq!(legacy["workspace"], serde_json::Value::Null);
        assert_eq!(db.load_session("agent").unwrap(), Some(agent));
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
            ..Default::default()
        };
        db.save_memory(&entry).unwrap();
        assert_eq!(db.memories("concise").unwrap().len(), 1);
        entry = db.memories("").unwrap().remove(0);
        entry.content = "I prefer detailed answers".into();
        db.save_memory(&entry).unwrap();
        assert!(db.memories("concise").unwrap().is_empty());
        assert_eq!(db.memories("detailed").unwrap().len(), 1);
        db.delete_memory(&entry.id).unwrap();
        assert!(db.memories("detailed").unwrap().is_empty());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod context_tests {
    use super::*;
    use crate::context::{fingerprint, Checkpoint, ContextState, Summary};
    #[test]
    fn checkpoints_persist_separately_from_original_transcripts_and_invalid_updates_are_atomic() {
        let db = LocalStore::initialize(Connection::open_in_memory().unwrap()).unwrap();
        let messages = json!([{"role":"user","content":"Keep instructions","attachments":[]},{"role":"assistant","content":"Original response","attachments":[]},{"role":"user","content":"Continue","attachments":[]}]);
        let source = vec![
            json!({"role":"user","content":"Keep instructions"}),
            json!({"role":"assistant","content":"Original response"}),
            json!({"role":"user","content":"Continue"}),
        ];
        let mut state = ContextState::restore(&source, None).unwrap();
        state.checkpoints.push(Checkpoint {
            through: 2,
            source_hash: fingerprint(&source[..2]),
            summary: Summary {
                task_requirements: vec!["Keep instructions".into()],
                corrections: vec![],
                decisions: vec![],
                changed_files: vec![],
                checks: vec![],
                unfinished_work: vec!["Continue".into()],
                relevant_context: vec![],
            },
        });
        let session = json!({"id":"context_session","title":"Compacted","updatedAt":123,"messages":messages,"contextState":state});
        db.save_session(&session).unwrap();
        let loaded = db.load_session("context_session").unwrap().unwrap();
        assert_eq!(loaded["messages"], messages);
        assert_eq!(loaded["contextState"]["transcript"], json!(source));
        assert_eq!(
            loaded["contextState"]["checkpoints"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let mut invalid = session.clone();
        invalid["contextState"]["checkpoints"][0]["sourceHash"] = json!("tampered");
        assert!(db.save_session(&invalid).is_err());
        assert_eq!(
            db.load_session("context_session").unwrap().unwrap(),
            session
        );
    }
}
