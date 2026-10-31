//! Verified pre-upgrade backups and the `upgrade-state.json` that stops the upgrade from looping.
//!
//! Everything here treats the data folder as hostile ground: no link is followed, a file is only
//! created exclusively, and pruning touches nothing but exact-pattern backup files. On unix the
//! folder and files are private (0700 and 0600). Windows has no mode bits or `O_NOFOLLOW`, so
//! there a symlink or reparse point is refused by checking `symlink_metadata` first.
use super::StorageError;
use rusqlite::{
    backup::{Backup, StepResult},
    types::ValueRef,
    Connection, OpenFlags,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// The folder in the data folder that holds the backups and the upgrade state.
pub(crate) const BACKUPS_DIR: &str = "backups";
/// How many of the newest backups pruning keeps (and the current one besides).
pub(crate) const KEEP_BACKUPS: usize = 3;
/// How many failed upgrades of one unchanged source are retried before the app stops trying.
pub(crate) const MAX_ATTEMPTS: u32 = 3;
/// Backups are deleted this long after the upgrade ran fine. Off until the owner decides
/// (proposed: `Some(30 days)`).
pub(crate) const DELETE_BACKUPS_AFTER: Option<Duration> = None;

const STATE_FILE: &str = "upgrade-state.json";
const MAX_STATE_BYTES: u64 = 4096;
const NAME_PREFIX: &str = "blackwall.sqlite3.pre-v6-";
const BACKUP_SUFFIX: &str = ".bak";
const PARTIAL_SUFFIX: &str = ".partial";
/// SQLite's journal and WAL sidecars for the partial file.
const SIDECARS: [&str; 3] = ["-journal", "-wal", "-shm"];
/// How long a backup keeps retrying a source that stays busy or locked before it fails.
#[cfg(not(test))]
const BACKUP_DEADLINE: Duration = Duration::from_secs(30);
#[cfg(test)]
const BACKUP_DEADLINE: Duration = Duration::from_millis(200);
/// The pause between retries of a busy or locked step.
const BACKUP_RETRY_PAUSE: Duration = Duration::from_millis(10);

/// Which backup an unfinished upgrade made, for which source, and how many tries it has had.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UpgradeState {
    pub backup: String,
    pub source_size: u64,
    pub source_mtime_ms: i64,
    pub source_hash: String,
    pub attempts: u32,
}

/// What identifies a source database: its size, its mtime and a hash of its logical content
/// (see [`source_id`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceId {
    pub size: u64,
    pub mtime_ms: i64,
    pub source_hash: String,
}

// ---- links, modes and no-follow opens ----

/// A symlink, or on Windows any reparse point (junctions and mount points included).
fn is_link(meta: &Metadata) -> bool {
    meta.file_type().is_symlink() || is_reparse(meta)
}

#[cfg(windows)]
fn is_reparse(meta: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse(_: &Metadata) -> bool {
    false
}

/// A plain file: not a link, a directory, a pipe or a device.
fn is_plain_file(meta: &Metadata) -> bool {
    meta.file_type().is_file() && !is_link(meta)
}

/// Whether `path` is a plain file, by `symlink_metadata`, which never follows.
fn plain_file_at(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| is_plain_file(&meta))
}

/// `O_NOFOLLOW` so a link at the last component fails, and `O_NONBLOCK` so a pipe put in a
/// file's place cannot hang the open (it is refused by the type check afterwards).
#[cfg(unix)]
fn no_follow(options: &mut OpenOptions) -> &mut OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
}

#[cfg(not(unix))]
fn no_follow(options: &mut OpenOptions) -> &mut OpenOptions {
    options
}

/// Windows has no `O_NOFOLLOW`: look at the link first. Unix has the real thing in `no_follow`.
fn refuse_link(path: &Path) -> io::Result<()> {
    if cfg!(windows) {
        if let Ok(meta) = fs::symlink_metadata(path) {
            if is_link(&meta) {
                return Err(io::Error::other("refusing to follow a link"));
            }
        }
    }
    Ok(())
}

/// Exclusively creates a new private file for writing (0600 on unix), never through a link.
fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    refuse_link(path)?;
    no_follow(&mut options).open(path)
}

/// Opens an existing plain file, never through a link. `write` asks for write access, which a
/// final `sync_all` needs on Windows.
fn open_plain(path: &Path, write: bool) -> io::Result<File> {
    refuse_link(path)?;
    let mut options = OpenOptions::new();
    options.read(!write).write(write);
    let file = no_follow(&mut options).open(path)?;
    if !is_plain_file(&file.metadata()?) {
        return Err(io::Error::other("not a plain file"));
    }
    Ok(file)
}

/// Makes the rename or creation just done durable. Windows cannot open a directory to sync it.
fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

// ---- cleanup of what this run created ----

/// Removes a file this run created, and (for a backup's partial) SQLite's sidecars for it, when
/// dropped, including on a panic. Armed only after the create succeeded, so a name that was
/// already there is never touched. `keep` disarms it.
struct Created {
    path: PathBuf,
    armed: bool,
}

impl Created {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            armed: true,
        }
    }
    fn keep(mut self) {
        self.armed = false;
    }
}

impl Drop for Created {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
            for sidecar in SIDECARS {
                let mut path = self.path.as_os_str().to_owned();
                path.push(sidecar);
                let _ = fs::remove_file(PathBuf::from(path));
            }
        }
    }
}

// ---- the folder ----

/// `<data>/backups`: a real directory (never a link), private and ours. On unix the data folder
/// itself must be owned by this user and closed to group and other writers (anyone who can write
/// there could swap `backups` for a link between our checks and our opens), and `backups` is
/// created 0700 or, if it exists, must be exactly 0700 and owned by this user.
pub(crate) fn backups_dir(data: &Path) -> Result<PathBuf, StorageError> {
    #[cfg(unix)]
    {
        let meta = fs::symlink_metadata(data)?;
        if !meta.file_type().is_dir() || !data_folder_is_private(&meta, euid()) {
            eprintln!(
                "warn: backup: the data folder {} must be a real folder that is private to this user \
                 (owned by you, not writable by group or others). Fix its owner or permissions, then try again.",
                data.display()
            );
            return Err(StorageError::BackupFailed);
        }
    }
    let dir = data.join(BACKUPS_DIR);
    match fs::symlink_metadata(&dir) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_private_dir(&dir)?,
        Err(error) => return Err(error.into()),
    }
    let meta = fs::symlink_metadata(&dir)?;
    if !meta.file_type().is_dir() || is_link(&meta) {
        return Err(StorageError::BackupFailed);
    }
    #[cfg(unix)]
    if !backups_folder_is_private(&meta, euid()) {
        eprintln!(
            "warn: backup: the backups folder {} must be private to this user (owned by you, mode 0700).",
            dir.display()
        );
        return Err(StorageError::BackupFailed);
    }
    Ok(dir)
}

/// The effective user id. `rustix` has it safely; the workspace forbids `unsafe`.
#[cfg(unix)]
fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Owned by `euid` and not writable by group or other.
#[cfg(unix)]
fn data_folder_is_private(meta: &Metadata, euid: u32) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    meta.uid() == euid && meta.permissions().mode() & 0o022 == 0
}

/// Owned by `euid` and exactly 0700.
#[cfg(unix)]
fn backups_folder_is_private(meta: &Metadata, euid: u32) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    meta.uid() == euid && meta.permissions().mode() & 0o7777 == 0o700
}

fn create_private_dir(dir: &Path) -> Result<(), StorageError> {
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = fs::DirBuilder::new();
    match builder.create(dir) {
        Ok(()) => {}
        // Lost a race to another opener; the checks that follow judge what it made.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    {
        // The umask may have cut the mode; it can never be allowed to widen it.
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

// ---- the source ----

/// Tables rebuilt from `sessions`, skipped by [`source_id`]: v5's recall index
/// (`session_messages`, which the deferred v5 backfill rewrites on any open, `indexed_sessions`,
/// and the shadow tables of `message_search`, its external-content FTS). They hold nothing a
/// restore needs, and the upgrade drops them.
const DERIVED_TABLES: [&str; 6] = [
    "session_messages",
    "indexed_sessions",
    "message_search_data",
    "message_search_idx",
    "message_search_docsize",
    "message_search_config",
];

/// What identifies a source database, read through SQLite on `connection`.
///
/// NEVER open the database file outside SQLite. On unix, closing ANY descriptor of a file drops
/// every POSIX lock the process holds on it ("How To Corrupt An SQLite Database", 2.2), and the
/// app keeps other connections to this file in the same process (v5 opens one per command; a
/// run's writer can be active while Settings asks for the upgrade status). So nothing here opens
/// the file: the size and mtime come from `stat`, which opens no descriptor, and the rest is SQL.
///
/// `source_hash` covers everything a restore would bring back, table by table, so a new table
/// cannot fall through:
/// - `user_version`, `page_count` and `freelist_count`;
/// - the content of every table, by [`hash_tables`].
///
/// [`DERIVED_TABLES`] are skipped at the content level only: a write to them still moves
/// `page_count`, `freelist_count` and the size, so it can still change the identity. That errs
/// the safe way (at worst one more verified backup).
///
/// The caller reads it inside one transaction so the parts agree; the upgrade, under its write
/// lock, so it describes exactly what the backup copies.
pub(crate) fn source_id(
    connection: &Connection,
    database: &Path,
) -> Result<SourceId, StorageError> {
    let meta = fs::metadata(database)?;
    if !meta.is_file() {
        return Err(StorageError::BackupFailed);
    }
    let mut hash = Sha256::new();
    for pragma in ["user_version", "page_count", "freelist_count"] {
        let value: i64 = connection.pragma_query_value(None, pragma, |row| row.get(0))?;
        hash.update(format!("{pragma}={value};"));
    }
    hash_tables(connection, &mut hash)?;
    Ok(SourceId {
        size: meta.len(),
        mtime_ms: meta.modified().map(millis).unwrap_or(0),
        source_hash: hex(&hash.finalize()),
    })
}

/// Adds the content of every table in `main` to `hash`:
/// - `sessions`: each row's id, `updated_at` and body length, ordered by id. Every v5 chat write
///   (save, delete, import) changes an `updated_at` or the set of ids. Saves stamp `updatedAt` in
///   milliseconds, so two saves in one millisecond can share one; the body length tells those
///   apart for free, because `octet_length` reads it from the row header without loading the
///   body.
/// - every other ordinary or shadow table (settings, memory, proposals, paired devices, FTS
///   shadow tables): every row in full, in rowid order, or primary-key order for a WITHOUT ROWID
///   table. Not the `sqlite_%` internals, not virtual tables (`pragma_table_list` type
///   `virtual`; their rows live in their shadow tables, which are hashed), and not
///   [`DERIVED_TABLES`].
fn hash_tables(connection: &Connection, hash: &mut Sha256) -> Result<(), StorageError> {
    let tables: Vec<String> = connection
        .prepare(
            "SELECT name FROM pragma_table_list
             WHERE schema='main' AND type IN ('table','shadow')
               AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'
             ORDER BY name",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for table in tables {
        if DERIVED_TABLES.contains(&table.as_str()) {
            continue;
        }
        let query = if table == "sessions" {
            "SELECT id, updated_at, octet_length(body) FROM main.sessions ORDER BY id".to_owned()
        } else {
            format!(
                "SELECT * FROM main.{} ORDER BY {}",
                quoted(&table),
                row_order(connection, &table)?
            )
        };
        hash.update(format!("table{}:", table.len()));
        hash.update(&table);
        let mut statement = connection.prepare(&query)?;
        let columns = statement.column_count();
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            hash.update(b"row");
            for column in 0..columns {
                feed(hash, row.get_ref(column)?);
            }
        }
    }
    Ok(())
}

/// `rowid`, or for a WITHOUT ROWID table its primary-key columns in key order.
fn row_order(connection: &Connection, table: &str) -> Result<String, StorageError> {
    let without_rowid: bool = connection.query_row(
        "SELECT wr FROM pragma_table_list WHERE schema='main' AND name=?1",
        [table],
        |row| row.get(0),
    )?;
    if !without_rowid {
        return Ok("rowid".into());
    }
    let keys: Vec<String> = connection
        .prepare("SELECT name FROM pragma_table_info(?1,'main') WHERE pk>0 ORDER BY pk")?
        .query_map([table], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(keys
        .iter()
        .map(|key| quoted(key))
        .collect::<Vec<_>>()
        .join(","))
}

/// An SQL identifier, quoted.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Adds one value to the hash, tagged by type and length-prefixed, so no two rows run together.
fn feed(hash: &mut Sha256, value: ValueRef<'_>) {
    match value {
        ValueRef::Null => hash.update(b"n"),
        ValueRef::Integer(n) => hash.update(format!("i{n};")),
        ValueRef::Real(x) => hash.update(format!("r{};", x.to_bits())),
        ValueRef::Text(bytes) => {
            hash.update(format!("t{}:", bytes.len()));
            hash.update(bytes);
        }
        ValueRef::Blob(bytes) => {
            hash.update(format!("b{}:", bytes.len()));
            hash.update(bytes);
        }
    }
}

fn millis(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// ---- upgrade-state.json ----

/// The saved state, or `None` for anything wrong with it: missing, a link, not a plain file,
/// over 4 KiB, not exactly our fields, or naming something that is not one of our backups.
pub(crate) fn read_state(dir: &Path) -> Option<UpgradeState> {
    let file = open_plain(&dir.join(STATE_FILE), false).ok()?;
    let mut bytes = Vec::new();
    (&file)
        .take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return None;
    }
    let state: UpgradeState = serde_json::from_slice(&bytes).ok()?;
    parse_backup_name(&state.backup)?;
    Some(state)
}

/// Replaces the state atomically: an exclusive private temp file, synced, renamed over the old
/// one, then the folder synced. A link where the state file was is replaced, not followed.
pub(crate) fn write_state(dir: &Path, state: &UpgradeState) -> Result<(), StorageError> {
    let bytes = serde_json::to_vec(state)?;
    let temp = dir.join(format!("{STATE_FILE}.tmp-{:016x}", rand::random::<u64>()));
    let mut file = create_private(&temp)?;
    let cleanup = Created::new(&temp);
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, dir.join(STATE_FILE))?;
    cleanup.keep();
    sync_dir(dir)?;
    Ok(())
}

/// Whether a state file (or anything else) sits where it belongs. A `stat`, which opens nothing.
pub(crate) fn has_state(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join(STATE_FILE)).is_ok()
}

/// Removes the state once the upgrade has committed. A link there is removed, not followed.
pub(crate) fn clear_state(dir: &Path) -> Result<(), StorageError> {
    match fs::remove_file(dir.join(STATE_FILE)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

impl UpgradeState {
    /// Whether this state describes `source` and its backup is still there. The size and source
    /// hash decide; the mtime never does (it changes when nothing else has).
    pub(crate) fn matches(&self, source: &SourceId, dir: &Path) -> bool {
        self.source_size == source.size
            && self.source_hash == source.source_hash
            && parse_backup_name(&self.backup).is_some()
            && plain_file_at(&dir.join(&self.backup))
    }
}

// ---- names ----

/// `blackwall.sqlite3.pre-v6-<UTC yyyymmddThhmmssZ>-<8 hex>.bak`
pub(crate) fn backup_name(now: SystemTime, suffix: u32) -> String {
    format!(
        "{NAME_PREFIX}{}-{suffix:08x}{BACKUP_SUFFIX}",
        stamp(unix_seconds(now))
    )
}

/// The Unix time in a name made by [`backup_name`]; `None` for anything that is not exactly
/// that: another case, another length, an impossible date or a stray suffix.
pub(crate) fn parse_backup_name(name: &str) -> Option<i64> {
    let rest = name
        .strip_prefix(NAME_PREFIX)?
        .strip_suffix(BACKUP_SUFFIX)?;
    let (time, random) = rest.split_once('-')?;
    if !rest.is_ascii()
        || time.len() != 16
        || random.len() != 8
        || !random
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = time.get(range)?;
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    if time.as_bytes()[8] != b'T' || time.as_bytes()[15] != b'Z' {
        return None;
    }
    let (year, month, day) = (digits(0..4)?, digits(4..6)?, digits(6..8)?);
    let (hour, minute, second) = (digits(9..11)?, digits(11..13)?, digits(13..15)?);
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    // An impossible date (Feb 30, month 13) comes back as a different one.
    (stamp(seconds) == time).then_some(seconds)
}

fn unix_seconds(time: SystemTime) -> i64 {
    // Years past 9999 would not fit the name's four digits.
    const LAST: i64 = 253_402_300_799;
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(LAST).min(LAST),
        Err(_) => 0,
    }
}

fn stamp(seconds: i64) -> String {
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let in_day = seconds.rem_euclid(86_400);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        in_day / 3600,
        in_day % 3600 / 60,
        in_day % 60
    )
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted + 2) / 5 + 1;
    let month = if shifted < 10 {
        shifted + 3
    } else {
        shifted - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

// ---- making a backup ----

/// Copies `source` into a new verified `.bak` in `dir` and returns its file name. The copy is
/// made as a private `<name>.partial`, checked to be `expected_version` with a clean
/// `integrity_check`, synced and only then renamed. Any failure leaves no partial and no `.bak`.
pub(crate) fn create_backup(
    source: &Connection,
    dir: &Path,
    expected_version: i64,
) -> Result<String, StorageError> {
    create_named_backup(
        source,
        dir,
        &backup_name(SystemTime::now(), rand::random()),
        expected_version,
    )
}

fn create_named_backup(
    source: &Connection,
    dir: &Path,
    name: &str,
    expected_version: i64,
) -> Result<String, StorageError> {
    let finished = dir.join(name);
    let partial = dir.join(format!("{name}{PARTIAL_SUFFIX}"));
    match copy_and_verify(source, dir, &partial, &finished, expected_version) {
        Ok(()) => Ok(name.to_owned()),
        Err(error) => {
            eprintln!(
                "warn: backup: could not make a verified backup in {}: {error:?}",
                dir.display()
            );
            Err(StorageError::BackupFailed)
        }
    }
}

fn copy_and_verify(
    source: &Connection,
    dir: &Path,
    partial: &Path,
    finished: &Path,
    expected_version: i64,
) -> Result<(), StorageError> {
    create_private(partial)?;
    // From here the partial and its sidecars are ours, and go on any exit, even a panic.
    let _partial = Created::new(partial);
    #[cfg(test)]
    inject(dir, Fault::BackupWrite)?;
    // Not `SQLITE_OPEN_NOFOLLOW`: SQLite applies it to every path component, and a data folder
    // under a linked parent (macOS `/var`, a linked home) must still work. The file was just
    // created by us, exclusively and without following a link, and the folder is private.
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let mut copy = Connection::open_with_flags(partial, flags)?;
    super::configure(&copy, super::BUSY_TIMEOUT)?;
    let backup = Backup::new(source, &mut copy)?;
    #[cfg(test)]
    {
        inject(dir, Fault::BackupCopy)?;
        inject(dir, Fault::BackupPanic)?;
        inject(dir, Fault::Abort(AbortAt::DuringBackup))?;
    }
    copy_all_pages(&backup)?;
    drop(backup);
    // The copy inherits the source's journal mode; make the file self-contained.
    let mode: String =
        copy.pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("delete") {
        return Err(StorageError::BackupFailed);
    }
    copy.close().map_err(|(_, error)| error)?;
    // Verify the finished file as a fresh reader sees it, not through the writer's cache.
    let reader = Connection::open_with_flags(
        partial,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let version: i64 = reader.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let check: String = reader.pragma_query_value(None, "integrity_check", |row| row.get(0))?;
    #[cfg(test)]
    inject(dir, Fault::IntegrityCheck)?;
    reader.close().map_err(|(_, error)| error)?;
    if version != expected_version || check != "ok" {
        return Err(StorageError::BackupFailed);
    }
    open_plain(partial, true)?.sync_all()?;
    publish(dir, partial, finished)?;
    Ok(())
}

/// Copies every page, retrying a busy or locked step until [`BACKUP_DEADLINE`]. A source that
/// is still busy then (another writer, or a source connection inside a write transaction, which
/// SQLite reports as busy for as long as it lasts) fails the backup instead of spinning.
fn copy_all_pages(backup: &Backup<'_, '_>) -> Result<(), StorageError> {
    let deadline = Instant::now() + BACKUP_DEADLINE;
    loop {
        match backup.step(-1)? {
            StepResult::Done => return Ok(()),
            StepResult::More => {}
            _ if Instant::now() >= deadline => return Err(StorageError::BackupFailed),
            _ => std::thread::sleep(BACKUP_RETRY_PAUSE),
        }
    }
}

/// Moves the finished partial to its final name without ever replacing a file. A hard link fails
/// if `finished` exists, so it cannot overwrite. On a filesystem without hard links it falls
/// back to `rename` after checking that `finished` is absent. That fallback has a small race
/// (something could create `finished` between the check and the rename), accepted for the
/// filesystems that cannot link, because a failed backup would block the upgrade for good.
/// Any other link error is a failure.
fn publish(dir: &Path, partial: &Path, finished: &Path) -> io::Result<()> {
    #[cfg(test)]
    let no_links = inject(dir, Fault::NoHardLinks).is_err();
    #[cfg(not(test))]
    let no_links = false;
    let linked = if no_links {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    } else {
        fs::hard_link(partial, finished)
    };
    let drop_finished = |_: &io::Error| {
        let _ = fs::remove_file(finished);
    };
    match linked {
        Ok(()) => fs::remove_file(partial)
            .and_then(|()| sync_dir(dir))
            .inspect_err(drop_finished),
        Err(error) if links_unsupported(&error) => {
            if fs::symlink_metadata(finished).is_ok() {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            fs::rename(partial, finished)?;
            sync_dir(dir).inspect_err(drop_finished)
        }
        Err(error) => Err(error),
    }
}

/// Whether a failed `hard_link` means "this filesystem or volume cannot", not "this name is taken".
fn links_unsupported(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    let Some(code) = error.raw_os_error() else {
        return false;
    };
    #[cfg(unix)]
    let codes = [libc::EPERM, libc::ENOTSUP, libc::EOPNOTSUPP, libc::EXDEV];
    // ERROR_INVALID_FUNCTION (FAT, exFAT) and ERROR_NOT_SUPPORTED.
    #[cfg(windows)]
    let codes = [1, 50];
    codes.contains(&code)
}

/// Test fault hook, keyed by the data folder, which is the backups folder's parent.
#[cfg(test)]
fn inject(dir: &Path, point: Fault) -> Result<(), StorageError> {
    dir.parent().map_or(Ok(()), |data| fault(data, point))
}

// ---- pruning ----

/// Deletes the backups beyond the newest [`KEEP_BACKUPS`] by name timestamp, always keeping
/// `current`, and any stale `.partial` with its crash sidecars. With `max_age` set and `upgraded_at + max_age <= now`
/// it deletes every backup, `current` too. It touches only plain files whose names match the
/// exact pattern; everything else in the folder is left alone. Returns how many it deleted.
pub(crate) fn prune(
    dir: &Path,
    current: Option<&str>,
    now: SystemTime,
    upgraded_at: Option<SystemTime>,
    max_age: Option<Duration>,
) -> Result<usize, StorageError> {
    let meta = fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir() || is_link(&meta) {
        return Err(StorageError::BackupFailed);
    }
    let mut backups = Vec::new();
    let mut partials = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !plain_file_at(&entry.path()) {
            continue;
        }
        if let Some(stamp) = parse_backup_name(&name) {
            backups.push((stamp, name));
        } else if is_partial_or_sidecar(&name) {
            partials.push(name);
        }
    }
    backups.sort_by(|a, b| b.cmp(a));
    let expired = matches!(
        (upgraded_at, max_age),
        (Some(upgraded), Some(age)) if upgraded.checked_add(age).is_some_and(|due| due <= now)
    );
    let doomed = backups
        .into_iter()
        .enumerate()
        .filter(|(rank, (_, name))| expired || (*rank >= KEEP_BACKUPS && current != Some(name)))
        .map(|(_, (_, name))| name)
        .chain(partials);
    let mut removed = 0;
    for name in doomed {
        match fs::remove_file(dir.join(name)) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(removed)
}

/// `<exact backup name>.partial`, or SQLite's `-journal`, `-wal` or `-shm` file beside one,
/// which a crash can leave behind.
fn is_partial_or_sidecar(name: &str) -> bool {
    let partial = SIDECARS
        .iter()
        .find_map(|sidecar| name.strip_suffix(sidecar))
        .unwrap_or(name);
    partial
        .strip_suffix(PARTIAL_SUFFIX)
        .is_some_and(|whole| parse_backup_name(whole).is_some())
}

// ---- test faults ----

/// Where the tests make the upgrade fail, slow down or die. Test-only, and armed per data
/// folder so that threads upgrading different folders never see each other's faults.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    /// The backup fails after its partial file exists.
    BackupWrite,
    /// The backup fails while the SQLite copy is set up and about to run.
    BackupCopy,
    /// The same point, but the thread panics.
    BackupPanic,
    /// Hard links report "unsupported" when a backup is moved into its final name.
    NoHardLinks,
    /// The conversion with this number (from 0) hits an SQL error after writing its rows.
    ConvertError(usize),
    /// The whole-session check reports a difference.
    ForceMismatch,
    /// The upgrade fails just before it commits.
    BeforeCommit,
    /// The upgrade holds the write lock this long at the marked point.
    Slow(Duration),
    /// The backup's integrity check reports damage.
    IntegrityCheck,
    /// The post-upgrade VACUUM fails with `SQLITE_FULL`, as on a full disk.
    VacuumFull,
    /// The process dies at this point.
    Abort(AbortAt),
    /// At the upgrade's slow point (the backup is made, the write lock held) the process creates
    /// [`GATE_HELD`] in the data folder and waits until [`GATE_RELEASE`] exists there. Files, not
    /// sleeps, so a test that drives several processes can order them on any runner.
    Gate,
    /// The point just before the upgrade asks for the write lock. Under [`Fault::Gate`] the
    /// process marks it with a `locking-<pid>` file in the data folder.
    Locking,
}

/// The file the gated process creates once it is holding the upgrade's write lock.
#[cfg(test)]
pub(crate) const GATE_HELD: &str = "gate-held";
/// The file whose appearance lets the gated process go on.
#[cfg(test)]
pub(crate) const GATE_RELEASE: &str = "gate-release";
/// A gated process that is never released goes on after this, so a dead test cannot hang a runner.
#[cfg(test)]
const GATE_DEADLINE: Duration = Duration::from_secs(120);

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AbortAt {
    /// The backup is half copied: its `.partial` exists.
    DuringBackup,
    AfterBackup,
    DuringConvert,
    BeforeCommit,
    /// The upgrade is committed; the integrity check and the pruning have not run.
    AfterCommit,
}

#[cfg(test)]
static FAULTS: std::sync::Mutex<Vec<(PathBuf, Fault)>> = std::sync::Mutex::new(Vec::new());

/// Arms `fault` for the data folder (replacing any armed there), or clears it with `None`.
#[cfg(test)]
pub(crate) fn set_fault(data_directory: &Path, fault: Option<Fault>) {
    let mut armed = FAULTS.lock().unwrap_or_else(|e| e.into_inner());
    armed.retain(|(path, _)| path != data_directory);
    if let Some(fault) = fault {
        armed.push((data_directory.to_owned(), fault));
    }
}

/// Called at an injection point. Fails (or aborts, or sleeps) if the fault armed for this data
/// folder is `point` itself; `Slow` matches any duration, and sleeps the armed one.
#[cfg(test)]
pub(crate) fn fault(data_directory: &Path, point: Fault) -> Result<(), StorageError> {
    let armed = FAULTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(path, _)| path == data_directory)
        .map(|(_, fault)| *fault);
    match (armed, point) {
        (Some(Fault::Slow(pause)), Fault::Slow(_)) => {
            std::thread::sleep(pause);
            Ok(())
        }
        (Some(Fault::Gate), Fault::Locking) => {
            let _ = fs::write(
                data_directory.join(format!("locking-{}", std::process::id())),
                b"",
            );
            Ok(())
        }
        (Some(Fault::Gate), Fault::Slow(_)) => {
            let _ = fs::write(data_directory.join(GATE_HELD), b"");
            let deadline = Instant::now() + GATE_DEADLINE;
            while !data_directory.join(GATE_RELEASE).exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        }
        (Some(Fault::Abort(at)), Fault::Abort(here)) if at == here => std::process::abort(),
        (Some(Fault::BackupPanic), Fault::BackupPanic) => panic!("injected test panic"),
        (Some(armed), point) if armed == point => {
            Err(StorageError::Invalid("An injected test fault."))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

    /// A data folder holding a small v5-looking database, removed on drop.
    struct Data(PathBuf);
    impl Data {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("blackwall-backup-{:016x}", rand::random::<u64>()));
            fs::create_dir_all(&path).unwrap();
            // The data folder must be private whatever the umask (Ubuntu and Debian use 002).
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            let data = Self(path);
            let db = Connection::open(data.db()).unwrap();
            db.execute_batch(
                "CREATE TABLE t(x TEXT); INSERT INTO t VALUES ('kept'); PRAGMA user_version=5;",
            )
            .unwrap();
            data
        }
        fn db(&self) -> PathBuf {
            self.0.join(super::super::DATABASE_FILE)
        }
        fn source(&self) -> Connection {
            Connection::open(self.db()).unwrap()
        }
    }
    impl Drop for Data {
        fn drop(&mut self) {
            set_fault(&self.0, None);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }
    fn names(dir: &Path) -> Vec<String> {
        let mut all: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        all.sort();
        all
    }

    #[test]
    fn backup_is_verified_private_and_named_by_utc_time() {
        let data = Data::new();
        let dir = backups_dir(&data.0).unwrap();
        let before = SystemTime::now();
        let name = create_backup(&data.source(), &dir, 5).unwrap();
        let stamp = parse_backup_name(&name).unwrap();
        let secs = before
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(
            (secs..secs + 60).contains(&stamp),
            "{name} is not named for now"
        );
        assert_eq!(
            names(&dir),
            std::slice::from_ref(&name),
            "only the finished .bak remains"
        );
        let copy = Connection::open(dir.join(&name)).unwrap();
        let version: i64 = copy
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        let kept: String = copy.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!((version, kept.as_str()), (5, "kept"));
        drop(copy);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&dir.join(&name)), 0o600);
        }
        // The exact format.
        assert_eq!(
            backup_name(at(1_793_383_380), 0x1a2b_3c4d),
            "blackwall.sqlite3.pre-v6-20261030T180300Z-1a2b3c4d.bak"
        );
        assert_eq!(
            parse_backup_name("blackwall.sqlite3.pre-v6-20261030T180300Z-1a2b3c4d.bak"),
            Some(1_793_383_380)
        );
    }

    #[test]
    fn backup_of_the_wrong_version_is_refused_and_leaves_nothing() {
        let data = Data::new();
        let dir = backups_dir(&data.0).unwrap();
        let error = create_backup(&data.source(), &dir, 6).unwrap_err();
        assert!(matches!(error, StorageError::BackupFailed));
        assert!(names(&dir).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_or_shared_backups_folder_is_refused() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let refused = |data: &Data| {
            assert!(matches!(
                backups_dir(&data.0),
                Err(StorageError::BackupFailed)
            ));
        };
        let mode = |data: &Data, mode: u32| {
            fs::create_dir(data.0.join(BACKUPS_DIR)).unwrap();
            fs::set_permissions(data.0.join(BACKUPS_DIR), fs::Permissions::from_mode(mode))
                .unwrap();
        };
        let linked = Data::new();
        let elsewhere = Data::new();
        symlink(&elsewhere.0, linked.0.join(BACKUPS_DIR)).unwrap();
        refused(&linked);
        let shared = Data::new();
        mode(&shared, 0o755);
        refused(&shared);
        let loose = Data::new();
        mode(&loose, 0o770);
        refused(&loose);
        let file = Data::new();
        fs::write(file.0.join(BACKUPS_DIR), b"x").unwrap();
        refused(&file);
        // A private one is accepted, and a missing one is created.
        let good = Data::new();
        mode(&good, 0o700);
        assert!(backups_dir(&good.0).is_ok());
        assert!(backups_dir(&Data::new().0).is_ok());
    }

    #[test]
    fn a_failed_backup_leaves_no_partial_and_no_bak() {
        for point in [Fault::BackupWrite, Fault::BackupCopy, Fault::IntegrityCheck] {
            let data = Data::new();
            let dir = backups_dir(&data.0).unwrap();
            set_fault(&data.0, Some(point));
            let error = create_backup(&data.source(), &dir, 5).unwrap_err();
            assert!(matches!(error, StorageError::BackupFailed), "{point:?}");
            assert!(names(&dir).is_empty(), "{point:?} left {:?}", names(&dir));
            set_fault(&data.0, None);
            assert!(
                create_backup(&data.source(), &dir, 5).is_ok(),
                "clean after {point:?}"
            );
        }
    }

    #[test]
    fn a_source_that_stays_busy_fails_the_backup_instead_of_spinning() {
        let data = Data::new();
        let dir = backups_dir(&data.0).unwrap();
        // SQLite reports a source connection inside a write transaction as busy throughout.
        let source = data.source();
        source.execute_batch("BEGIN IMMEDIATE").unwrap();
        let started = Instant::now();
        let error = create_backup(&source, &dir, 5).unwrap_err();
        assert!(matches!(error, StorageError::BackupFailed), "{error:?}");
        assert!(started.elapsed() >= BACKUP_DEADLINE);
        assert!(names(&dir).is_empty(), "left {:?}", names(&dir));
        source.execute_batch("ROLLBACK").unwrap();
        assert!(create_backup(&source, &dir, 5).is_ok());
    }

    fn state(backup: &str) -> UpgradeState {
        UpgradeState {
            backup: backup.into(),
            source_size: 4096,
            source_mtime_ms: 1_000,
            source_hash: "ab".repeat(32),
            attempts: 1,
        }
    }
    const GOOD: &str = "blackwall.sqlite3.pre-v6-20261030T180300Z-1a2b3c4d.bak";

    #[test]
    fn state_is_strict_bounded_and_atomic() {
        let data = Data::new();
        let dir = backups_dir(&data.0).unwrap();
        let file = dir.join("upgrade-state.json");
        assert_eq!(read_state(&dir), None, "missing is absent");
        write_state(&dir, &state(GOOD)).unwrap();
        assert_eq!(read_state(&dir), Some(state(GOOD)));
        let mut second = state(GOOD);
        second.attempts = 2;
        write_state(&dir, &second).unwrap();
        assert_eq!(read_state(&dir), Some(second));
        assert_eq!(names(&dir), ["upgrade-state.json"], "no temp file is left");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o7777, 0o600);
        }
        let valid = serde_json::to_value(state(GOOD)).unwrap();
        let mut unknown = valid.clone();
        unknown["extra"] = true.into();
        let mut traversal = valid.clone();
        traversal["backup"] = "../../etc/passwd".into();
        let mut padded = valid.to_string().into_bytes();
        padded.extend(std::iter::repeat_n(b' ', 5000));
        let bad: [(&str, Vec<u8>); 6] = [
            ("unknown field", unknown.to_string().into_bytes()),
            ("path in the name", traversal.to_string().into_bytes()),
            ("over 4 KiB", padded),
            ("garbage", b"not json".to_vec()),
            ("trailing data", format!("{valid} {valid}").into_bytes()),
            ("empty", Vec::new()),
        ];
        for (why, bytes) in bad {
            fs::write(&file, bytes).unwrap();
            assert_eq!(read_state(&dir), None, "{why}");
        }
        fs::remove_file(&file).unwrap();
        fs::create_dir(&file).unwrap();
        assert_eq!(read_state(&dir), None, "a directory is not a state file");
        fs::remove_dir(&file).unwrap();
        #[cfg(unix)]
        {
            let target = data.0.join("elsewhere.json");
            fs::write(&target, serde_json::to_vec(&state(GOOD)).unwrap()).unwrap();
            std::os::unix::fs::symlink(&target, &file).unwrap();
            assert_eq!(read_state(&dir), None, "a symlink is not followed");
            // Writing replaces the link itself and leaves its target alone.
            write_state(&dir, &state(GOOD)).unwrap();
            assert!(!fs::symlink_metadata(&file).unwrap().is_symlink());
            assert_eq!(read_state(&dir), Some(state(GOOD)));
        }
    }

    #[test]
    fn state_matches_on_size_and_source_hash_not_mtime() {
        let data = Data::new();
        let dir = backups_dir(&data.0).unwrap();
        let backup = create_backup(&data.source(), &dir, 5).unwrap();
        let id = source_id(&data.source(), &data.db()).unwrap();
        assert_eq!(id.size, fs::metadata(data.db()).unwrap().len());
        assert_eq!(id.source_hash.len(), 64);
        let saved = UpgradeState {
            backup: backup.clone(),
            source_size: id.size,
            source_mtime_ms: id.mtime_ms,
            source_hash: id.source_hash.clone(),
            attempts: 1,
        };
        assert!(saved.matches(&id, &dir));
        let touched = SourceId {
            mtime_ms: id.mtime_ms + 60_000,
            ..id.clone()
        };
        assert!(saved.matches(&touched, &dir), "mtime alone does not matter");
        let bigger = SourceId {
            size: id.size + 4096,
            ..id.clone()
        };
        assert!(!saved.matches(&bigger, &dir));
        let other = SourceId {
            source_hash: "00".repeat(32),
            ..id.clone()
        };
        assert!(!saved.matches(&other, &dir));
        // The real file: a new mtime still matches, a changed version does not.
        let file = fs::OpenOptions::new().write(true).open(data.db()).unwrap();
        file.set_modified(at(1_000_000)).unwrap();
        drop(file);
        let moved = source_id(&data.source(), &data.db()).unwrap();
        assert_ne!(moved.mtime_ms, id.mtime_ms);
        assert!(saved.matches(&moved, &dir));
        data.source()
            .execute_batch("PRAGMA user_version=6;")
            .unwrap();
        let changed = source_id(&data.source(), &data.db()).unwrap();
        assert!(!saved.matches(&changed, &dir), "the version changed");
        // The backup it names must still be a real file there.
        fs::remove_file(dir.join(&backup)).unwrap();
        assert!(!saved.matches(&id, &dir));
        let escape = UpgradeState {
            backup: "../x".into(),
            ..saved
        };
        assert!(!escape.matches(&id, &dir));
    }

    #[test]
    fn the_source_hash_follows_every_v5_chat_write_and_nothing_else() {
        let data = Data::new();
        let db = data.source();
        db.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT NOT NULL,
                 updated_at INTEGER NOT NULL, body TEXT NOT NULL);
             INSERT INTO sessions VALUES ('a','A',1,'{\"x\":1}'), ('b','B',2,'{}');",
        )
        .unwrap();
        let hash = || source_id(&db, &data.db()).unwrap().source_hash;
        let start = hash();
        assert_eq!(hash(), start, "reading twice gives the same identity");
        let mut seen = vec![start.clone()];
        let mut changes = |sql: &str| {
            db.execute_batch(sql).unwrap();
            let now = hash();
            assert!(!seen.contains(&now), "{sql} kept an earlier identity");
            seen.push(now);
        };
        // A save that bumps the date, one in the same millisecond, an import and a delete.
        changes("UPDATE sessions SET updated_at=3, body='{\"x\":2}' WHERE id='a'");
        changes("UPDATE sessions SET body='{\"x\":22}' WHERE id='a'");
        changes("INSERT INTO sessions VALUES ('c','C',3,'{}')");
        changes("DELETE FROM sessions WHERE id='b'");
        changes("PRAGMA user_version=4");
    }

    #[test]
    fn the_source_hash_covers_every_table_a_restore_brings_back() {
        let data = Data::new();
        let directory = data.0.join("v5");
        crate::storage::fixtures::build_v5(&directory, &[], 5);
        let db = Connection::open(directory.join(super::super::DATABASE_FILE)).unwrap();
        let database = directory.join(super::super::DATABASE_FILE);
        let hash = || source_id(&db, &database).unwrap().source_hash;
        let mut seen = vec![hash()];
        let mut changes = |what: &str, sql: &str| {
            db.execute_batch(sql).unwrap();
            let now = hash();
            assert!(!seen.contains(&now), "{what} did not change the identity");
            seen.push(now);
        };
        changes(
            "a settings write",
            "INSERT INTO settings VALUES ('preferences','{}')",
        );
        changes("a settings edit", "UPDATE settings SET value='{\"a\":1}'");
        changes(
            "a memory entry",
            "INSERT INTO memory(id,content,updated_at) VALUES ('m','fact',1);
             INSERT INTO memory_search(id,content) VALUES ('m','fact');",
        );
        changes(
            "a memory edit",
            "UPDATE memory SET content='fact two', revision=2",
        );
        changes(
            "a proposal",
            "INSERT INTO memory_proposals VALUES ('p','{}')",
        );
        changes(
            "a paired device",
            "INSERT INTO paired_devices VALUES ('d','{}')",
        );
        changes(
            "a new table",
            "CREATE TABLE later(x); INSERT INTO later VALUES (1)",
        );
        // The recall index is rebuilt from `sessions`: its content is not part of the identity.
        // (Skipping it is content-level only: its writes still move page_count, freelist_count
        // and the size, which can change the whole identity. That errs the safe way.)
        let content = || {
            let mut hash = Sha256::new();
            hash_tables(&db, &mut hash).unwrap();
            hex(&hash.finalize())
        };
        let before = content();
        db.execute_batch(
            "INSERT INTO sessions VALUES ('s','S',1,'{}');
             INSERT INTO indexed_sessions VALUES ('s',NULL,'chat');
             INSERT INTO session_messages(session_id,message_id,message_index,role,content)
                 VALUES ('s','m',0,'user','needle');",
        )
        .unwrap();
        let with_chat = content();
        assert_ne!(with_chat, before, "the chat itself counts");
        db.execute_batch("DELETE FROM session_messages; DELETE FROM indexed_sessions;")
            .unwrap();
        assert_eq!(content(), with_chat, "the recall index does not");
    }

    #[test]
    fn text_and_a_blob_of_the_same_bytes_hash_differently() {
        let data = Data::new();
        let db = data.source();
        db.execute_batch("CREATE TABLE v(x); INSERT INTO v VALUES ('abc');")
            .unwrap();
        let text = source_id(&db, &data.db()).unwrap().source_hash;
        db.execute_batch("UPDATE v SET x=CAST('abc' AS BLOB)")
            .unwrap();
        assert_eq!(
            db.query_row("SELECT typeof(x) FROM v", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "blob"
        );
        assert_ne!(source_id(&db, &data.db()).unwrap().source_hash, text);
    }

    fn touch(dir: &Path, name: &str) {
        fs::write(dir.join(name), b"x").unwrap();
    }
    const NOW: u64 = 1_800_000_000;
    fn aged(days: u64) -> String {
        backup_name(at(NOW - days * 86_400), days as u32)
    }

    #[test]
    fn prune_keeps_three_newest_plus_current_and_ignores_lookalikes() {
        let data = Data::new();
        let dir = backups_dir(&data.0).unwrap();
        let all: Vec<String> = (1..=6).map(aged).collect();
        for name in &all {
            touch(&dir, name);
        }
        let partial = format!("{}.partial", aged(9));
        touch(&dir, &partial);
        let lookalikes = [
            "upgrade-state.json".to_owned(),
            format!("{}.old", all[0]),
            "BLACKWALL.sqlite3.pre-v6-20261030T180301Z-1a2b3c4d.bak".to_owned(),
            "blackwall.sqlite3.pre-v6-20261030T180300Z-1A2B3C4D.bak".to_owned(),
            "blackwall.sqlite3.pre-v6-20261330T180300Z-1a2b3c4d.bak".to_owned(),
            "blackwall.sqlite3.pre-v6-20261030T180300Z-1a2b3c4.bak".to_owned(),
            "notes.bak".to_owned(),
            all[0].replace("blackwall", "other"),
        ];
        for name in &lookalikes {
            touch(&dir, name);
        }
        // An exact-pattern name that is a directory is not a backup file either.
        let folder = aged(20);
        fs::create_dir(dir.join(&folder)).unwrap();
        let mut untouched: Vec<String> = lookalikes.to_vec();
        untouched.push(folder);
        #[cfg(unix)]
        {
            let target = data.0.join("target");
            fs::write(&target, b"x").unwrap();
            let link = aged(21);
            std::os::unix::fs::symlink(&target, dir.join(&link)).unwrap();
            untouched.push(link);
        }
        // `current` is the 5th newest of 6, so 4 stay: the newest 3 and it.
        let removed = prune(&dir, Some(&all[4]), at(NOW), None, None).unwrap();
        assert_eq!(removed, 3, "the 4th and 6th backups and the stale partial");
        let left = names(&dir);
        for name in [&all[0], &all[1], &all[2], &all[4]] {
            assert!(left.contains(name), "{name} should stay");
        }
        for gone in [&all[3], &all[5], &partial] {
            assert!(!left.contains(gone), "{gone} should go");
        }
        for name in &untouched {
            assert!(left.contains(name), "{name} is not ours to touch");
        }
        assert_eq!(prune(&dir, Some(&all[4]), at(NOW), None, None).unwrap(), 0);
    }

    #[test]
    fn age_rule_is_off_by_default_and_deletes_when_set() {
        assert_eq!(DELETE_BACKUPS_AFTER, None, "off until the owner decides");
        assert_eq!(KEEP_BACKUPS, 3);
        let day = Duration::from_secs(86_400);
        let data = Data::new();
        let dir = backups_dir(&data.0).unwrap();
        for days in 1..=4 {
            touch(&dir, &aged(days));
        }
        let long_ago = Some(at(NOW - 400 * 86_400));
        // Off: even a very old upgrade deletes nothing beyond the keep rule (3 + current = 4).
        let none = prune(
            &dir,
            Some(&aged(4)),
            at(NOW),
            long_ago,
            DELETE_BACKUPS_AFTER,
        )
        .unwrap();
        assert_eq!((none, names(&dir).len()), (0, 4));
        // On, but not yet due, or no upgrade time known.
        let early = Some(at(NOW - 29 * 86_400));
        let current = Some(aged(4));
        let current = current.as_deref();
        assert_eq!(
            prune(&dir, current, at(NOW), early, Some(30 * day)).unwrap(),
            0
        );
        assert_eq!(
            prune(&dir, current, at(NOW), None, Some(30 * day)).unwrap(),
            0
        );
        assert_eq!(names(&dir).len(), 4);
        // On and due: exactly at the limit counts, and the current one goes too.
        let due = Some(at(NOW - 30 * 86_400));
        let all = prune(&dir, Some(&aged(1)), at(NOW), due, Some(30 * day)).unwrap();
        assert_eq!(all, 4);
        assert!(names(&dir).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_data_folder_others_can_write_is_refused() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        for (mode, ok) in [
            (0o700, true),
            (0o755, true),
            (0o770, false),
            (0o720, false),
            (0o707, false),
            (0o777, false),
        ] {
            let data = Data::new();
            fs::set_permissions(&data.0, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                backups_dir(&data.0).is_ok(),
                ok,
                "data folder mode {mode:o}"
            );
        }
        // Another owner cannot be made without root, so the ownership rule is tested directly.
        let data = Data::new();
        fs::create_dir(data.0.join(BACKUPS_DIR)).unwrap();
        fs::set_permissions(data.0.join(BACKUPS_DIR), fs::Permissions::from_mode(0o700)).unwrap();
        let data_meta = fs::metadata(&data.0).unwrap();
        let backups_meta = fs::metadata(data.0.join(BACKUPS_DIR)).unwrap();
        let me = data_meta.uid();
        assert_eq!(me, euid());
        assert!(data_folder_is_private(&data_meta, me));
        assert!(!data_folder_is_private(&data_meta, me + 1));
        assert!(backups_folder_is_private(&backups_meta, me));
        assert!(!backups_folder_is_private(&backups_meta, me + 1));
    }

    #[test]
    fn an_existing_backup_name_is_never_overwritten() {
        let data = Data::new();
        let dir = backups_dir(&data.0).unwrap();
        let taken = backup_name(at(NOW), 7);
        fs::write(dir.join(&taken), b"precious").unwrap();
        let error = create_named_backup(&data.source(), &dir, &taken, 5).unwrap_err();
        assert!(matches!(error, StorageError::BackupFailed));
        assert_eq!(fs::read(dir.join(&taken)).unwrap(), b"precious");
        assert_eq!(names(&dir), [taken], "and no partial is left");
    }

    #[test]
    fn a_panic_during_the_copy_still_removes_the_partial() {
        let data = Data::new();
        let dir = backups_dir(&data.0).unwrap();
        set_fault(&data.0, Some(Fault::BackupPanic));
        let source = data.source();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = create_backup(&source, &dir, 5);
        }));
        assert!(outcome.is_err(), "the fault panicked");
        assert!(names(&dir).is_empty(), "left {:?}", names(&dir));
    }

    #[test]
    fn a_leftover_partial_name_is_not_ours_to_remove() {
        // A name that already exists is not something this run created, so a failed create
        // leaves it alone.
        let data = Data::new();
        let dir = backups_dir(&data.0).unwrap();
        let taken = backup_name(at(NOW), 9);
        let partial = format!("{taken}{PARTIAL_SUFFIX}");
        fs::write(dir.join(&partial), b"someone else's").unwrap();
        assert!(create_named_backup(&data.source(), &dir, &taken, 5).is_err());
        assert_eq!(fs::read(dir.join(&partial)).unwrap(), b"someone else's");
    }

    #[test]
    fn prune_also_removes_the_crash_sidecars_of_a_partial() {
        let data = Data::new();
        let dir = backups_dir(&data.0).unwrap();
        let backup = aged(1);
        touch(&dir, &backup);
        let partial = format!("{}{PARTIAL_SUFFIX}", aged(2));
        let mut ours = vec![partial.clone()];
        ours.extend(SIDECARS.iter().map(|sidecar| format!("{partial}{sidecar}")));
        // A sidecar whose partial is already gone is still ours: it names an exact stem.
        ours.push(format!("{}{PARTIAL_SUFFIX}-wal", aged(3)));
        let others = [
            format!("{}-wal", aged(4)),
            format!("{partial}-wal.old"),
            format!("{partial}-other"),
            "notes.partial-wal".to_owned(),
            "upgrade-state.json".to_owned(),
        ];
        for name in ours.iter().chain(&others) {
            touch(&dir, name);
        }
        let removed = prune(&dir, None, at(NOW), None, None).unwrap();
        assert_eq!(removed, ours.len());
        let mut left = others.to_vec();
        left.push(backup);
        left.sort();
        assert_eq!(names(&dir), left);
    }

    #[cfg(unix)]
    #[test]
    fn a_pipe_where_the_state_file_belongs_is_refused_without_blocking() {
        let data = Data::new();
        let dir = backups_dir(&data.0).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(dir.join("upgrade-state.json"))
            .status()
            .unwrap();
        assert!(made.success());
        assert_eq!(read_state(&dir), None);
    }

    #[test]
    fn the_state_file_limit_is_exactly_4_kib() {
        let data = Data::new();
        let dir = backups_dir(&data.0).unwrap();
        let file = dir.join("upgrade-state.json");
        let body = serde_json::to_string(&state(GOOD)).unwrap();
        for (size, ok) in [(4095, true), (4096, true), (4097, false)] {
            let padded = format!("{body}{}", " ".repeat(size - body.len()));
            assert_eq!(padded.len(), size);
            fs::write(&file, padded).unwrap();
            assert_eq!(read_state(&dir).is_some(), ok, "{size} bytes");
        }
    }

    #[test]
    fn without_hard_links_the_backup_still_completes_and_never_overwrites() {
        let data = Data::new();
        let dir = backups_dir(&data.0).unwrap();
        set_fault(&data.0, Some(Fault::NoHardLinks));
        let name = create_backup(&data.source(), &dir, 5).unwrap();
        assert_eq!(
            names(&dir),
            std::slice::from_ref(&name),
            "no partial remains"
        );
        let copy = Connection::open(dir.join(&name)).unwrap();
        let kept: String = copy.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, "kept");
        drop(copy);
        // The fallback checks first, so an existing name is left alone and the call fails.
        let taken = backup_name(at(NOW), 11);
        fs::write(dir.join(&taken), b"precious").unwrap();
        let error = create_named_backup(&data.source(), &dir, &taken, 5).unwrap_err();
        assert!(matches!(error, StorageError::BackupFailed));
        assert_eq!(fs::read(dir.join(&taken)).unwrap(), b"precious");
        assert_eq!(names(&dir).len(), 2, "and no partial is left");
    }

    #[test]
    fn only_filesystem_limits_trigger_the_rename_fallback() {
        assert!(links_unsupported(&io::Error::from(
            io::ErrorKind::Unsupported
        )));
        assert!(!links_unsupported(&io::Error::from(
            io::ErrorKind::AlreadyExists
        )));
        assert!(!links_unsupported(&io::Error::from(
            io::ErrorKind::NotFound
        )));
        assert!(!links_unsupported(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
        #[cfg(unix)]
        {
            for code in [libc::EPERM, libc::ENOTSUP, libc::EOPNOTSUPP, libc::EXDEV] {
                assert!(
                    links_unsupported(&io::Error::from_raw_os_error(code)),
                    "{code}"
                );
            }
            for code in [libc::EEXIST, libc::ENOENT, libc::EACCES, libc::ENOSPC] {
                assert!(
                    !links_unsupported(&io::Error::from_raw_os_error(code)),
                    "{code}"
                );
            }
        }
    }
}
