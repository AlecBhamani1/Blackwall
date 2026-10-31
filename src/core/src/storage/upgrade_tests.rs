use super::*;
use crate::memory::MemoryClient;
use crate::storage::backup::{backup_name, backups_dir, set_fault, AbortAt, Fault, BACKUPS_DIR};
use crate::storage::fixtures::{
    build_v5, golden, master, platform_neutral, v5_sessions, RECALL_QUERIES,
};
use crate::storage::search::search_v6;
use crate::storage::{RecallQuery, BUSY_TIMEOUT, DATABASE_FILE};
use serde_json::{json, Map, Value};
use std::{
    fs,
    path::PathBuf,
    process::Child,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// The ids fixture's renames: rows 0 and 5 had no id, and row 3 repeated row 2's `a`.
const RENAMED: [(usize, &str); 3] = [(0, "message_0"), (3, "message_3"), (5, "message_5")];

/// A data folder that is removed, with its faults disarmed, when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "blackwall-upgrade-{name}-{:016x}",
            rand::random::<u64>()
        )))
    }
    fn database(&self) -> PathBuf {
        self.0.join(DATABASE_FILE)
    }
    fn backups(&self) -> PathBuf {
        self.0.join(BACKUPS_DIR)
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        set_fault(&self.0, None);
        set_test_busy_timeout(&self.0, None);
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture(id: &str) -> Value {
    v5_sessions()
        .into_iter()
        .find(|session| session["id"] == id)
        .unwrap()
}

/// The v5 body as v6 must load it: the ids fixture with its documented ids.
fn expected(session: &Value) -> Value {
    let mut session = session.clone();
    if session["id"] == "ids_missing_and_duplicate" {
        for (position, id) in RENAMED {
            session["messages"][position]["id"] = json!(id);
        }
    }
    session
}

fn version_of(database: &Path) -> i64 {
    Connection::open(database)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

/// Every file name in the backups folder, sorted; none when it does not exist.
fn backup_files(dir: &Scratch) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir.backups()) else {
        return vec![];
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn baks(dir: &Scratch) -> Vec<String> {
    backup_files(dir)
        .into_iter()
        .filter(|name| name.ends_with(".bak"))
        .collect()
}

fn session_rows(database: &Path) -> Vec<(String, String, i64, String)> {
    let connection = Connection::open(database).unwrap();
    let mut statement = connection
        .prepare("SELECT id,title,updated_at,body FROM sessions ORDER BY id")
        .unwrap();
    let rows = statement
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap();
    rows.collect::<Result<_, _>>().unwrap()
}

fn open6(dir: &Path) -> Result<LocalStore, StorageError> {
    LocalStore::open_at(dir, V6)
}

fn failed_backup(result: Result<LocalStore, StorageError>) -> (PathBuf, String) {
    match result {
        Err(
            error @ StorageError::UpgradeFailed {
                backup: Some(_), ..
            },
        ) => {
            let text = error.to_string();
            let StorageError::UpgradeFailed {
                backup: Some(path), ..
            } = error
            else {
                unreachable!()
            };
            (path, text)
        }
        Err(other) => panic!("expected UpgradeFailed with a backup, got {other:?}"),
        Ok(_) => panic!("expected UpgradeFailed, but the upgrade succeeded"),
    }
}

fn v6_recall(db: &LocalStore) -> Value {
    let mut results = Map::new();
    for query in RECALL_QUERIES {
        let hits = search_v6(
            db,
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
    Value::Object(results)
}

#[test]
fn every_fixture_upgrades_with_equal_json_list_context_and_recall() {
    let dir = Scratch::new("fixtures");
    let fixtures = v5_sessions();
    build_v5(&dir, &fixtures, 5);
    let db = open6(&dir).unwrap();
    assert_eq!(version_of(&dir.database()), V6);
    // Each chat, its context included, loads as its v5 body; the ids fixture with its mapping.
    for session in &fixtures {
        let id = session["id"].as_str().unwrap();
        if id == "bad_role" {
            assert_eq!(db.load_json_v6(id).unwrap(), None);
            continue;
        }
        assert_eq!(
            db.load_json_v6(id).unwrap().unwrap(),
            expected(session),
            "{id}"
        );
    }
    let ids = db
        .load_json_v6("ids_missing_and_duplicate")
        .unwrap()
        .unwrap();
    for (position, id) in RENAMED {
        assert_eq!(ids["messages"][position]["id"], id);
    }
    assert_eq!(ids["messages"][2]["id"], "a");
    // The list, in order, with titles, modes and workspaces, is v5's (F6).
    let v5 = LocalStore::initialize(Connection::open_in_memory().unwrap()).unwrap();
    for session in fixtures.iter().filter(|s| s["id"] != "bad_role") {
        v5.save_session(session).unwrap();
    }
    assert_eq!(db.list_v6().unwrap(), v5.list_sessions().unwrap());
    // Recall is v5's. No golden hit is in the ids fixture, so its renames cannot show (F13a).
    let golden = golden("v5_recall.json");
    assert!(!golden.to_string().contains("ids_missing_and_duplicate"));
    assert_eq!(platform_neutral(v6_recall(&db)), golden);
    // The upgrade finished: one verified backup stays, and the state file is gone.
    assert_eq!(baks(&dir).len(), 1);
    assert_eq!(backup_files(&dir), baks(&dir));
}

#[test]
fn keys_follow_the_conversion_order_and_a_new_chat_takes_the_next() {
    let dir = Scratch::new("keys");
    build_v5(&dir, &v5_sessions(), 5);
    let db = open6(&dir).unwrap();
    let ids = |sql: &str| -> Vec<String> {
        let mut statement = db.connection().prepare(sql).unwrap();
        let rows = statement.query_map([], |row| row.get(0)).unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    };
    // Newest first, as `convert_all` walks them; the set-aside chat's key was rolled back with it.
    let by_key = ids("SELECT id FROM sessions ORDER BY key");
    assert_eq!(
        by_key,
        ids("SELECT id FROM sessions ORDER BY updated_at DESC, rowid")
    );
    let keys: Vec<i64> = {
        let mut statement = db
            .connection()
            .prepare("SELECT key FROM sessions ORDER BY key")
            .unwrap();
        let rows = statement.query_map([], |row| row.get(0)).unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(keys, (1..=by_key.len() as i64).collect::<Vec<_>>());
    db.create_session(&crate::storage::NewSession {
        id: "brand_new".into(),
        title: "New".into(),
        mode: None,
        workspace: None,
        updated_at: 1,
    })
    .unwrap();
    let key: i64 = db
        .connection()
        .query_row("SELECT key FROM sessions WHERE id='brand_new'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(key, by_key.len() as i64 + 1);
}

#[test]
fn the_invalid_fixture_is_set_aside_and_noticed_once() {
    let dir = Scratch::new("notices");
    build_v5(&dir, &v5_sessions(), 5);
    let db = open6(&dir).unwrap();
    let notices = db.take_storage_notices().unwrap();
    assert_eq!(
        notices[..2],
        [
            StorageNotice::SetAside { chats: 1 },
            StorageNotice::RenamedIds {
                messages: 3,
                chats: 1
            },
        ]
    );
    match &notices[2..] {
        [StorageNotice::UpgradeStorageReport {
            deduped_bytes,
            inline_bytes,
            inline_reasons,
        }] => {
            assert!(*deduped_bytes > 0);
            assert_eq!(*inline_bytes > 0, !inline_reasons.is_empty());
        }
        other => panic!("expected one storage report, got {other:?}"),
    }
    // Shown once: taken, they are gone, and a reopen adds none.
    assert_eq!(db.take_storage_notices().unwrap(), []);
    drop(db);
    let db = open6(&dir).unwrap();
    assert_eq!(db.take_storage_notices().unwrap(), []);
    // The chat itself is kept whole, set aside.
    let raw: String = db
        .connection()
        .query_row(
            "SELECT body FROM quarantined_sessions WHERE id='bad_role'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(raw, serde_json::to_string(&fixture("bad_role")).unwrap());
    // The webview reads them by `kind`.
    assert_eq!(
        serde_json::to_value(StorageNotice::RenamedIds {
            messages: 3,
            chats: 1
        })
        .unwrap(),
        json!({"kind": "renamedIds", "messages": 3, "chats": 1})
    );
    assert_eq!(
        serde_json::to_value(StorageNotice::UpgradeStorageReport {
            deduped_bytes: 1,
            inline_bytes: 2,
            inline_reasons: [("tool".to_owned(), 3)].into(),
        })
        .unwrap(),
        json!({"kind": "upgradeStorageReport", "dedupedBytes": 1, "inlineBytes": 2,
            "inlineReasons": {"tool": 3}})
    );
}

#[test]
fn the_import_records_its_set_asides_as_a_notice() {
    let dir = Scratch::new("import");
    let mut db = open6(&dir).unwrap();
    db.migrate_sessions(&[fixture("desktop_plain"), fixture("bad_role")])
        .unwrap();
    assert_eq!(
        db.take_storage_notices().unwrap(),
        [StorageNotice::SetAside { chats: 1 }]
    );
    assert_eq!(
        db.load_json_v6("desktop_plain").unwrap().unwrap(),
        fixture("desktop_plain")
    );
}

#[test]
fn every_version_below_six_upgrades_and_a_fresh_database_takes_no_backup() {
    let sessions: Vec<Value> = v5_sessions().into_iter().take(2).collect();
    for version in 0..=5 {
        let dir = Scratch::new(&format!("v{version}"));
        build_v5(&dir, &sessions, version);
        assert_eq!(
            LocalStore::upgrade_status(&dir).unwrap(),
            UpgradeStatus::Pending
        );
        let db = open6(&dir).unwrap();
        assert_eq!(version_of(&dir.database()), V6, "v{version}");
        for session in &sessions {
            let id = session["id"].as_str().unwrap();
            assert_eq!(
                &db.load_json_v6(id).unwrap().unwrap(),
                session,
                "v{version}"
            );
        }
        // The old memory shape got today's columns and tables.
        db.connection()
            .execute_batch("SELECT scope,fact_key,revision FROM memory; SELECT * FROM memory_proposals; SELECT * FROM paired_devices;")
            .unwrap();
        assert_eq!(baks(&dir).len(), 1, "v{version}");
        // The backup is the source as it was, at its own version.
        let backup = dir.backups().join(&baks(&dir)[0]);
        assert_eq!(version_of(&backup), version);
        assert_eq!(backup_files(&dir), baks(&dir), "no state file is left");
        assert_eq!(
            LocalStore::upgrade_status(&dir).unwrap(),
            UpgradeStatus::Current
        );
        // A v6 store refuses a v5 opener cleanly.
        drop(db);
        assert!(matches!(
            LocalStore::open_at(&dir, 5),
            Err(StorageError::Invalid(text)) if text.contains("newer Blackwall version")
        ));
    }

    let fresh = Scratch::new("fresh");
    let db = open6(&fresh).unwrap();
    assert_eq!(version_of(&fresh.database()), V6);
    assert!(
        !fresh.backups().exists(),
        "a fresh database takes no backup"
    );
    assert_eq!(db.list_v6().unwrap(), Vec::<Value>::new());
    assert_eq!(db.take_storage_notices().unwrap(), []);
    // Production still opens v5 until the switch-over.
    let production = Scratch::new("production");
    drop(LocalStore::open(&production).unwrap());
    assert_eq!(version_of(&production.database()), 5);
}

#[test]
fn a_chat_that_does_not_rebuild_is_set_aside_whole_not_stored_lossy() {
    let dir = Scratch::new("mismatch");
    let sessions: Vec<Value> = v5_sessions().into_iter().take(2).collect();
    build_v5(&dir, &sessions, 5);
    set_fault(&dir, Some(Fault::ForceMismatch));
    let db = open6(&dir).unwrap();
    assert_eq!(
        db.take_storage_notices().unwrap(),
        [StorageNotice::SetAside { chats: 2 }]
    );
    assert_eq!(db.list_v6().unwrap(), Vec::<Value>::new());
    for session in &sessions {
        let raw: String = db
            .connection()
            .query_row(
                "SELECT body FROM quarantined_sessions WHERE id=?1",
                [session["id"].as_str().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(raw, serde_json::to_string(session).unwrap());
    }
}

#[test]
fn an_injected_sql_error_rolls_back_to_whole_v5_and_names_the_backup() {
    let dir = Scratch::new("rollback");
    build_v5(&dir, &v5_sessions(), 5);
    let bytes = fs::read(dir.database()).unwrap();
    let schema = master(&dir.database());
    let rows = session_rows(&dir.database());
    set_fault(&dir, Some(Fault::ConvertError(3)));
    let (path, text) = failed_backup(open6(&dir));
    assert!(text.contains(&path.display().to_string()), "{text}");
    assert_eq!(path.parent(), Some(dir.backups().as_path()));
    // The database is exactly the v5 file it was.
    assert_eq!(fs::read(dir.database()).unwrap(), bytes);
    assert_eq!(master(&dir.database()), schema);
    // The backup stays, verified: v5 with every row.
    assert_eq!(version_of(&path), 5);
    assert_eq!(session_rows(&path), rows);
    // The failure before commit rolls back the same way.
    set_fault(&dir, Some(Fault::BeforeCommit));
    let (again, _) = failed_backup(open6(&dir));
    assert_eq!(again, path, "the matching backup is reused");
    assert_eq!(fs::read(dir.database()).unwrap(), bytes);
}

#[test]
fn disk_full_during_backup_changes_nothing() {
    for fault in [Fault::BackupWrite, Fault::BackupCopy] {
        let dir = Scratch::new("diskfull");
        build_v5(&dir, &v5_sessions(), 5);
        let bytes = fs::read(dir.database()).unwrap();
        set_fault(&dir, Some(fault));
        assert!(matches!(open6(&dir), Err(StorageError::BackupFailed)));
        assert_eq!(fs::read(dir.database()).unwrap(), bytes);
        assert_eq!(backup_files(&dir), Vec::<String>::new(), "{fault:?}");
        assert_eq!(
            LocalStore::upgrade_status(&dir).unwrap(),
            UpgradeStatus::Pending
        );
    }
}

#[cfg(unix)]
#[test]
fn a_backups_folder_open_to_others_is_refused_and_changes_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Scratch::new("shared");
    build_v5(&dir, &v5_sessions(), 5);
    let bytes = fs::read(dir.database()).unwrap();
    fs::create_dir(dir.backups()).unwrap();
    fs::set_permissions(dir.backups(), fs::Permissions::from_mode(0o755)).unwrap();
    match open6(&dir) {
        Err(StorageError::Invalid(text)) => assert!(text.contains("private to your user")),
        other => panic!("expected the private-folder error, got {:?}", other.err()),
    }
    assert_eq!(fs::read(dir.database()).unwrap(), bytes);
    assert_eq!(backup_files(&dir), Vec::<String>::new());
}

#[test]
fn three_failures_on_one_source_make_one_backup_then_stop() {
    let dir = Scratch::new("stop");
    build_v5(&dir, &v5_sessions(), 5);
    set_fault(&dir, Some(Fault::ConvertError(0)));
    let mut first = None;
    for attempt in 1..=3u32 {
        let (path, _) = failed_backup(open6(&dir));
        assert_eq!(first.get_or_insert_with(|| path.clone()), &path);
        assert_eq!(
            LocalStore::upgrade_status(&dir).unwrap(),
            UpgradeStatus::Failed {
                backup: path.display().to_string(),
                attempts: attempt,
                stopped: attempt == 3,
            }
        );
        // A new mtime alone is the same source.
        fs::File::options()
            .write(true)
            .open(dir.database())
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(u64::from(attempt) * 60))
            .unwrap();
    }
    let path = first.unwrap();
    match open6(&dir) {
        Err(StorageError::UpgradeStopped(stopped)) => assert_eq!(stopped, path),
        other => panic!("expected UpgradeStopped, got {:?}", other.err()),
    }
    assert_eq!(baks(&dir).len(), 1);
    assert_eq!(version_of(&dir.database()), 5);

    // Retry starts the count again; with the fault gone the upgrade goes through.
    set_fault(&dir, None);
    let db = LocalStore::retry_upgrade(&dir).unwrap();
    assert_eq!(version_of(&dir.database()), V6);
    assert_eq!(db.list_v6().unwrap().len(), v5_sessions().len() - 1);
    assert_eq!(
        LocalStore::upgrade_status(&dir).unwrap(),
        UpgradeStatus::Current
    );
    assert_eq!(baks(&dir), [path.file_name().unwrap().to_str().unwrap()]);
}

#[test]
fn an_upgraded_store_reopens_lock_free_while_a_writer_holds_the_lock() {
    let dir = Scratch::new("lockfree");
    build_v5(&dir, &v5_sessions()[..2], 5);
    drop(open6(&dir).unwrap());
    let writer = Connection::open(dir.database()).unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; INSERT INTO settings(key,value) VALUES ('held','1');")
        .unwrap();
    let started = Instant::now();
    let db = open6(&dir).unwrap();
    assert_eq!(db.list_v6().unwrap().len(), 2);
    assert!(
        started.elapsed() < BUSY_TIMEOUT,
        "the v6 open waited for the writer"
    );
    writer.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn a_post_commit_integrity_failure_is_a_notice_not_an_error() {
    let dir = Scratch::new("integrity");
    build_v5(&dir, &v5_sessions()[..2], 5);
    // A first attempt leaves its verified backup, so the next one reuses it and never runs the
    // backup's own integrity check: the fault then trips only the post-commit check.
    set_fault(&dir, Some(Fault::BeforeCommit));
    let (path, _) = failed_backup(open6(&dir));
    set_fault(&dir, Some(Fault::IntegrityCheck));
    let db = open6(&dir).unwrap();
    assert_eq!(version_of(&dir.database()), V6);
    let notices = db.take_storage_notices().unwrap();
    assert!(
        notices.contains(&StorageNotice::IntegrityFailed {
            backup: Some(path.display().to_string())
        }),
        "{notices:?}"
    );
    assert!(path.is_file(), "the backup to restore from stays");
}

/// Copies a file, or a folder of files, from one data folder to another.
fn copy_into(from: &Path, to: &Path, name: &str) {
    let (from, to) = (from.join(name), to.join(name));
    if from.is_dir() {
        fs::create_dir(&to).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&to, fs::Permissions::from_mode(0o700)).unwrap();
        }
        for entry in fs::read_dir(&from).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    } else {
        fs::copy(from, to).unwrap();
    }
}

#[test]
fn a_crashed_upgrade_reports_its_state_until_an_open_recovers_it() {
    let dir = Scratch::new("crashed");
    let sessions = v5_sessions();
    build_v5(&dir, &sessions, 5);
    set_fault(&dir, Some(Fault::ConvertError(0)));
    failed_backup(open6(&dir));
    set_fault(&dir, None);
    // A writer caught mid-transaction, copied as a crash leaves it: the database, its hot
    // journal, and the upgrade's backups folder.
    let crashed = Scratch::new("crashed-copy");
    fs::create_dir(&crashed.0).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&crashed.0, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let writer = Connection::open(dir.database()).unwrap();
    writer
        .execute_batch("PRAGMA cache_size=1; BEGIN IMMEDIATE; UPDATE sessions SET body=body||' ';")
        .unwrap();
    for name in [
        DATABASE_FILE,
        &format!("{DATABASE_FILE}-journal"),
        BACKUPS_DIR,
    ] {
        copy_into(&dir, &crashed, name);
    }
    writer.execute_batch("ROLLBACK").unwrap();

    let backup = crashed.backups().join(&baks(&crashed)[0]);
    assert_eq!(
        LocalStore::upgrade_status(&crashed).unwrap(),
        UpgradeStatus::Failed {
            backup: backup.display().to_string(),
            attempts: 1,
            stopped: false,
        }
    );
    // The open rolls the journal back, matches the source and reuses its backup.
    let db = open6(&crashed).unwrap();
    for session in sessions.iter().filter(|s| s["id"] != "bad_role") {
        let id = session["id"].as_str().unwrap();
        assert_eq!(
            db.load_json_v6(id).unwrap().unwrap(),
            expected(session),
            "{id}"
        );
    }
    assert_eq!(baks(&crashed).len(), 1);
    assert_eq!(
        LocalStore::upgrade_status(&crashed).unwrap(),
        UpgradeStatus::Current
    );
}

#[test]
fn status_is_pending_not_failed_while_another_window_holds_the_lock() {
    let dir = Scratch::new("inprogress");
    build_v5(&dir, &v5_sessions()[..2], 5);
    set_fault(&dir, Some(Fault::ConvertError(0)));
    let (path, _) = failed_backup(open6(&dir));
    let writer = Connection::open(dir.database()).unwrap();
    // An upgrade before its first write holds only the lock; one committing holds it exclusively.
    for begin in ["BEGIN IMMEDIATE", "BEGIN EXCLUSIVE"] {
        writer.execute_batch(begin).unwrap();
        assert_eq!(
            LocalStore::upgrade_status(&dir).unwrap(),
            UpgradeStatus::Pending,
            "{begin}"
        );
        writer.execute_batch("ROLLBACK").unwrap();
    }
    assert_eq!(
        LocalStore::upgrade_status(&dir).unwrap(),
        UpgradeStatus::Failed {
            backup: path.display().to_string(),
            attempts: 1,
            stopped: false,
        }
    );
}

#[test]
fn a_held_lock_returns_upgrade_busy_and_changes_nothing() {
    let dir = Scratch::new("busy");
    build_v5(&dir, &v5_sessions()[..2], 5);
    let bytes = fs::read(dir.database()).unwrap();
    set_test_busy_timeout(&dir, Some(Duration::from_millis(50)));
    let writer = Connection::open(dir.database()).unwrap();
    // IMMEDIATE: the version reads, the lock does not come. EXCLUSIVE: not even the read does.
    for begin in ["BEGIN IMMEDIATE", "BEGIN EXCLUSIVE"] {
        writer.execute_batch(begin).unwrap();
        assert!(
            matches!(open6(&dir), Err(StorageError::UpgradeBusy)),
            "{begin}"
        );
        writer.execute_batch("ROLLBACK").unwrap();
    }
    assert_eq!(fs::read(dir.database()).unwrap(), bytes);
    assert_eq!(backup_files(&dir), Vec::<String>::new());
    drop(writer);
    drop(open6(&dir).unwrap());
    assert_eq!(version_of(&dir.database()), V6);
}

#[test]
fn an_integrity_failure_on_a_fresh_database_is_still_noticed() {
    let dir = Scratch::new("fresh-integrity");
    set_fault(&dir, Some(Fault::IntegrityCheck));
    let db = open6(&dir).unwrap();
    assert_eq!(version_of(&dir.database()), V6);
    assert_eq!(
        db.take_storage_notices().unwrap(),
        [StorageNotice::IntegrityFailed { backup: None }]
    );
    assert_eq!(
        serde_json::to_value(StorageNotice::IntegrityFailed { backup: None }).unwrap(),
        json!({"kind": "integrityFailed", "backup": null})
    );
}

#[test]
fn a_failed_attempt_never_deletes_older_backups() {
    let dir = Scratch::new("keepold");
    build_v5(&dir, &v5_sessions()[..2], 5);
    let backups = backups_dir(&dir).unwrap();
    let old: Vec<String> = (1..=3u32)
        .map(|day| {
            backup_name(
                UNIX_EPOCH + Duration::from_secs(u64::from(day) * 86_400),
                day,
            )
        })
        .collect();
    for name in &old {
        fs::write(backups.join(name), b"an older backup").unwrap();
    }
    set_fault(&dir, Some(Fault::ConvertError(0)));
    let (path, _) = failed_backup(open6(&dir));
    let current = path.file_name().unwrap().to_str().unwrap().to_owned();
    let mut all = old.clone();
    all.push(current.clone());
    assert_eq!(baks(&dir), all, "a failed attempt pruned a backup");
    // Only the committed upgrade prunes: the 3 newest stay, its own among them.
    set_fault(&dir, None);
    drop(open6(&dir).unwrap());
    assert_eq!(baks(&dir), [old[1].clone(), old[2].clone(), current]);
}

#[test]
fn a_foreign_key_violation_rolls_back_to_whole_v5() {
    let dir = Scratch::new("foreignkey");
    build_v5(&dir, &v5_sessions()[..2], 5);
    // Written with foreign keys off, as an old or damaged file could hold it.
    Connection::open(dir.database())
        .unwrap()
        .execute_batch(
            "PRAGMA foreign_keys=OFF;
             CREATE TABLE extra(session TEXT REFERENCES sessions(id));
             INSERT INTO extra VALUES ('no_such_chat');",
        )
        .unwrap();
    let bytes = fs::read(dir.database()).unwrap();
    match open6(&dir) {
        Err(StorageError::UpgradeFailed {
            backup: Some(path),
            source,
        }) => {
            assert!(path.is_file());
            assert!(
                matches!(*source, StorageError::Invalid(text) if text == FOREIGN_KEYS_BROKEN),
                "{source:?}"
            );
        }
        other => panic!("expected UpgradeFailed, got {:?}", other.err()),
    }
    assert_eq!(fs::read(dir.database()).unwrap(), bytes);
}

#[test]
fn a_changed_source_after_a_stop_gets_a_new_backup_and_count() {
    let dir = Scratch::new("changed");
    build_v5(&dir, &v5_sessions()[..2], 5);
    set_fault(&dir, Some(Fault::ConvertError(0)));
    let (first, _) = failed_backup(open6(&dir));
    for _ in 0..2 {
        failed_backup(open6(&dir));
    }
    assert!(matches!(open6(&dir), Err(StorageError::UpgradeStopped(_))));
    // A v5 window still running writes a setting: this is a different source now.
    Connection::open(dir.database())
        .unwrap()
        .execute("INSERT INTO settings(key,value) VALUES ('v5','write')", [])
        .unwrap();
    let (second, _) = failed_backup(open6(&dir));
    assert_ne!(second, first);
    assert_eq!(
        LocalStore::upgrade_status(&dir).unwrap(),
        UpgradeStatus::Failed {
            backup: second.display().to_string(),
            attempts: 1,
            stopped: false,
        }
    );
    assert_eq!(baks(&dir).len(), 2);
}

// ---- crashes, two processes and stale writers (A11) ----
//
// A child is this test binary run again on one `#[ignore]`d test, so it has `cfg(test)` and the
// data-folder-keyed faults, which an in-process thread-local could not give it. A test that finds
// the child's env missing (someone ran the ignored test by hand) does nothing.

const CRASH_AT: &str = "BLACKWALL_TEST_CRASH_AT";
const CHILD_DIR: &str = "BLACKWALL_TEST_CHILD_DIR";
const CHILD_SYNC: &str = "BLACKWALL_TEST_CHILD_SYNC";
const CHILD_NAME: &str = "BLACKWALL_TEST_CHILD_NAME";
/// Not an abort: the crash child reached the end of its open.
const SURVIVED: i32 = 42;
/// Far longer than any runner needs; a wait only ever ends early, on the thing it waits for.
const WAIT: Duration = Duration::from_secs(120);

const CRASH_POINTS: [(&str, AbortAt); 5] = [
    ("DuringBackup", AbortAt::DuringBackup),
    ("AfterBackup", AbortAt::AfterBackup),
    ("DuringConvert", AbortAt::DuringConvert),
    ("BeforeCommit", AbortAt::BeforeCommit),
    ("AfterCommit", AbortAt::AfterCommit),
];

/// Waits for `path`, failing fast if a child dies first and at `WAIT` if nothing ever comes.
fn wait_for(path: &Path, children: &mut [Child]) {
    let start = Instant::now();
    while !path.exists() {
        for child in children.iter_mut() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "a child exited while the test waited for {}",
                path.display()
            );
        }
        assert!(
            start.elapsed() < WAIT,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Kills any child still running when the test ends, however it ends.
struct Reaper(Vec<Child>);

impl Drop for Reaper {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Starts this binary on the ignored test `name`, its output in `log` (a file, so no pipe can fill
/// and block the child).
fn spawn_child(name: &str, log: &Path, env: &[(&str, &str)]) -> Child {
    let log = fs::File::create(log).unwrap();
    std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            name,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("RUST_BACKTRACE", "0")
        .envs(env.iter().copied())
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap()
}

fn wait_exit(mut child: Child) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if start.elapsed() > WAIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("a child did not exit");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn table_exists(database: &Path, name: &str) -> bool {
    Connection::open(database)
        .unwrap()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
            [name],
            |row| row.get(0),
        )
        .unwrap()
}

/// Rows in the tables beside the chats, so a crash test can see that they are untouched too.
fn seed_other_tables(database: &Path) {
    Connection::open(database)
        .unwrap()
        .execute_batch(
            "INSERT INTO settings VALUES ('preferences','{\"theme\":\"dark\"}');
             INSERT INTO memory(id,content,updated_at,fact_key) VALUES ('m','a fact',1,'m');
             INSERT INTO memory_search(id,content) VALUES ('m','a fact');
             INSERT INTO memory_proposals VALUES ('p','{\"proposal\":1}');
             INSERT INTO paired_devices VALUES ('d','{\"device\":1}');",
        )
        .unwrap();
}

/// Every row of the non-chat tables a restore brings back, byte for byte.
fn other_tables(database: &Path) -> Vec<(String, Vec<String>)> {
    let connection = Connection::open(database).unwrap();
    ["settings", "memory", "memory_proposals", "paired_devices"]
        .iter()
        .map(|table| {
            let mut statement = connection
                .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                .unwrap();
            let columns = statement.column_count();
            let rows = statement
                .query_map([], |row| {
                    Ok((0..columns)
                        .map(|i| format!("{:?}", row.get_ref(i).unwrap()))
                        .collect::<Vec<_>>()
                        .join("|"))
                })
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            ((*table).to_owned(), rows)
        })
        .collect()
}

/// Every chat but the deliberately invalid one loads as it did in v5.
fn assert_chats_intact(db: &LocalStore, sessions: &[Value]) {
    for session in sessions.iter().filter(|s| s["id"] != "bad_role") {
        let id = session["id"].as_str().unwrap();
        assert_eq!(
            db.load_json_v6(id).unwrap().unwrap(),
            expected(session),
            "{id}"
        );
    }
}

#[test]
fn a_crash_after_backup_during_convert_or_before_commit_reopens_as_whole_v5_then_upgrades_reusing_the_backup(
) {
    // The process is killed (SIGABRT, or the Windows fast-fail) at each point, with no cleanup.
    for (name, at) in CRASH_POINTS {
        let dir = Scratch::new("crash");
        let sessions = v5_sessions();
        build_v5(&dir, &sessions, 5);
        seed_other_tables(&dir.database());
        let before = session_rows(&dir.database());
        let before_other = other_tables(&dir.database());
        let sync = Scratch::new("crash-sync");
        fs::create_dir_all(&sync.0).unwrap();

        let child = spawn_child(
            "storage::upgrade::tests::crash_child",
            &sync.0.join("child.log"),
            &[(CRASH_AT, name), (CHILD_DIR, dir.to_str().unwrap())],
        );
        let status = wait_exit(child);
        let log = fs::read_to_string(sync.0.join("child.log")).unwrap_or_default();
        assert!(!status.success(), "{name}: the child did not crash\n{log}");
        assert_ne!(
            status.code(),
            Some(SURVIVED),
            "{name}: no abort fired\n{log}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(), Some(6), "{name}: not an abort\n{log}");
        }
        assert!(
            !matches!(
                LocalStore::upgrade_status(&dir).unwrap(),
                UpgradeStatus::Current
            ) || at == AbortAt::AfterCommit,
            "{name}"
        );

        if at == AbortAt::AfterCommit {
            // Committed, then killed before the integrity check and the pruning: its state file
            // is still there.
            assert_eq!(version_of(&dir.database()), V6, "{name}");
            assert!(
                backup_files(&dir).contains(&"upgrade-state.json".to_owned()),
                "{name}"
            );
        } else {
            // Reading the file rolls its hot journal back: the whole v5 database, untouched.
            assert_eq!(version_of(&dir.database()), 5, "{name}");
            assert_eq!(session_rows(&dir.database()), before, "{name}");
            assert_eq!(other_tables(&dir.database()), before_other, "{name}");
            assert!(!table_exists(&dir.database(), "chat_events"), "{name}");
        }

        let db = open6(&dir).unwrap();
        assert_chats_intact(&db, &sessions);
        drop(db);
        // The next successful open finishes the cleanup: no state file is left, whatever the
        // crash point (after a commit, the v6 open clears it).
        assert!(
            !backup_files(&dir).contains(&"upgrade-state.json".to_owned()),
            "{name}: {:?}",
            backup_files(&dir)
        );
        assert_eq!(baks(&dir).len(), 1, "{name}: {:?}", backup_files(&dir));
        // Whatever the crash left in the backups folder is gone or still one backup's own.
        assert!(
            backup_files(&dir).iter().all(|f| !f.contains("partial")),
            "{name}: {:?}",
            backup_files(&dir)
        );
        // Opening again neither upgrades nor backs up.
        drop(open6(&dir).unwrap());
        assert_eq!(baks(&dir).len(), 1, "{name}");
        assert_eq!(
            LocalStore::upgrade_status(&dir).unwrap(),
            UpgradeStatus::Current,
            "{name}"
        );
    }
}

/// The crash test's child: arms the abort and opens at v6.
#[test]
#[ignore = "child of the crash test"]
fn crash_child() {
    let (Ok(at), Ok(dir)) = (std::env::var(CRASH_AT), std::env::var(CHILD_DIR)) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let (_, at) = CRASH_POINTS.iter().find(|(name, _)| *name == at).unwrap();
    set_fault(&dir, Some(Fault::Abort(*at)));
    let _ = open6(&dir);
    std::process::exit(SURVIVED);
}

#[test]
fn two_processes_opening_one_v5_database_one_migrates_and_the_other_waits() {
    let dir = Scratch::new("two");
    let sessions = v5_sessions();
    build_v5(&dir, &sessions, 5);
    let sync = Scratch::new("two-sync");
    fs::create_dir_all(&sync.0).unwrap();
    let data = dir.to_str().unwrap();
    let sync_path = sync.to_str().unwrap();
    let mut children = Reaper(
        ["a", "b"]
            .iter()
            .map(|name| {
                spawn_child(
                    "storage::upgrade::tests::upgrade_child",
                    &sync.0.join(format!("{name}.log")),
                    &[
                        (CHILD_DIR, data),
                        (CHILD_SYNC, sync_path),
                        (CHILD_NAME, name),
                    ],
                )
            })
            .collect(),
    );
    // Both are up and about to open; only then do they go, together.
    for name in ["a", "b"] {
        wait_for(&sync.0.join(format!("{name}.ready")), &mut children.0);
    }
    fs::write(sync.0.join("go"), b"").unwrap();
    // One of them is now inside the upgrade, holding the write lock with its backup made.
    wait_for(&dir.0.join(backup::GATE_HELD), &mut children.0);
    // Both reached the write lock: the winner holds it, so the other is asking for it now.
    let start = Instant::now();
    while locking_marks(&dir) < 2 {
        for child in &mut children.0 {
            assert!(child.try_wait().unwrap().is_none(), "a child exited early");
        }
        assert!(
            start.elapsed() < WAIT,
            "the second opener never reached the lock"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // The other cannot be done: it can only be waiting for the lock (or still on its way to it).
    for child in &mut children.0 {
        assert!(child.try_wait().unwrap().is_none());
    }
    assert_eq!(version_of(&dir.database()), 5);
    assert_eq!(baks(&dir).len(), 1, "{:?}", backup_files(&dir));
    fs::write(dir.0.join(backup::GATE_RELEASE), b"").unwrap();

    let logs: Vec<String> = ["a", "b"]
        .iter()
        .zip(children.0.drain(..))
        .map(|(name, child)| {
            let status = wait_exit(child);
            let log = fs::read_to_string(sync.0.join(format!("{name}.log"))).unwrap();
            assert!(status.success(), "{name} failed: {status:?}\n{log}");
            log
        })
        .collect();
    // One upgraded; the other waited for the lock and then found v6.
    let upgraded = |log: &String| log.contains("chats upgraded to v6");
    let waited = |log: &String| log.contains("found v6 after waiting");
    assert_eq!(
        logs.iter().filter(|log| upgraded(log)).count(),
        1,
        "{logs:?}"
    );
    assert_eq!(logs.iter().filter(|log| waited(log)).count(), 1, "{logs:?}");
    assert!(
        logs.iter().all(|log| upgraded(log) != waited(log)),
        "{logs:?}"
    );
    assert_eq!(baks(&dir).len(), 1);
    assert_eq!(version_of(&dir.database()), V6);
    assert_chats_intact(&open6(&dir).unwrap(), &sessions);
    assert_eq!(
        LocalStore::upgrade_status(&dir).unwrap(),
        UpgradeStatus::Current
    );
}

/// How many openers have marked that they are about to ask for the write lock.
fn locking_marks(dir: &Scratch) -> usize {
    fs::read_dir(&dir.0)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("locking-")
        })
        .count()
}

/// One of the two openers of the two-process test. The one that wins the lock parks at the gate.
#[test]
#[ignore = "child of the two-process test"]
fn upgrade_child() {
    let (Ok(dir), Ok(sync), Ok(name)) = (
        std::env::var(CHILD_DIR),
        std::env::var(CHILD_SYNC),
        std::env::var(CHILD_NAME),
    ) else {
        return;
    };
    let (dir, sync) = (PathBuf::from(dir), PathBuf::from(sync));
    set_fault(&dir, Some(Fault::Gate));
    fs::write(sync.join(format!("{name}.ready")), b"").unwrap();
    let start = Instant::now();
    while !sync.join("go").exists() {
        assert!(start.elapsed() < WAIT, "never told to go");
        std::thread::sleep(Duration::from_millis(5));
    }
    let db = open6(&dir).unwrap();
    assert_eq!(user_version(db.connection()).unwrap(), V6);
}

/// Every row of every v6 table that holds a chat, for comparing before and after.
fn v6_dump(database: &Path) -> Vec<(String, Vec<String>)> {
    let connection = Connection::open(database).unwrap();
    [
        "sessions ORDER BY id",
        "chat_events ORDER BY sid, seq",
        "chat_context ORDER BY session_id",
        "chat_checkpoints ORDER BY session_id, ordinal",
        "chat_search_keys ORDER BY id",
        "quarantined_sessions ORDER BY id",
        "settings ORDER BY key",
    ]
    .iter()
    .map(|table| {
        let mut statement = connection
            .prepare(&format!("SELECT * FROM {table}"))
            .unwrap();
        let columns = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                Ok((0..columns)
                    .map(|i| format!("{:?}", row.get_ref(i).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|"))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        (table.to_string(), rows)
    })
    .collect()
}

fn count(database: &Path, sql: &str, id: &str) -> i64 {
    Connection::open(database)
        .unwrap()
        .query_row(sql, [id], |row| row.get(0))
        .unwrap()
}

/// A v5 window that was open before the upgrade, and a v6 database it now points at.
fn stale_v5_writer(name: &str) -> (Scratch, LocalStore, Vec<Value>) {
    let dir = Scratch::new(name);
    let sessions = v5_sessions();
    build_v5(&dir, &sessions, 5);
    let stale = LocalStore::open(&dir).unwrap();
    drop(open6(&dir).unwrap());
    assert_eq!(version_of(&dir.database()), V6);
    (dir, stale, sessions)
}

#[test]
fn a_v5_writer_left_open_across_the_upgrade_fails_without_touching_v6_rows() {
    let (dir, stale, sessions) = stale_v5_writer("stale-save");
    let before = v6_dump(&dir.database());
    // `save_session` indexes into `session_messages`, which the upgrade dropped (F25).
    let existing = fixture("ids_missing_and_duplicate");
    assert!(stale.save_session(&existing).is_err());
    let mut new = existing.clone();
    new["id"] = json!("a_chat_only_the_stale_window_knows");
    assert!(stale.save_session(&new).is_err());
    assert_eq!(v6_dump(&dir.database()), before);
    assert_chats_intact(&open6(&dir).unwrap(), &sessions);
}

#[test]
fn a_v5_delete_on_v6_cascades_cleanly() {
    let (dir, stale, sessions) = stale_v5_writer("stale-delete");
    let tables = [
        "chat_events",
        "chat_context",
        "chat_checkpoints",
        "chat_search_keys",
    ];
    // A chat's rows by its id, or by its integer key for the tables keyed on it (R6-1). The key
    // is read before the delete: once the row is gone, a lookup by id would find nothing anyway.
    let key = |id: &str| {
        // A chat set aside by the upgrade has no row and no key.
        let sql = "SELECT COALESCE((SELECT key FROM sessions WHERE id=?1), -1)";
        count(&dir.database(), sql, id)
    };
    let rows = |id: &str, key: i64| -> Vec<i64> {
        tables
            .iter()
            .map(|table| {
                // `sid` has integer affinity, so the key compares as a number.
                let (column, value) = match *table {
                    "chat_events" | "chat_search_keys" => ("sid", key.to_string()),
                    _ => ("session_id", id.to_owned()),
                };
                count(
                    &dir.database(),
                    &format!("SELECT COUNT(*) FROM {table} WHERE {column}=?1"),
                    &value,
                )
            })
            .collect()
    };
    // The chat with the most rows, so every table that can hold some does.
    let id = sessions
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .max_by_key(|id| rows(id, key(id)).iter().sum::<i64>())
        .unwrap()
        .to_owned();
    let id = id.as_str();
    let id_key = key(id);
    let before = rows(id, id_key);
    assert!(before[0] > 0 && before[3] > 0, "{id}: {before:?}");
    let others = v6_dump(&dir.database());
    let search_before = count(
        &dir.database(),
        "SELECT COUNT(*) FROM chat_search WHERE ?1=?1",
        id,
    );

    stale.delete_session(id).unwrap();

    assert_eq!(rows(id, id_key), vec![0; tables.len()]);
    assert_eq!(
        count(
            &dir.database(),
            "SELECT COUNT(*) FROM sessions WHERE id=?1",
            id
        ),
        0
    );
    // Its search entries went with its keys, and nothing else did.
    assert_eq!(
        count(
            &dir.database(),
            "SELECT COUNT(*) FROM chat_search WHERE ?1=?1",
            id
        ),
        search_before - before[3]
    );
    let after = v6_dump(&dir.database());
    for ((table, kept), (_, all)) in after.iter().zip(&others) {
        assert!(
            kept.iter().all(|row| all.contains(row)),
            "{table} gained a row"
        );
    }
    let connection = Connection::open(dir.database()).unwrap();
    assert!(connection
        .prepare("PRAGMA foreign_key_check")
        .unwrap()
        .query([])
        .unwrap()
        .next()
        .unwrap()
        .is_none());
    let check: String = connection
        .pragma_query_value(None, "integrity_check", |row| row.get(0))
        .unwrap();
    assert_eq!(check, "ok");
    // Every other chat is whole.
    let rest: Vec<Value> = sessions.into_iter().filter(|s| s["id"] != id).collect();
    assert_chats_intact(&open6(&dir).unwrap(), &rest);
}

/// A regression test for the POSIX lock drop: once the upgrading process has its backup made (its
/// source identified, the backup copied, the state written), the write lock is still its own, and
/// another process cannot take it.
#[test]
fn another_process_cannot_take_the_write_lock_from_an_upgrade_in_progress() {
    let dir = Scratch::new("heldlock");
    build_v5(&dir, &v5_sessions()[..2], 5);
    let sync = Scratch::new("heldlock-sync");
    fs::create_dir_all(&sync.0).unwrap();
    let mut children = Reaper(vec![spawn_child(
        "storage::upgrade::tests::upgrade_child",
        &sync.0.join("a.log"),
        &[
            (CHILD_DIR, dir.to_str().unwrap()),
            (CHILD_SYNC, sync.to_str().unwrap()),
            (CHILD_NAME, "a"),
        ],
    )]);
    wait_for(&sync.0.join("a.ready"), &mut children.0);
    fs::write(sync.0.join("go"), b"").unwrap();
    wait_for(&dir.0.join(backup::GATE_HELD), &mut children.0);
    let other = Connection::open(dir.database()).unwrap();
    other.busy_timeout(Duration::ZERO).unwrap();
    let taken = other.execute_batch("BEGIN IMMEDIATE");
    let _ = other.execute_batch("ROLLBACK");
    drop(other);
    fs::write(dir.0.join(backup::GATE_RELEASE), b"").unwrap();
    let child = children.0.pop().unwrap();
    let status = wait_exit(child);
    let log = fs::read_to_string(sync.0.join("a.log")).unwrap();
    assert!(
        matches!(&taken, Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::DatabaseBusy),
        "another process took the upgrade's write lock: {taken:?}"
    );
    assert!(status.success(), "{status:?}\n{log}");
    assert_eq!(version_of(&dir.database()), V6);
}

/// The production code of `file`: everything before its test module.
fn production(file: &str) -> &str {
    let end = file
        .find("#[cfg(test)]\n#[allow(clippy::unwrap_used)]")
        .or_else(|| file.find("#[cfg(test)]\nmod tests"))
        .unwrap();
    &file[..end]
}

/// The rule behind the POSIX lock fix: the database file is never opened outside SQLite, because
/// on unix closing any descriptor of it drops every lock this process holds on it. Every std open
/// in these three files is listed here, and none of them takes the database path.
#[test]
fn the_database_file_is_never_opened_outside_sqlite() {
    let upgrade = production(include_str!("upgrade.rs"));
    let backup = production(include_str!("backup.rs"));
    let storage = production(include_str!("../storage.rs"));
    let opens = [
        "File::open(",
        "File::options(",
        "File::create(",
        "OpenOptions::new(",
        "fs::read(",
        "fs::read_to_string(",
        "fs::copy(",
        "fs::write(",
        "open_plain(",
        "create_private(",
    ];
    let calls = |code: &str| -> Vec<String> {
        code.lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//") && opens.iter().any(|open| line.contains(open)))
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(
        calls(upgrade),
        Vec::<String>::new(),
        "upgrade.rs opens a file"
    );
    // `open_at` creates the database file exclusively: the open succeeds only for a brand-new
    // file, which no connection can hold a lock on yet, and opens nothing when it exists. The
    // other write is the no-lock marker, not the database.
    assert_eq!(
        calls(storage),
        [
            "match fs::OpenOptions::new()",
            "let _ = fs::write(directory.join(NO_LOCK_MARKER), b\"\");",
        ],
        "a new std open in storage.rs: it must never be the database file"
    );
    assert_eq!(
        calls(backup),
        [
            "fn create_private(path: &Path) -> io::Result<File> {",
            "let mut options = OpenOptions::new();",
            "fn open_plain(path: &Path, write: bool) -> io::Result<File> {",
            "let mut options = OpenOptions::new();",
            "File::open(dir)?.sync_all()",
            "let file = open_plain(&dir.join(STATE_FILE), false).ok()?;",
            "let mut file = create_private(&temp)?;",
            "create_private(partial)?;",
            "open_plain(partial, true)?.sync_all()?;",
            // Test-only: the two-process test's marker files, in the data folder.
            "let _ = fs::write(",
            "let _ = fs::write(data_directory.join(GATE_HELD), b\"\");",
        ],
        "a new std open in backup.rs: it must never be the database file"
    );
    // The state file is written only inside the locked upgrade (`back_up`), and cleared only
    // under the write lock (the post-commit cleanup and the v6 open finishing one a crash cut).
    assert_eq!(upgrade.matches("backup::write_state(").count(), 1);
    assert_eq!(upgrade.matches("backup::clear_state(").count(), 2);
    // The identity is read through SQLite on a connection it is given, and only stat-ed.
    let start = backup.find("pub(crate) fn source_id(").unwrap();
    let body = &backup[start..start + backup[start..].find("\n}\n").unwrap()];
    assert!(body.contains("connection: &Connection"));
    for open in [
        "File",
        "OpenOptions",
        "open_plain",
        "create_private",
        "fs::read",
    ] {
        assert!(!body.contains(open), "source_id uses {open}");
    }
}

#[test]
fn retry_resets_the_count_only_under_the_write_lock() {
    let dir = Scratch::new("retrylock");
    build_v5(&dir, &v5_sessions()[..2], 5);
    set_fault(&dir, Some(Fault::ConvertError(0)));
    for _ in 0..3 {
        failed_backup(open6(&dir));
    }
    let state = dir.backups().join("upgrade-state.json");
    let stopped = fs::read(&state).unwrap();
    // Another window holds the write lock: Retry waits for it, gives up, and leaves the state
    // file exactly as it was, still stopped.
    set_test_busy_timeout(&dir, Some(Duration::from_millis(50)));
    let writer = Connection::open(dir.database()).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(matches!(
        LocalStore::retry_upgrade(&dir),
        Err(StorageError::UpgradeBusy)
    ));
    writer.execute_batch("ROLLBACK").unwrap();
    assert_eq!(fs::read(&state).unwrap(), stopped);
    assert!(matches!(
        LocalStore::upgrade_status(&dir).unwrap(),
        UpgradeStatus::Failed {
            attempts: 3,
            stopped: true,
            ..
        }
    ));
    assert!(matches!(open6(&dir), Err(StorageError::UpgradeStopped(_))));
    // With the lock free, Retry counts this as attempt 1 of a new run: still failing, not stopped.
    let (path, _) = failed_backup(LocalStore::retry_upgrade(&dir));
    assert_eq!(
        LocalStore::upgrade_status(&dir).unwrap(),
        UpgradeStatus::Failed {
            backup: path.display().to_string(),
            attempts: 1,
            stopped: false,
        }
    );
    assert_eq!(baks(&dir).len(), 1, "Retry reuses the backup");
    set_fault(&dir, None);
    drop(LocalStore::retry_upgrade(&dir).unwrap());
    assert_eq!(version_of(&dir.database()), V6);
}
