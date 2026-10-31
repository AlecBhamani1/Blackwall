//! The v5 to v6 upgrade, run by `open_at(directory, 6)`. It is all or nothing:
//!
//! 1. Lock first (`BEGIN IMMEDIATE`, waiting up to [`UPGRADE_BUSY_TIMEOUT`]) and re-read the
//!    version; another window may have finished the upgrade while this one waited.
//! 2. Back up under the lock and verify the copy (see `backup`), or reuse the backup an earlier
//!    failed attempt made of this same source. `upgrade-state.json` counts the attempts; after
//!    [`MAX_ATTEMPTS`] the upgrade stops until the owner retries.
//! 3. In the same transaction: the legacy steps, the v6 schema, then every chat through
//!    `convert_session_in_tx` (one savepoint each; an invalid chat is set aside, not fatal).
//! 4. `foreign_key_check`, `user_version=6`, commit. Any SQL or I/O error before the commit rolls
//!    everything back: the database stays v5 and the error names the backup.
//! 5. After the commit: `integrity_check`, for detection only (a failure is a notice, and the file
//!    is then never vacuumed), then prune, then one `VACUUM`, best effort (R6-3). A VACUUM that
//!    cannot run now is left owed, in `settings`, and the next open runs it, after finishing any
//!    post-commit step a crash cut short.
//!
//! A database with no data yet takes the same steps without a backup or a VACUUM. A v6 open only
//! reads the version and whether a VACUUM is owed: it never takes the write lock otherwise.
use super::backup::{self, UpgradeState, BACKUPS_DIR, DELETE_BACKUPS_AFTER, MAX_ATTEMPTS};
use super::chats::{apply_v6_schema, write_tx};
use super::convert::{convert_session_in_tx, migrate_v6, ConvertOutcome};
use super::{
    configure, is_busy, LocalStore, StorageError, BUSY_TIMEOUT, DATABASE_FILE, NEWER_VERSION,
    UPGRADE_BUSY_TIMEOUT,
};
use rusqlite::{params, Connection, ErrorCode, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The schema version of the v6 event log.
pub(crate) const V6: i64 = 6;
/// The settings key holding the notices not yet shown, as a JSON array.
const NOTICES_KEY: &str = "storage_notices";
/// The settings key holding when the upgrade committed, in Unix milliseconds.
const SINCE_KEY: &str = "storage_v6_since";
/// The settings key present while the post-upgrade VACUUM is still owed (R6-3).
const VACUUM_PENDING_KEY: &str = "storage_vacuum_pending";
/// The longest a VACUUM waits for another window's lock before it is left for the next open.
const VACUUM_BUSY_TIMEOUT: Duration = Duration::from_secs(1);
const SET_SETTING: &str = "INSERT INTO settings(key,value) VALUES (?1,?2) \
     ON CONFLICT(key) DO UPDATE SET value=excluded.value";
const FOREIGN_KEYS_BROKEN: &str =
    "The upgraded chats failed a database safety check. Your existing data has been left in place.";
/// What the owner sees when the data folder or its backups folder is not private (the folder's
/// name is only logged).
const NOT_PRIVATE: &str = "Couldn't back up your chats, so nothing was changed. Blackwall's data \
     folder must be private to your user account: owned by you, and not writable by other users. \
     Fix its owner or permissions, then try again.";

/// Something the owner is told once, after startup. Kept in `settings` until taken.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum StorageNotice {
    /// Chats the upgrade or the browser import could not convert. They are kept whole, set aside.
    SetAside { chats: u32 },
    /// Messages that were given generated ids because theirs were missing or repeated.
    RenamedIds { messages: u32, chats: u32 },
    /// The check after the upgrade found damage. `backup` is the pre-upgrade copy to restore,
    /// or `None` for a database that had no data to back up.
    IntegrityFailed { backup: Option<String> },
    /// How much the upgrade deduplicated, and what it left inline and why (P2-6).
    #[serde(rename_all = "camelCase")]
    UpgradeStorageReport {
        deduped_bytes: u64,
        inline_bytes: u64,
        inline_reasons: BTreeMap<String, u64>,
    },
}

/// Where the upgrade stands, read without changing anything.
#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum UpgradeStatus {
    Current,
    Pending,
    /// Failed `attempts` times on this source; `stopped` once no attempt is made without Retry.
    Failed {
        backup: String,
        attempts: u32,
        stopped: bool,
    },
}

/// The verified pre-upgrade backup an upgrade runs behind.
struct Backup {
    dir: PathBuf,
    name: String,
}

impl Backup {
    fn path(&self) -> PathBuf {
        self.dir.join(&self.name)
    }
}

/// What the conversions add up to.
#[derive(Default)]
struct Summary {
    kept: u32,
    set_aside: u32,
    renamed_messages: u32,
    renamed_chats: u32,
    deduplicated_bytes: u64,
    inline_bytes: u64,
    inline_reasons: BTreeMap<String, u64>,
}

impl Summary {
    fn add(&mut self, outcome: ConvertOutcome) {
        match outcome {
            ConvertOutcome::Kept(report) => {
                self.kept += 1;
                if report.renamed_ids > 0 {
                    self.renamed_messages += report.renamed_ids;
                    self.renamed_chats += 1;
                }
                self.deduplicated_bytes += report.deduplicated_bytes;
                self.inline_bytes += report.inline_bytes;
                for (reason, count) in report.inline_reasons {
                    *self.inline_reasons.entry(reason.to_owned()).or_default() += u64::from(count);
                }
            }
            ConvertOutcome::SetAside { .. } => self.set_aside += 1,
        }
    }

    fn notices(&self) -> Vec<StorageNotice> {
        let mut notices = vec![];
        if self.set_aside > 0 {
            notices.push(StorageNotice::SetAside {
                chats: self.set_aside,
            });
        }
        if self.renamed_messages > 0 {
            notices.push(StorageNotice::RenamedIds {
                messages: self.renamed_messages,
                chats: self.renamed_chats,
            });
        }
        if self.kept > 0 {
            notices.push(StorageNotice::UpgradeStorageReport {
                deduped_bytes: self.deduplicated_bytes,
                inline_bytes: self.inline_bytes,
                inline_reasons: self.inline_reasons.clone(),
            });
        }
        notices
    }

    fn log(&self) {
        eprintln!(
            "info: upgrade: {} chats upgraded to v6, {} set aside; deduplicated_bytes={} inline_bytes={} inline_reasons={:?}",
            self.kept, self.set_aside, self.deduplicated_bytes, self.inline_bytes, self.inline_reasons
        );
    }
}

// ---- open ----

/// Opens `connection` at v6, upgrading the database first when it is older.
/// `retry` starts the attempt count again (the Retry button), under the write lock.
pub(super) fn open_v6(
    directory: &Path,
    connection: Connection,
    retry: bool,
) -> Result<LocalStore, StorageError> {
    let (busy, upgrade_busy) = busy_timeouts(directory);
    // `configure` already reads the schema, so it can be the step that finds the file busy.
    let read = |wait| configure(&connection, wait).and_then(|()| user_version(&connection));
    let version = match read(busy) {
        Err(error) if is_busy(&error) => match read(upgrade_busy) {
            Err(error) if is_busy(&error) => return Err(StorageError::UpgradeBusy),
            version => version?,
        },
        version => version?,
    };
    if version > V6 {
        return Err(StorageError::Invalid(NEWER_VERSION));
    }
    if version == V6 {
        // The cleanup a crash cut short runs first: it checks the file, so the VACUUM after it
        // never rewrites one that was not checked.
        finish_interrupted_cleanup(directory, &connection);
        vacuum_if_pending(directory, &connection, busy);
        prune_expired_once(directory, &connection);
    } else {
        connection.busy_timeout(upgrade_busy)?;
        let upgraded = upgrade(directory, &connection, retry);
        connection.busy_timeout(BUSY_TIMEOUT)?;
        upgraded?;
    }
    connection.busy_timeout(BUSY_TIMEOUT)?;
    Ok(LocalStore { connection })
}

/// The open's normal and upgrade lock waits: [`BUSY_TIMEOUT`] and [`UPGRADE_BUSY_TIMEOUT`],
/// unless a test set a short wait for this data folder.
fn busy_timeouts(directory: &Path) -> (Duration, Duration) {
    #[cfg(test)]
    if let Some(wait) = test_busy_timeout(directory) {
        return (wait, wait);
    }
    #[cfg(not(test))]
    let _ = directory;
    (BUSY_TIMEOUT, UPGRADE_BUSY_TIMEOUT)
}

#[cfg(test)]
static TEST_BUSY_TIMEOUTS: std::sync::Mutex<Vec<(PathBuf, Duration)>> =
    std::sync::Mutex::new(Vec::new());

/// Makes opens of `directory` wait at most `wait` for a lock (both waits), or restores the real
/// waits with `None`. Test-only and per data folder, like the faults.
#[cfg(test)]
pub(crate) fn set_test_busy_timeout(directory: &Path, wait: Option<Duration>) {
    let mut set = TEST_BUSY_TIMEOUTS.lock().unwrap_or_else(|e| e.into_inner());
    set.retain(|(path, _)| path != directory);
    if let Some(wait) = wait {
        set.push((directory.to_owned(), wait));
    }
}

#[cfg(test)]
fn test_busy_timeout(directory: &Path) -> Option<Duration> {
    TEST_BUSY_TIMEOUTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(path, _)| path == directory)
        .map(|(_, wait)| *wait)
}

fn user_version(connection: &Connection) -> Result<i64, StorageError> {
    Ok(connection.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

fn upgrade(directory: &Path, connection: &Connection, retry: bool) -> Result<(), StorageError> {
    #[cfg(test)]
    backup::fault(directory, backup::Fault::Locking)?;
    // Dropping `tx` on any early return rolls back.
    let tx = match write_tx(connection) {
        Err(error) if is_busy(&error) => return Err(StorageError::UpgradeBusy),
        tx => tx?,
    };
    let version = user_version(&tx)?;
    if version == V6 {
        eprintln!("info: upgrade: found v6 after waiting; another window upgraded the chats");
        return Ok(());
    }
    if version > V6 {
        return Err(StorageError::Invalid(NEWER_VERSION));
    }
    let now = SystemTime::now();
    let backup = if has_data(&tx, version)? {
        Some(back_up(directory, &tx, version, retry)?)
    } else {
        None
    };
    let failed = |source: StorageError| {
        eprintln!(
            "warn: upgrade: rolled back to the previous version (backup: {:?}): {source:?}",
            backup.as_ref().map(Backup::path)
        );
        StorageError::UpgradeFailed {
            backup: backup.as_ref().map(Backup::path),
            source: Box::new(source),
        }
    };
    let summary = convert_all(directory, &tx, version, millis(now)).map_err(failed)?;
    // Owed from the commit on, so a crash before it runs still leaves it for the next open. A
    // database that had no data has nothing to reclaim.
    if backup.is_some() {
        tx.execute(SET_SETTING, params![VACUUM_PENDING_KEY, "1"])
            .map_err(|error| failed(error.into()))?;
    }
    #[cfg(test)]
    {
        backup::fault(directory, backup::Fault::BeforeCommit).map_err(failed)?;
        backup::fault(
            directory,
            backup::Fault::Abort(backup::AbortAt::BeforeCommit),
        )
        .map_err(failed)?;
    }
    tx.commit().map_err(|error| failed(error.into()))?;
    #[cfg(test)]
    backup::fault(
        directory,
        backup::Fault::Abort(backup::AbortAt::AfterCommit),
    )?;
    summary.log();
    after_commit(directory, connection, backup.as_ref(), now);
    Ok(())
}

/// Whether there is anything to back up: any version from 1, or a `sessions` table at 0.
fn has_data(tx: &Connection, version: i64) -> Result<bool, StorageError> {
    if version >= 1 {
        return Ok(true);
    }
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='sessions')",
        [],
        |row| row.get(0),
    )?)
}

/// Makes (or reuses) the verified backup of the locked source and counts this attempt. The
/// caller holds the write lock on `tx`, so the source cannot change while it is identified and
/// copied, and the identity describes exactly what the backup holds.
///
/// `upgrade-state.json` is only ever written here, under that lock. On `retry` the saved attempt
/// count starts again at 0, so a stopped upgrade gets [`MAX_ATTEMPTS`] more tries.
fn back_up(
    directory: &Path,
    tx: &Connection,
    version: i64,
    retry: bool,
) -> Result<Backup, StorageError> {
    let dir = match backup::backups_dir(directory) {
        Err(StorageError::BackupFailed) => return Err(StorageError::Invalid(NOT_PRIVATE)),
        dir => dir?,
    };
    let database = directory.join(DATABASE_FILE);
    let source = backup::source_id(tx, &database)?;
    let saved = backup::read_state(&dir)
        .filter(|state| state.matches(&source, &dir))
        .map(|state| UpgradeState {
            attempts: if retry { 0 } else { state.attempts },
            ..state
        });
    let (made, attempts) = match saved {
        Some(state) if state.attempts >= MAX_ATTEMPTS => {
            let path = dir.join(state.backup);
            eprintln!(
                "warn: upgrade: stopped after {} attempts; the backup is {}",
                state.attempts,
                path.display()
            );
            return Err(StorageError::UpgradeStopped(path));
        }
        // The backup an earlier attempt made is checked again before it is trusted: something may
        // have changed it since (security L3). One that fails is passed over, as if no state
        // were saved, and a fresh backup is made.
        Some(state) if backup::still_verified(&dir.join(&state.backup), version) => (
            Backup {
                dir,
                name: state.backup,
            },
            state.attempts,
        ),
        _ => {
            // SQLite's backup refuses (SQLITE_BUSY) a source connection that is inside a write
            // transaction, so a second, read-only connection reads the file the lock holds still.
            let reader = Connection::open_with_flags(
                &database,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            configure(&reader, BUSY_TIMEOUT)?;
            let name = backup::create_backup(&reader, &dir, version)?;
            drop(reader);
            (Backup { dir, name }, 0)
        }
    };
    // From here the backup exists, so every error names it.
    let named = |source: StorageError| {
        eprintln!(
            "warn: upgrade: failed after its backup {}: {source:?}",
            made.path().display()
        );
        StorageError::UpgradeFailed {
            backup: Some(made.path()),
            source: Box::new(source),
        }
    };
    // Older backups are pruned only after the commit: a failed attempt never deletes one.
    let state = UpgradeState {
        backup: made.name.clone(),
        source_size: source.size,
        source_mtime_ms: source.mtime_ms,
        source_hash: source.source_hash,
        attempts: attempts + 1,
    };
    backup::write_state(&made.dir, &state).map_err(named)?;
    #[cfg(test)]
    {
        backup::fault(directory, backup::Fault::Slow(Duration::ZERO)).map_err(named)?;
        backup::fault(
            directory,
            backup::Fault::Abort(backup::AbortAt::AfterBackup),
        )
        .map_err(named)?;
    }
    Ok(made)
}

/// Everything that turns the locked database into v6, inside the caller's transaction.
#[cfg_attr(not(test), allow(unused_variables))] // `directory` keys the test faults
fn convert_all(
    directory: &Path,
    tx: &Connection,
    version: i64,
    now_ms: i64,
) -> Result<Summary, StorageError> {
    LocalStore::legacy_schema(tx, version)?;
    apply_v6_schema(tx)?;
    let chats: Vec<(String, String, i64)> = {
        let mut statement =
            tx.prepare("SELECT id, title, updated_at FROM sessions ORDER BY updated_at DESC")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut summary = Summary::default();
    let mut body = tx.prepare("SELECT body FROM sessions WHERE id=?1")?;
    // The folder's fault drives the conversion's own, which is per thread: the opener's.
    #[cfg(test)]
    let force_mismatch = backup::fault(directory, backup::Fault::ForceMismatch).is_err();
    // `_index` numbers the conversions for the test faults.
    #[cfg_attr(not(test), allow(clippy::unused_enumerate_index))]
    for (_index, (id, title, updated_at)) in chats.iter().enumerate() {
        // One body at a time, so the upgrade never holds every chat in memory.
        let raw: String = body.query_row([id], |row| row.get(0))?;
        #[cfg(test)]
        if force_mismatch {
            super::convert::set_fault(Some(super::convert::Fault::ForceMismatch));
        }
        let outcome = convert_session_in_tx(tx, id, title, *updated_at, &raw, now_ms);
        #[cfg(test)]
        if force_mismatch {
            super::convert::set_fault(None);
        }
        let outcome = outcome?;
        #[cfg(test)]
        {
            backup::fault(directory, backup::Fault::ConvertError(_index))?;
            backup::fault(
                directory,
                backup::Fault::Abort(backup::AbortAt::DuringConvert),
            )?;
        }
        summary.add(outcome);
    }
    record_notices(tx, &summary.notices())?;
    tx.execute(SET_SETTING, params![SINCE_KEY, now_ms.to_string()])?;
    let broken = tx
        .prepare("PRAGMA foreign_key_check")?
        .query([])?
        .next()?
        .is_some();
    // Every chat kept was given its key as it converted (R6-1).
    let keyless: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE key IS NULL)",
        [],
        |row| row.get(0),
    )?;
    if broken || keyless {
        return Err(StorageError::Invalid(FOREIGN_KEYS_BROKEN));
    }
    tx.pragma_update(None, "user_version", V6)?;
    Ok(summary)
}

/// Detection and cleanup once v6 is committed. Nothing here can undo the upgrade, so nothing here
/// fails the open: a problem is logged, or for a failed integrity check, made a notice.
fn after_commit(
    directory: &Path,
    connection: &Connection,
    backup: Option<&Backup>,
    now: SystemTime,
) {
    check_committed(
        directory,
        connection,
        backup.map(|backup| backup.path().display().to_string()),
    );
    if let Err(error) = clean_up(directory, connection, backup, now) {
        eprintln!("warn: upgrade: could not prune the backups: {error:?}");
    }
    // A database the check found damaged no longer owes the VACUUM, so it is left exactly as it
    // is, for the owner to restore.
    vacuum_if_pending(directory, connection, UPGRADE_BUSY_TIMEOUT);
}

/// The check after a committed upgrade, for detection only. On a failure it records the notice
/// and drops the owed VACUUM in one transaction, so a damaged file is never rewritten.
fn check_committed(directory: &Path, connection: &Connection, backup: Option<String>) {
    if integrity_ok(directory, connection) {
        return;
    }
    eprintln!("warn: upgrade: the integrity check after the upgrade found a problem");
    let recorded = write_tx(connection).and_then(|tx| {
        record_notices(&tx, &[StorageNotice::IntegrityFailed { backup }])?;
        tx.execute("DELETE FROM settings WHERE key=?1", [VACUUM_PENDING_KEY])?;
        Ok(tx.commit()?)
    });
    if let Err(error) = recorded {
        eprintln!("warn: upgrade: could not record the integrity notice: {error:?}");
    }
}

/// Whether the post-upgrade VACUUM is still owed.
fn vacuum_owed(connection: &Connection) -> Result<bool, StorageError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM settings WHERE key=?1)",
        [VACUUM_PENDING_KEY],
        |row| row.get(0),
    )?)
}

/// Runs the VACUUM the upgrade left owed, once, best effort (R6-3). It rewrites the file without
/// the pages the v5 bodies and the v5 search index freed. It never fails the open:
/// - an upgrade whose post-commit cleanup has not run yet (`upgrade-state.json` is still there):
///   left owed, because that cleanup checks the file first;
/// - less free disk than twice the database (VACUUM writes a whole temporary copy, then journals
///   the rewrite): left owed and logged;
/// - busy (another window holds a lock): left owed, for the next open;
/// - the disk filled, or its temporary file could not be made, while it ran (`SQLITE_FULL`,
///   `SQLITE_CANTOPEN`): left owed, so it runs again once there is room, on every OS. VACUUM is
///   atomic, so the database is as it was;
/// - any other error: logged and no longer owed, so a damaged file is not retried at every open.
///
/// Two windows can both find it owed. The second one re-reads the flag under the write lock,
/// and skips when the first has cleared it or has already left no free page; the only overlap
/// left is the instant between the first one's check and its VACUUM, where a second VACUUM costs
/// time and nothing else.
///
/// It never runs inside a transaction (SQLite refuses one there anyway). `restore` is the lock
/// wait the connection goes back to.
fn vacuum_if_pending(directory: &Path, connection: &Connection, restore: Duration) {
    match vacuum_owed(connection) {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            eprintln!("warn: storage: could not read whether a VACUUM is owed: {error:?}");
            return;
        }
    }
    if backup::has_state(&directory.join(BACKUPS_DIR)) {
        eprintln!("info: storage: the VACUUM waits for the upgrade's cleanup; left for later");
        return;
    }
    if !connection.is_autocommit() {
        eprintln!("warn: storage: the VACUUM was not run inside a transaction; left for later");
        return;
    }
    let size = connection
        .query_row(
            "SELECT page_count * page_size FROM pragma_page_count, pragma_page_size",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_or(0, |size| u64::try_from(size).unwrap_or(0));
    if let Some(free) = free_disk(directory) {
        let needed = size.saturating_mul(2);
        if free < needed {
            eprintln!(
                "info: storage: skipped the VACUUM: {free} bytes free, it needs about {needed}; left for a later open"
            );
            return;
        }
    }
    let wait = busy_timeouts(directory).0.min(VACUUM_BUSY_TIMEOUT);
    let started = std::time::Instant::now();
    let result = connection
        .busy_timeout(wait)
        .map_err(StorageError::from)
        .and_then(|()| vacuum(directory, connection));
    if let Err(error) = connection.busy_timeout(restore) {
        eprintln!("warn: storage: could not restore the lock wait after the VACUUM: {error}");
    }
    match result {
        Ok(false) => return,
        Ok(true) => eprintln!(
            "info: storage: VACUUM of {size} bytes took {} ms",
            started.elapsed().as_millis()
        ),
        Err(error) if is_busy(&error) => {
            eprintln!("info: storage: the VACUUM is busy (another window is open); left for the next open");
            return;
        }
        Err(error) if out_of_room(&error) => {
            eprintln!("warn: storage: the VACUUM ran out of disk space; left for a later open: {error:?}");
            return;
        }
        Err(error) => {
            eprintln!("warn: storage: the VACUUM failed and will not be retried: {error:?}")
        }
    }
    if let Err(error) =
        connection.execute("DELETE FROM settings WHERE key=?1", [VACUUM_PENDING_KEY])
    {
        // Still owed, so the next open runs it again: a second VACUUM only costs time.
        eprintln!("warn: storage: could not clear the owed VACUUM: {error}");
    }
}

/// The VACUUM pass itself; `false` when another window has done it. Under the write lock it
/// re-reads the flag, then flushes the search index with FTS5 `optimize` before the rewrite.
///
/// `optimize` merges the index into one segment and drops what deleted rows left behind. Those
/// include the deletes of a v5 process still running across the upgrade: its `DELETE FROM
/// sessions` cascades through `chat_search_keys` to `chat_search` without the `optimize` a v6
/// delete runs (security L1). The gap left: such a delete after this pass stays in the index
/// until the next v6 delete of an indexed chat optimizes it.
fn vacuum(directory: &Path, connection: &Connection) -> Result<bool, StorageError> {
    let tx = write_tx(connection)?;
    if !vacuum_owed(&tx)? {
        return Ok(false);
    }
    let free: i64 = tx.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
    if free == 0 {
        // Another window's VACUUM ran and has not cleared the flag yet.
        tx.execute("DELETE FROM settings WHERE key=?1", [VACUUM_PENDING_KEY])?;
        tx.commit()?;
        return Ok(false);
    }
    tx.execute_batch("INSERT INTO chat_search(chat_search) VALUES ('optimize');")?;
    tx.commit()?;
    #[cfg(test)]
    if backup::fault(directory, backup::Fault::VacuumFull).is_err() {
        return Err(StorageError::Database(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            None,
        )));
    }
    #[cfg(not(test))]
    let _ = directory;
    connection.execute_batch("VACUUM")?;
    Ok(true)
}

/// The disk filled, or SQLite could not make a temporary file: worth trying again later.
fn out_of_room(error: &StorageError) -> bool {
    matches!(
        error,
        StorageError::Database(rusqlite::Error::SqliteFailure(error, _))
            if matches!(error.code, ErrorCode::DiskFull | ErrorCode::CannotOpen)
    )
}

/// Bytes free to this user on the data folder's volume, or `None` where it cannot be read.
/// Windows has no safe call for it (the crate forbids `unsafe`), so there the VACUUM just runs:
/// it is atomic, and one that runs out of space leaves the database as it was.
fn free_disk(directory: &Path) -> Option<u64> {
    #[cfg(test)]
    if let Some(free) = test_free_disk(directory) {
        return Some(free);
    }
    #[cfg(unix)]
    return rustix::fs::statvfs(directory)
        .ok()
        .map(|stat| stat.f_bavail.saturating_mul(stat.f_frsize));
    #[cfg(not(unix))]
    {
        let _ = directory;
        None
    }
}

#[cfg(test)]
static TEST_FREE_DISK: std::sync::Mutex<Vec<(PathBuf, u64)>> = std::sync::Mutex::new(Vec::new());

/// Makes the VACUUM of `directory` see `free` bytes free, or the real figure with `None`.
/// Test-only and per data folder, like the faults.
#[cfg(test)]
pub(crate) fn set_test_free_disk(directory: &Path, free: Option<u64>) {
    let mut set = TEST_FREE_DISK.lock().unwrap_or_else(|e| e.into_inner());
    set.retain(|(path, _)| path != directory);
    if let Some(free) = free {
        set.push((directory.to_owned(), free));
    }
}

#[cfg(test)]
fn test_free_disk(directory: &Path) -> Option<u64> {
    TEST_FREE_DISK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(path, _)| path == directory)
        .map(|(_, free)| *free)
}

#[cfg_attr(not(test), allow(unused_variables))] // `directory` keys the test faults
fn integrity_ok(directory: &Path, connection: &Connection) -> bool {
    #[cfg(test)]
    if backup::fault(directory, backup::Fault::IntegrityCheck).is_err() {
        return false;
    }
    connection
        .pragma_query_value(None, "integrity_check", |row| row.get::<_, String>(0))
        .is_ok_and(|check| check == "ok")
}

/// Under the write lock: prune the backups (keeping this upgrade's) and delete the state file.
fn clean_up(
    directory: &Path,
    connection: &Connection,
    backup: Option<&Backup>,
    now: SystemTime,
) -> Result<(), StorageError> {
    let dir = match backup {
        Some(backup) => backup.dir.clone(),
        None if fs::symlink_metadata(directory.join(BACKUPS_DIR)).is_ok() => {
            backup::backups_dir(directory)?
        }
        None => return Ok(()),
    };
    // Held only for the lock; dropping it rolls back.
    let _lock = write_tx(connection)?;
    backup::prune(
        &dir,
        backup.map(|backup| backup.name.as_str()),
        now,
        Some(now),
        DELETE_BACKUPS_AFTER,
    )?;
    backup::clear_state(&dir)
}

/// A state file at v6 means an upgrade committed and then stopped (a crash) before its post-commit
/// steps: finish them. The integrity check first, as `after_commit` runs it (a failure is a
/// notice, and drops the owed VACUUM), then the pruning under the write lock, keeping the backup
/// the state names. Only a `stat` on the usual lock-free open; a failure is logged and the next
/// open tries again.
fn finish_interrupted_cleanup(directory: &Path, connection: &Connection) {
    let dir = directory.join(BACKUPS_DIR);
    if !backup::has_state(&dir) {
        return;
    }
    let current = backup::read_state(&dir).map(|state| state.backup);
    check_committed(
        directory,
        connection,
        current
            .as_ref()
            .map(|name| dir.join(name).display().to_string()),
    );
    let finished = backup::backups_dir(directory).and_then(|dir| {
        let _lock = write_tx(connection)?;
        backup::prune(&dir, current.as_deref(), SystemTime::now(), None, None)?;
        backup::clear_state(&dir)
    });
    if let Err(error) = finished {
        eprintln!("warn: upgrade: could not finish an interrupted cleanup: {error:?}");
    }
}

/// With [`DELETE_BACKUPS_AFTER`] set, a v6 open deletes expired backups, once per process.
fn prune_expired_once(directory: &Path, connection: &Connection) {
    static PRUNED: OnceLock<()> = OnceLock::new();
    let Some(max_age) = DELETE_BACKUPS_AFTER else {
        return;
    };
    PRUNED.get_or_init(|| {
        if let Err(error) = prune_expired(directory, connection, max_age) {
            eprintln!("warn: upgrade: could not delete expired backups: {error:?}");
        }
    });
}

// TODO(#51 age-prune): before `DELETE_BACKUPS_AFTER` is turned on, (1) take the write lock
// around this prune as the post-commit prune does, and (2) key the once-per-process guard by data
// folder: the single `OnceLock` in `prune_expired_once` skips every folder after the first.
fn prune_expired(
    directory: &Path,
    connection: &Connection,
    max_age: Duration,
) -> Result<(), StorageError> {
    if fs::symlink_metadata(directory.join(BACKUPS_DIR)).is_err() {
        return Ok(());
    }
    let since: Option<String> = connection
        .query_row(
            "SELECT value FROM settings WHERE key=?1",
            [SINCE_KEY],
            |row| row.get(0),
        )
        .optional()?;
    let Some(since) = since.and_then(|since| since.parse::<u64>().ok()) else {
        return Ok(());
    };
    let upgraded_at = UNIX_EPOCH + Duration::from_millis(since);
    let dir = backup::backups_dir(directory)?;
    backup::prune(
        &dir,
        None,
        SystemTime::now(),
        Some(upgraded_at),
        Some(max_age),
    )?;
    Ok(())
}

fn millis(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |since| {
        i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
    })
}

// ---- notices ----

fn read_notices(connection: &Connection) -> Result<Vec<StorageNotice>, StorageError> {
    let saved: Option<String> = connection
        .query_row(
            "SELECT value FROM settings WHERE key=?1",
            [NOTICES_KEY],
            |row| row.get(0),
        )
        .optional()?;
    Ok(saved
        .map(|text| serde_json::from_str(&text))
        .transpose()?
        .unwrap_or_default())
}

/// Adds `notices` to those not yet shown, inside the caller's write transaction.
fn record_notices(connection: &Connection, notices: &[StorageNotice]) -> Result<(), StorageError> {
    if notices.is_empty() {
        return Ok(());
    }
    let mut all = read_notices(connection)?;
    all.extend_from_slice(notices);
    connection.execute(
        SET_SETTING,
        params![NOTICES_KEY, serde_json::to_string(&all)?],
    )?;
    Ok(())
}

/// The browser import on a v6 store. Chats it sets aside become a notice (F24).
pub(super) fn migrate(store: &mut LocalStore, sessions: &[Value]) -> Result<(), StorageError> {
    let set_aside = migrate_v6(store, sessions)?;
    if set_aside > 0 {
        let tx = write_tx(&store.connection)?;
        record_notices(&tx, &[StorageNotice::SetAside { chats: set_aside }])?;
        tx.commit()?;
    }
    Ok(())
}

/// A read-only connection met a hot journal it may not roll back.
fn needs_rollback(error: &StorageError) -> bool {
    matches!(
        error,
        StorageError::Database(rusqlite::Error::SqliteFailure(error, _))
            if error.extended_code == rusqlite::ffi::SQLITE_READONLY_ROLLBACK
    )
}

/// The state file as it stands, when the source cannot be compared with it.
fn unmatched_status(dir: &Path) -> UpgradeStatus {
    backup::read_state(dir).map_or(UpgradeStatus::Pending, |state| failed_status(dir, &state))
}

fn failed_status(dir: &Path, state: &UpgradeState) -> UpgradeStatus {
    if state.attempts == 0 {
        return UpgradeStatus::Pending;
    }
    UpgradeStatus::Failed {
        backup: dir.join(&state.backup).display().to_string(),
        attempts: state.attempts,
        stopped: state.attempts >= MAX_ATTEMPTS,
    }
}

impl LocalStore {
    /// The notices not yet shown, removed as they are read: each one is shown once.
    pub fn take_storage_notices(&self) -> Result<Vec<StorageNotice>, StorageError> {
        // Most opens have none: only then is the write lock worth taking.
        if read_notices(&self.connection)?.is_empty() {
            return Ok(vec![]);
        }
        let tx = write_tx(&self.connection)?;
        let notices = read_notices(&tx)?;
        tx.execute("DELETE FROM settings WHERE key=?1", [NOTICES_KEY])?;
        tx.commit()?;
        Ok(notices)
    }

    /// Where the upgrade of the database in `directory` stands. It reads the version and
    /// `upgrade-state.json`; it never upgrades. Its only write lock is a zero-wait probe that it
    /// rolls straight back.
    ///
    /// - A window holding the write lock may be upgrading right now: `Pending`, never `Failed`.
    /// - A crashed upgrade leaves a hot `-journal` that a read-only connection cannot roll back, so
    ///   the source cannot be read and matched: the state file is reported as it stands. The next
    ///   open rolls the journal back.
    pub fn upgrade_status(directory: &Path) -> Result<UpgradeStatus, StorageError> {
        let path = directory.join(DATABASE_FILE);
        if fs::symlink_metadata(&path).is_err() {
            return Ok(UpgradeStatus::Pending);
        }
        let dir = directory.join(BACKUPS_DIR);
        let reader = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // `configure` reads the schema, so it shares the version read's outcomes.
        match configure(&reader, Duration::ZERO).and_then(|()| user_version(&reader)) {
            Ok(version) if version >= V6 => return Ok(UpgradeStatus::Current),
            Ok(_) => {}
            Err(error) if is_busy(&error) => return Ok(UpgradeStatus::Pending),
            Err(error) if needs_rollback(&error) => return Ok(unmatched_status(&dir)),
            Err(error) => return Err(error),
        }
        // A writer that holds the lock without a journal yet (an upgrade before its first write)
        // is invisible to readers; only taking the lock shows it.
        let probe = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        configure(&probe, Duration::ZERO)?;
        match probe
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
            .map_err(StorageError::from)
        {
            Err(error) if is_busy(&error) => return Ok(UpgradeStatus::Pending),
            result => result?,
        }
        drop(probe);
        let mut journal = path.as_os_str().to_owned();
        journal.push("-journal");
        if fs::symlink_metadata(PathBuf::from(journal)).is_ok() {
            return Ok(unmatched_status(&dir));
        }
        let Some(state) = backup::read_state(&dir) else {
            return Ok(UpgradeStatus::Pending);
        };
        // The identity the upgrade compares, read the same way: through SQLite, in one read
        // transaction, never by opening the file (see `backup::source_id`). It reads every chat,
        // so it is computed only here, with a state file to compare it with.
        let identity = reader
            .unchecked_transaction()
            .map_err(StorageError::from)
            .and_then(|read| {
                let source = backup::source_id(&read, &path)?;
                read.commit()?;
                Ok(source)
            });
        Ok(match identity {
            Ok(source) if state.matches(&source, &dir) => failed_status(&dir, &state),
            Ok(_) => UpgradeStatus::Pending,
            Err(error) if is_busy(&error) => UpgradeStatus::Pending,
            Err(error) if needs_rollback(&error) => unmatched_status(&dir),
            Err(error) => return Err(error),
        })
    }

    /// The Retry button: the database opens at v6, and the upgrade that runs starts its attempt
    /// count again at 0.
    pub fn retry_upgrade(directory: &Path) -> Result<Self, StorageError> {
        // The count is reset inside the upgrade, under the write lock (never here, where another
        // window's upgrade could be writing the same state file).
        Self::open_with(directory, V6, true)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "upgrade_tests.rs"]
mod tests;
