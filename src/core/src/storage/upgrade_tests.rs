use super::*;
use crate::memory::MemoryClient;
use crate::storage::backup::{set_fault, Fault, BACKUPS_DIR};
use crate::storage::fixtures::{
    build_v5, golden, master, platform_neutral, v5_sessions, RECALL_QUERIES,
};
use crate::storage::search::search_v6;
use crate::storage::{RecallQuery, BUSY_TIMEOUT, DATABASE_FILE};
use serde_json::{json, Map, Value};
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
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
            backup: path.display().to_string()
        }),
        "{notices:?}"
    );
    assert!(path.is_file(), "the backup to restore from stays");
}
