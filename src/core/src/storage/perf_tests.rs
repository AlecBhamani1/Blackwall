//! What a v6 write costs, and which index a read uses (R5-9).
//!
//! Two kinds of check, so the suite stays deterministic on a slow CI box:
//! - Counts decide pass or fail: SQL statements run, rows changed and pages added by an append, and
//!   the `EXPLAIN QUERY PLAN` of every hot read. None of them depends on the clock.
//! - Timings are printed, and fail only at a ceiling 10x the budget (R5-9).
//!
//! - Sizes are asserted against a v5 file built the real way, with its recall index (R6-4).
//!
//! The scale is small by default. `BLACKWALL_PERF_SCALE=full` runs 500 chats of 300 messages (500
//! is the most a v5 file holds) and 500 simulated chats; run it with `--release --nocapture` to
//! see the numbers. On CI every report is also added to the job's summary page.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::{
    chats::{
        append, append_model, append_run_start, append_ui_assistant, append_ui_user,
        counter_repairs, read_counters, statements, v6_in_memory, write_context, write_counters,
        write_tx, Counters, COUNTERS_SQL, EVENTS_SQL, INSERT_EVENT_SQL, LIST_SQL,
    },
    convert::upgrade_inline_values,
    fixtures::{build_v5_indexed, long_chat},
    property_tests::simulate_v5,
    search::{index_message, search_v6},
    upgrade::set_test_free_disk,
    view::{ContextRow, Kind},
    LocalStore, NewSession, RecallQuery, DATABASE_FILE,
};
use crate::memory::MemoryClient;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

/// Every timing is the median of this many runs.
const RUNS: usize = 20;
/// R4-6 budgeted 50 ms to open a 300-message chat and 5 ms to append; R5-9 allows 10x. An open
/// only reads, so its ceiling is the same on every OS (Windows CI measured about 12 ms in debug).
const OPEN_CEILING: Duration = Duration::from_millis(500);
/// An append commits under `synchronous=FULL`. On Windows that commit is dominated by
/// `FlushFileBuffers` on the journal and the database (CI measured 109 ms in debug while the
/// statement, change, page and VM-op counts stayed the same as elsewhere), so the ceiling there
/// is 500 ms. The counts above decide whether an append's work grew; this only catches a
/// pathology.
#[cfg(windows)]
const APPEND_CEILING: Duration = Duration::from_millis(500);
#[cfg(not(windows))]
const APPEND_CEILING: Duration = Duration::from_millis(50);

// ---- helpers ----

fn make(db: &LocalStore, id: &str) {
    db.create_session(&NewSession {
        id: id.into(),
        title: format!("Chat {id}"),
        mode: None,
        workspace: None,
        updated_at: 1_000,
    })
    .unwrap();
}

/// One write transaction over the session's counters, as a real writer runs it.
fn write<R>(
    db: &LocalStore,
    id: &str,
    f: impl FnOnce(&Connection, &mut Counters) -> Result<R, super::StorageError>,
) -> R {
    let tx = write_tx(db.connection()).unwrap();
    let mut counters = read_counters(&tx, id).unwrap();
    let result = f(&tx, &mut counters).unwrap();
    write_counters(&tx, id, &counters, 0).unwrap();
    tx.commit().unwrap();
    result
}

fn user_message(n: usize) -> Value {
    json!({"id": format!("u{n}"), "role": "user", "content": format!("question number {n} about retries"),
           "attachments": [], "status": "complete", "createdAt": 5})
}

/// What starting a run writes: the user row, a `model` row that refers to it, and the run's start
/// row. That is two reserved UI messages. Returns the user row's seq.
fn begin_run(tx: &Connection, id: &str, c: &mut Counters, n: usize) -> i64 {
    let user = append_ui_user(tx, id, c, &user_message(n)).unwrap();
    append_model(tx, id, c, &json!({ "ref": user })).unwrap();
    append_run_start(tx, id, c, &format!("a{n}"), 6, user).unwrap();
    user
}

/// A chat of `ui_messages` finished UI messages: user rows, each with its `model` ref, answered by
/// a standalone assistant row. With `indexed`, each message also gets its search entry, as every
/// writer adds it.
fn chat_with_history(db: &LocalStore, id: &str, ui_messages: usize, indexed: bool) {
    make(db, id);
    write(db, id, |tx, c| {
        for n in 0..ui_messages / 2 {
            let (message, at) = (user_message(n), c.ui_count);
            let user = append_ui_user(tx, id, c, &message)?;
            append_model(tx, id, c, &json!({ "ref": user }))?;
            let answer = json!({"id": format!("r{n}"), "role": "assistant", "content": format!("answer {n}"),
                    "attachments": [], "status": "complete", "createdAt": 7});
            let reply = append_ui_assistant(tx, id, c, &answer)?;
            if indexed {
                index_message(tx, id, c, user, at, &message)?;
                index_message(tx, id, c, reply, at + 1, &answer)?;
            }
        }
        Ok(())
    });
}

/// A chat of `runs` finished runs written the way a live recorder writes them: each has its user
/// row, a `model` ref to it, a start row, `PROGRESS_ROWS` tool deltas and a terminal that holds
/// only the answer, which a second `model` ref points at. The context row covers every model row,
/// so a load resolves a ref to each finished run's assistant message, refolding that run's
/// progress rows each time.
const PROGRESS_ROWS: usize = 4;
fn chat_with_runs(db: &LocalStore, id: &str, runs: usize) {
    // Straight into the table: the full-scale database already holds the 500 sessions
    // `create_session` allows.
    db.connection()
        .execute(
            "INSERT INTO sessions(id,title,updated_at,body) VALUES (?1,?1,1000,'')",
            [id],
        )
        .unwrap();
    write(db, id, |tx, c| {
        for n in 0..runs {
            // The search allowance is derived from the messages, so a consistent chat indexes them.
            let (message, at) = (user_message(n), c.ui_count);
            let user = append_ui_user(tx, id, c, &message)?;
            index_message(tx, id, c, user, at, &message)?;
            append_model(tx, id, c, &json!({ "ref": user }))?;
            let start = append_run_start(tx, id, c, &format!("a{n}"), 6, user)?;
            for k in 0..PROGRESS_ROWS {
                let delta = json!({"run": start, "reasoning": format!("thinking about step {n}.{k}"),
                    "tools": [{"id": format!("t{n}_{k}"), "name": "read_file", "status": "complete",
                               "arguments": "{\"path\":\"src/main.rs\"}", "output": "fn main() {}\n"}]});
                append(tx, id, c, Kind::UiProgress, &delta)?;
            }
            let terminal = append_ui_assistant(
                tx,
                id,
                c,
                &json!({"run": start, "content": format!("answer {n}"), "status": "complete"}),
            )?;
            let answer = json!({"id": format!("a{n}"), "role": "assistant", "content": format!("answer {n}")});
            index_message(tx, id, c, terminal, c.ui_count - 1, &answer)?;
            append_model(tx, id, c, &json!({ "ref": terminal }))?;
        }
        let ctx = ContextRow {
            model_from: 0,
            model_len: c.model_count,
            covered_messages: c.ui_count,
            source_hash: "0".repeat(64),
            server_usage: None,
        };
        write_context(tx, id, c, &ctx, &[], false)
    });
}

fn pragma(db: &LocalStore, name: &str) -> i64 {
    db.connection()
        .query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
        .unwrap()
}

fn total_changes(db: &LocalStore) -> i64 {
    db.connection()
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap()
}

/// The plan of `sql` as one string. Tests match on its stable words ("SEARCH", "USING INDEX x"),
/// never on a whole line: the wording of a plan changes between SQLite versions.
fn plan(db: &LocalStore, sql: &str, args: &[&str]) -> String {
    let mut statement = db
        .connection()
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap();
    let rows = statement
        .query_map(rusqlite::params_from_iter(args), |r| r.get::<_, String>(3))
        .unwrap();
    rows.collect::<Result<Vec<_>, _>>().unwrap().join(" | ")
}

/// Prints `body` under `title` and, when `$GITHUB_STEP_SUMMARY` is set (every CI step), adds it to
/// the job's summary page, so the numbers show without opening the log. A summary that cannot be
/// written is only logged: it never fails a test.
fn report(title: &str, body: &str) {
    let build = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let text = format!(
        "### {title} ({}, {build} build)\n\n{body}\n\n",
        std::env::consts::OS
    );
    println!("{text}");
    let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") else {
        return;
    };
    // One append per report, so reports from tests running in parallel never interleave.
    let written = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()));
    if let Err(error) = written {
        eprintln!("could not add the report to the step summary: {error}");
    }
}

fn mib(bytes: u64) -> String {
    format!("{:.2} MiB", bytes as f64 / f64::from(1 << 20))
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

fn ms(time: Duration) -> f64 {
    time.as_secs_f64() * 1000.0
}

// ---- the append costs the same at any history length ----

/// What one append cost: statements prepared through `stmt`, rows changed (SQLite's
/// `total_changes`, which counts the search index's own shadow-table writes too), pages added, and
/// SQLite VM progress callbacks (one per instruction-count check, inside every statement SQLite
/// runs, the FTS5 module's own included).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cost {
    statements: u64,
    changes: i64,
    pages: i64,
    vm_ops: u64,
}

/// Begins a run in a chat of `history` UI messages and measures it, for 10, 300 and 1,998 (the
/// most a chat can hold before a run starts: the cap is 2,000 and a run reserves 2). With
/// `indexed`, the history has its search entries and the append adds the user message's, as the
/// recorder does. The costs must not depend on the history.
fn append_costs(indexed: bool) -> Vec<(usize, Cost)> {
    let mut costs = vec![];
    for history in [10usize, 300, 1_998] {
        let mut db = v6_in_memory();
        let id = "s";
        chat_with_history(&db, id, history, indexed);
        let ui_before = read_counters(db.connection(), id).unwrap().ui_count;
        assert_eq!(
            ui_before, history as u64,
            "the history is {history} UI messages"
        );
        let ops = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&ops);
        db.connection
            .progress_handler(
                1,
                Some(move || {
                    counter.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            )
            .unwrap();
        let before = (statements(), total_changes(&db), pragma(&db, "page_count"));
        let ops_before = ops.load(Ordering::Relaxed);
        write(&db, id, |tx, c| {
            let at = c.ui_count;
            let user = begin_run(tx, id, c, history);
            if indexed {
                index_message(tx, id, c, user, at, &user_message(history))?;
            }
            Ok(())
        });
        let vm_ops = ops.load(Ordering::Relaxed) - ops_before;
        db.connection
            .progress_handler(0, None::<fn() -> bool>)
            .unwrap();
        let cost = Cost {
            statements: statements() - before.0,
            changes: total_changes(&db) - before.1,
            pages: pragma(&db, "page_count") - before.2,
            vm_ops,
        };
        assert_eq!(
            read_counters(db.connection(), id).unwrap().ui_count,
            history as u64 + 2
        );
        costs.push((history, cost));
    }
    costs
}

fn assert_flat(costs: &[(usize, Cost)], max_pages: i64) {
    let first = costs[0].1;
    for (history, cost) in costs {
        // The row and page counts are what the write touches, whatever the chat holds.
        assert!(
            cost.pages <= max_pages,
            "{history} messages: {} new pages",
            cost.pages
        );
        assert_eq!(
            (cost.statements, cost.changes, cost.vm_ops),
            (first.statements, first.changes, first.vm_ops),
            "an append at {history} messages did different work: {costs:?}"
        );
    }
}

#[test]
fn an_append_costs_the_same_at_any_history_length() {
    let costs = append_costs(false);
    println!("append cost: {costs:?}");
    assert_flat(&costs, 4);
}

/// A12 review: the same, with the search entry an append also writes, which `stmt` cannot see
/// into (FTS5 runs its own statements). `total_changes` and the VM count do see it.
///
/// Not covered: FTS5's segment merges. `chat_with_history` writes the whole history in one
/// transaction, so the index holds one segment here, and no append in this test triggers an
/// automerge. A merge's cost depends on the index, not on the chat's length, and lands on the
/// append that triggers it.
#[test]
fn an_append_with_its_search_entry_costs_the_same_at_any_history_length() {
    let costs = append_costs(true);
    println!("indexed append cost: {costs:?}");
    let bare = append_costs(false);
    assert!(
        costs[0].1.changes > bare[0].1.changes && costs[0].1.vm_ops > bare[0].1.vm_ops,
        "the search entry is in the measure: {costs:?} vs {bare:?}"
    );
    assert_flat(&costs, 6);
}

// ---- the reads use their indexes ----

#[test]
fn load_list_and_counter_reads_use_their_indexes() {
    let db = v6_in_memory();
    chat_with_history(&db, "s", 20, false);
    make(&db, "t");

    // Load: a range seek on the (sid, seq) unique index, already in order (R6-1).
    let events_index: String = db
        .connection()
        .query_row(
            "SELECT name FROM pragma_index_list('chat_events') WHERE origin='u'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let load = plan(&db, EVENTS_SQL, &["1"]);
    assert!(load.contains("SEARCH chat_events"), "{load}");
    assert!(load.contains(&format!("INDEX {events_index}")), "{load}");
    assert!(load.contains("sid=?"), "{load}");
    assert!(!load.contains("SCAN"), "{load}");
    assert!(
        !load.contains("TEMP B-TREE"),
        "{load}: the index is the order"
    );
    // An append finds the session's key with one seek on `sessions.id`.
    let insert = plan(&db, INSERT_EVENT_SQL, &["s", "0", "model", "{}"]);
    assert!(insert.contains("SEARCH sessions"), "{insert}");
    assert!(!insert.contains("SCAN"), "{insert}");

    // Counters: a seek on the unique index SQLite builds for `sessions.id TEXT PRIMARY KEY`.
    let autoindex: String = db
        .connection()
        .query_row(
            "SELECT name FROM pragma_index_list('sessions') WHERE origin='pk'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(autoindex, "sqlite_autoindex_sessions_1");
    let counters = plan(&db, COUNTERS_SQL, &["s"]);
    assert!(counters.contains("SEARCH sessions"), "{counters}");
    assert!(
        counters.contains(&format!("USING INDEX {autoindex}")),
        "{counters}"
    );
    assert!(counters.contains("id=?"), "{counters}");
    assert!(!counters.contains("SCAN"), "{counters}");

    // List: newest first from `sessions_updated_at`, never from `chat_events`, with no sort.
    let index_exists: i64 = db
        .connection()
        .query_row(
            "SELECT count(*) FROM pragma_index_list('sessions') WHERE name='sessions_updated_at'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(index_exists, 1);
    let list = plan(&db, LIST_SQL, &[]);
    // SQLite may read it as a covering index when every listed column is in it.
    assert!(
        list.contains("USING INDEX sessions_updated_at")
            || list.contains("USING COVERING INDEX sessions_updated_at"),
        "{list}"
    );
    assert!(!list.contains("chat_events"), "{list}");
    assert!(!list.contains("TEMP B-TREE"), "{list}");
}

// ---- large history, timed ----

/// (chats, UI messages per chat). Small by default; `BLACKWALL_PERF_SCALE=full` is the PR run.
/// 500 chats is the most a v5 file can hold, so it is the biggest real upgrade.
fn perf_scale() -> (usize, usize) {
    match std::env::var("BLACKWALL_PERF_SCALE").as_deref() {
        Ok("full") => (500, 300),
        _ => (24, 300),
    }
}

fn full_scale() -> bool {
    std::env::var("BLACKWALL_PERF_SCALE").as_deref() == Ok("full")
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        Self(std::env::temp_dir().join(format!("blackwall-{name}-{:016x}", rand::random::<u64>())))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        set_test_free_disk(&self.0, None);
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn count(db: &LocalStore, sql: &str) -> i64 {
    db.connection().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// A v5 file upgraded the way an open does it, measured in its two steps.
struct Upgraded {
    db: LocalStore,
    /// The open that upgrades, with its VACUUM held off by a low-disk reading.
    upgrade: Duration,
    v6_file: u64,
    /// The next open, which runs the VACUUM the upgrade owed before anything else.
    vacuum: Duration,
    vacuumed_file: u64,
}

fn upgrade_then_vacuum(directory: &Path) -> Upgraded {
    // `metadata` is a `stat`: it never opens the database file.
    let size = || fs::metadata(directory.join(DATABASE_FILE)).unwrap().len();
    set_test_free_disk(directory, Some(0));
    let started = Instant::now();
    let db = LocalStore::open_at(directory, 6).unwrap();
    let upgrade = started.elapsed();
    drop(db);
    let v6_file = size();
    set_test_free_disk(directory, None);
    let started = Instant::now();
    let db = LocalStore::open_at(directory, 6).unwrap();
    let vacuum = started.elapsed();
    assert_eq!(
        pragma(&db, "freelist_count"),
        0,
        "the second open ran the owed VACUUM"
    );
    Upgraded {
        db,
        upgrade,
        v6_file,
        vacuum,
        vacuumed_file: size(),
    }
}

/// One dataset's sizes, as the PR reports them.
fn size_rows(name: &str, v5: u64, up: &Upgraded) -> String {
    format!(
        "| {name} | {} | {} | {} | {:.2} | {:.0} ms | {:.0} ms |",
        mib(v5),
        mib(up.v6_file),
        mib(up.vacuumed_file),
        up.vacuumed_file as f64 / v5 as f64,
        ms(up.upgrade),
        ms(up.vacuum),
    )
}

const SIZE_HEADER: &str =
    "| data | v5 indexed | v6 | v6 vacuumed | vacuumed / v5 | upgrade | VACUUM |\n\
                           |---|---:|---:|---:|---:|---:|---:|";

#[test]
fn large_history_v6_timings_are_reported_and_bounded() {
    let (chats, per_chat) = perf_scale();
    // A few distinct generated chats, copied under new ids: building 500 distinct ones would
    // measure the generator.
    let templates: Vec<Value> = (0..chats.min(8))
        .map(|seed| long_chat("template", seed as u64, per_chat / 2))
        .collect();
    let bodies: Vec<Value> = (0..chats)
        .map(|n| {
            let mut body = templates[n % templates.len()].clone();
            body["id"] = json!(format!("perf_{n:04}"));
            body["updatedAt"] = json!(1_700_000_000_000u64 + n as u64);
            body
        })
        .collect();
    let text_bytes: usize = bodies
        .iter()
        .flat_map(|b| b["messages"].as_array().unwrap())
        .map(|m| m["content"].as_str().unwrap().len())
        .sum();
    let v5_body_bytes: usize = bodies
        .iter()
        .map(|b| serde_json::to_string(b).unwrap().len())
        .sum();

    // The v5 file as users have it: saved by v5 itself, with its recall index (R6-4).
    let directory = Scratch::new("perf");
    let v5_file = fs::metadata(build_v5_indexed(&directory.0, &bodies))
        .unwrap()
        .len();
    let upgraded = upgrade_then_vacuum(&directory.0);
    let db = &upgraded.db;
    assert_eq!(count(db, "SELECT count(*) FROM sessions"), chats as i64);
    assert_eq!(count(db, "SELECT count(*) FROM quarantined_sessions"), 0);
    assert_eq!(upgrade_inline_values(), 0);
    // What the log holds, and how much of it is references rather than copies.
    let events = count(db, "SELECT count(*) FROM chat_events");
    let progress = count(
        db,
        "SELECT count(*) FROM chat_events WHERE kind='ui_progress'",
    );
    let refs = count(
        db,
        "SELECT count(*) FROM chat_events WHERE kind='model' AND body LIKE '{\"ref\":%'",
    );
    let event_bytes = count(db, "SELECT sum(length(body)) FROM chat_events") as u64;

    // Open a 300-message chat: the same call the app makes. It rebuilds the screen and the model
    // context, which resolves every `model` ref (a ref to a finished run's assistant message
    // refolds that run's progress rows); that is all in memory, so the statement count is fixed.
    let ids: Vec<String> = (0..RUNS)
        .map(|k| format!("perf_{:04}", k * chats / RUNS))
        .collect();
    let mut open_times = vec![];
    let mut open_statements = vec![];
    for id in &ids {
        let before = statements();
        let started = Instant::now();
        let chat = db.load_json_v6(id).unwrap().unwrap();
        open_times.push(started.elapsed());
        open_statements.push(statements() - before);
        assert_eq!(chat["messages"].as_array().unwrap().len(), per_chat);
    }
    assert!(
        open_statements.iter().all(|n| *n == open_statements[0]),
        "{open_statements:?}"
    );
    let open = median(open_times);

    // The same, for chats written as live runs, with progress rows to refold. The largest chat a
    // run can start in holds 1,998 UI messages.
    let mut native = vec![];
    for (name, runs) in [("native", per_chat / 2), ("largest", 999)] {
        let id = format!("{name}_chat");
        chat_with_runs(db, &id, runs);
        let mut times = vec![];
        let mut used = vec![];
        for _ in 0..RUNS {
            let before = statements();
            let started = Instant::now();
            let chat = db.load_json_v6(&id).unwrap().unwrap();
            times.push(started.elapsed());
            used.push(statements() - before);
            assert_eq!(chat["messages"].as_array().unwrap().len(), runs * 2);
            assert_eq!(
                chat["contextState"]["transcript"].as_array().unwrap().len(),
                runs * 2
            );
        }
        assert!(used.iter().all(|n| *n == used[0]), "{used:?}");
        assert_eq!(
            used[0], open_statements[0],
            "{name}: loads cost the same statements"
        );
        native.push((name, runs, median(times), used[0]));
    }

    // Append: begin a run on a different chat each time, with its search entry, as a writer does.
    let mut append_times = vec![];
    let mut append_statements = vec![];
    for (k, id) in ids.iter().enumerate() {
        let before = statements();
        let started = Instant::now();
        write(db, id, |tx, c| {
            let index = c.ui_count;
            let message = user_message(10_000 + k);
            let user = append_ui_user(tx, id, c, &message)?;
            index_message(tx, id, c, user, index, &message)?;
            append_model(tx, id, c, &json!({ "ref": user }))?;
            append_run_start(tx, id, c, &format!("a{k}"), 6, user)?;
            Ok(())
        });
        append_times.push(started.elapsed());
        append_statements.push(statements() - before);
    }
    assert!(
        append_statements.iter().all(|n| *n == append_statements[0]),
        "{append_statements:?}"
    );
    let append = median(append_times);

    // The list, from typed columns only.
    let listed = (count(db, "SELECT count(*) FROM sessions") as usize).min(500);
    let mut list_times = vec![];
    for _ in 0..RUNS {
        let before = statements();
        let started = Instant::now();
        let list = db.list_v6().unwrap();
        list_times.push(started.elapsed());
        assert_eq!(statements() - before, 1);
        assert_eq!(list.len(), listed);
    }
    let list_time = median(list_times);

    // Search rebuilds each hit's chat with `load_chat`: count what that costs at the 10-hit limit.
    let query = RecallQuery {
        query: "retries".into(),
        limit: Some(10),
        ..Default::default()
    };
    let before = statements();
    let started = Instant::now();
    let found = search_v6(db, MemoryClient::Desktop, &query).unwrap();
    let search = started.elapsed();
    let search_statements = statements() - before;
    assert!(!found.results.is_empty());
    let hit_chats = found
        .results
        .iter()
        .map(|hit| hit.session_id.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();

    // Delete: a chat, its rows by the cascade, its search entries, and the FTS5 `optimize` that
    // `delete_v6` runs after removing them. Reported only. Every timed chat but the first, which
    // the check below still reads.
    let mut delete_times = vec![];
    for id in &ids[1..] {
        let indexed = count(
            db,
            &format!(
                "SELECT count(*) FROM chat_search_keys WHERE sid=(SELECT key FROM sessions WHERE id='{id}')"
            ),
        );
        assert!(indexed > 0, "{id} has search entries to delete");
        let started = Instant::now();
        db.delete_v6(id).unwrap();
        delete_times.push(started.elapsed());
    }
    let delete = median(delete_times);

    // Lists never read a body: break every one and the list is the same.
    let list_before = db.list_v6().unwrap();
    db.connection()
        .execute("UPDATE chat_events SET body='not json'", [])
        .unwrap();
    assert_eq!(db.list_v6().unwrap(), list_before);
    assert!(
        db.load_json_v6("perf_0000").is_err(),
        "the bodies really are broken"
    );

    report(
        &format!("v6 storage timings: {chats} chats x {per_chat} UI messages"),
        &format!(
            "{events} events: {progress} progress rows, {refs} model refs\n\n\
             | measure | value |\n|---|---:|\n\
             | upgrade, without its VACUUM | {:.0} ms |\n\
             | VACUUM, at the next open | {:.0} ms |\n\
             | open a {per_chat}-message chat (median of {RUNS}) | {:.2} ms, {} statements |\n\
             | open a live-run chat of {} runs | {:.2} ms |\n\
             | open a live-run chat of {} runs | {:.2} ms |\n\
             | append: user + model ref + start + search entry | {:.2} ms, {} statements |\n\
             | list, {listed} rows | {:.2} ms |\n\
             | search, limit 10 | {:.2} ms, {search_statements} statements, {} hits in {hit_chats} chats |\n\
             | delete a chat with its search entries, then `optimize` (median of {}) | {:.2} ms |\n\n\
             {SIZE_HEADER}\n{}\n\n\
             message text {}, v5 bodies {}, v6 event bodies {}; vacuumed v6 / text {:.2}",
            ms(upgraded.upgrade),
            ms(upgraded.vacuum),
            ms(open),
            open_statements[0],
            native[0].1,
            ms(native[0].2),
            native[1].1,
            ms(native[1].2),
            ms(append),
            append_statements[0],
            ms(list_time),
            ms(search),
            found.results.len(),
            ids.len() - 1,
            ms(delete),
            size_rows("synthetic", v5_file, &upgraded),
            mib(text_bytes as u64),
            mib(v5_body_bytes as u64),
            mib(event_bytes),
            upgraded.vacuumed_file as f64 / text_bytes as f64,
        ),
    );

    assert!(open <= OPEN_CEILING, "open took {:.1} ms", ms(open));
    assert!(
        native[0].2 <= OPEN_CEILING,
        "opening a {}-run chat took {:.1} ms",
        native[0].1,
        ms(native[0].2)
    );
    assert!(append <= APPEND_CEILING, "append took {:.1} ms", ms(append));
    assert_eq!(counter_repairs(), 0);
    // R6-4: after its VACUUM, v6 is no bigger than the v5 file it came from.
    assert!(
        upgraded.vacuumed_file <= v5_file,
        "v6 {} vs v5 {}",
        upgraded.vacuumed_file,
        v5_file
    );
}

/// R6-4 on chats the real agent makes (`simulate_v5`): tools, compactions, crashes mid-run and
/// replies of a few KiB, which is where the old layout spilled onto overflow pages.
#[test]
fn v6_after_its_vacuum_is_no_larger_than_an_indexed_v5_on_simulated_chats() {
    let seeds: u64 = if full_scale() { 500 } else { 40 };
    let bodies: Vec<Value> = (0..seeds).map(|seed| simulate_v5(seed).0).collect();
    let directory = Scratch::new("perf-sim");
    let v5_file = fs::metadata(build_v5_indexed(&directory.0, &bodies))
        .unwrap()
        .len();
    let upgraded = upgrade_then_vacuum(&directory.0);
    assert_eq!(
        count(&upgraded.db, "SELECT count(*) FROM sessions"),
        seeds as i64
    );
    assert_eq!(
        count(&upgraded.db, "SELECT count(*) FROM quarantined_sessions"),
        0
    );
    for body in &bodies {
        let id = body["id"].as_str().unwrap();
        assert_eq!(
            &upgraded.db.load_json_v6(id).unwrap().unwrap(),
            body,
            "{id}"
        );
    }
    report(
        &format!("v6 storage size: {seeds} simulated chats"),
        &format!(
            "{SIZE_HEADER}\n{}",
            size_rows("simulate_v5", v5_file, &upgraded)
        ),
    );
    assert!(
        upgraded.vacuumed_file <= v5_file,
        "v6 {} vs v5 {}",
        upgraded.vacuumed_file,
        v5_file
    );
}
