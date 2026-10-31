//! What a v6 write costs, and which index a read uses (R5-9).
//!
//! Two kinds of check, so the suite stays deterministic on a slow CI box:
//! - Counts decide pass or fail: SQL statements run, rows changed and pages added by an append, and
//!   the `EXPLAIN QUERY PLAN` of every hot read. None of them depends on the clock.
//! - Timings are printed, and fail only at a ceiling 10x the budget (R5-9).
//!
//! The scale is small by default. `BLACKWALL_PERF_SCALE=full` runs 800 chats of 300 messages; run
//! it with `--release --nocapture` to see the numbers.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::{
    chats::{
        append, append_model, append_run_start, append_ui_assistant, append_ui_user,
        counter_repairs, read_counters, statements, v6_in_memory, write_context, write_counters,
        write_tx, Counters, COUNTERS_SQL, EVENTS_SQL, INSERT_EVENT_SQL, LIST_SQL,
    },
    convert::upgrade_inline_values,
    fixtures::{build_v5, long_chat},
    search::{index_message, search_v6},
    view::{ContextRow, Kind},
    LocalStore, NewSession, RecallQuery, DATABASE_FILE,
};
use crate::memory::MemoryClient;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

/// Every timing is the median of this many runs.
const RUNS: usize = 20;
/// R4-6 budgeted 50 ms to open a 300-message chat and 5 ms to append; R5-9 allows 10x.
const OPEN_CEILING: Duration = Duration::from_millis(500);
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
/// row. That is two reserved UI messages.
fn begin_run(tx: &Connection, id: &str, c: &mut Counters, n: usize) {
    let user = append_ui_user(tx, id, c, &user_message(n)).unwrap();
    append_model(tx, id, c, &json!({ "ref": user })).unwrap();
    append_run_start(tx, id, c, &format!("a{n}"), 6, user).unwrap();
}

/// A chat of `ui_messages` finished UI messages: user rows, each with its `model` ref, answered by
/// a standalone assistant row.
fn chat_with_history(db: &LocalStore, id: &str, ui_messages: usize) {
    make(db, id);
    write(db, id, |tx, c| {
        for n in 0..ui_messages / 2 {
            let user = append_ui_user(tx, id, c, &user_message(n))?;
            append_model(tx, id, c, &json!({ "ref": user }))?;
            append_ui_assistant(
                tx,
                id,
                c,
                &json!({"id": format!("r{n}"), "role": "assistant", "content": format!("answer {n}"),
                        "attachments": [], "status": "complete", "createdAt": 7}),
            )?;
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
    // Straight into the table: the full-scale database already holds more than the 500 sessions
    // `create_session` allows, because the upgrade does not enforce that limit.
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

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

fn ms(time: Duration) -> f64 {
    time.as_secs_f64() * 1000.0
}

// ---- the append costs the same at any history length ----

#[test]
fn an_append_costs_the_same_at_any_history_length() {
    // 1,998 is the most a chat can hold before a run starts (the cap is 2,000 and a run reserves 2).
    let mut costs = vec![];
    for history in [10usize, 300, 1_998] {
        let db = v6_in_memory();
        let id = "s";
        chat_with_history(&db, id, history);
        let ui_before = read_counters(db.connection(), id).unwrap().ui_count;
        let (statements_before, changes_before, pages_before) =
            (statements(), total_changes(&db), pragma(&db, "page_count"));
        write(&db, id, |tx, c| {
            begin_run(tx, id, c, history);
            Ok(())
        });
        let statements_after = statements();
        let (changes_after, pages_after) = (total_changes(&db), pragma(&db, "page_count"));
        let ui_after = read_counters(db.connection(), id).unwrap().ui_count;
        assert_eq!(
            ui_before, history as u64,
            "the history is {history} UI messages"
        );
        assert_eq!(ui_after, history as u64 + 2);
        let cost = (
            statements_after - statements_before,
            changes_after - changes_before,
            pages_after - pages_before,
        );
        // The row and page counts are what the write touches, whatever the chat holds.
        assert!(cost.2 <= 4, "{history} messages: {} new pages", cost.2);
        costs.push((history, cost));
    }
    println!("append cost (statements, rows changed, pages added): {costs:?}");
    for (history, (statements, changes, _)) in &costs {
        assert_eq!(
            (*statements, *changes),
            (costs[0].1 .0, costs[0].1 .1),
            "an append at {history} messages ran a different number of statements or changes"
        );
    }
}

// ---- the reads use their indexes ----

#[test]
fn load_list_and_counter_reads_use_their_indexes() {
    let db = v6_in_memory();
    chat_with_history(&db, "s", 20);
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
fn perf_scale() -> (usize, usize) {
    match std::env::var("BLACKWALL_PERF_SCALE").as_deref() {
        Ok("full") => (800, 300),
        _ => (24, 300),
    }
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn count(db: &LocalStore, sql: &str) -> i64 {
    db.connection().query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn large_history_v6_timings_are_reported_and_bounded() {
    let (chats, per_chat) = perf_scale();
    // A few distinct generated chats, copied under new ids: building 800 distinct ones would
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

    let directory = Scratch(
        std::env::temp_dir().join(format!("blackwall-perf-{:016x}", rand::random::<u64>())),
    );
    let path = build_v5(&directory.0, &bodies, 5);
    let v5_file = fs::metadata(&path).unwrap().len();
    let started = Instant::now();
    let db = LocalStore::open_at(&directory.0, 6).unwrap();
    let upgrade = started.elapsed();
    assert_eq!(count(&db, "SELECT count(*) FROM sessions"), chats as i64);
    assert_eq!(count(&db, "SELECT count(*) FROM quarantined_sessions"), 0);
    assert_eq!(upgrade_inline_values(), 0);
    let db_path = directory.0.join(DATABASE_FILE);
    let v6_file = fs::metadata(&db_path).unwrap().len();
    let page_size = pragma(&db, "page_size") as u64;
    let live = (pragma(&db, "page_count") - pragma(&db, "freelist_count")) as u64 * page_size;
    // What the log holds, and how much of it is references rather than copies.
    let events = count(&db, "SELECT count(*) FROM chat_events");
    let progress = count(
        &db,
        "SELECT count(*) FROM chat_events WHERE kind='ui_progress'",
    );
    let refs = count(
        &db,
        "SELECT count(*) FROM chat_events WHERE kind='model' AND body LIKE '{\"ref\":%'",
    );
    let event_bytes = count(&db, "SELECT sum(length(body)) FROM chat_events") as u64;

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
        chat_with_runs(&db, &id, runs);
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
        write(&db, id, |tx, c| {
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
    let listed = (count(&db, "SELECT count(*) FROM sessions") as usize).min(500);
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
    let found = search_v6(&db, MemoryClient::Desktop, &query).unwrap();
    let search = started.elapsed();
    let search_statements = statements() - before;
    assert!(!found.results.is_empty());
    let hit_chats = found
        .results
        .iter()
        .map(|hit| hit.session_id.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();

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

    println!(
        "perf scale: {chats} chats x {per_chat} UI messages ({events} events: {progress} progress, {refs} model refs)\n\
         upgrade: {:.0} ms total\n\
         open a {per_chat}-message chat: median of {RUNS} {:.2} ms ({} statements)\n\
         open a live-run chat, median of {RUNS}: {} runs {:.2} ms, {} runs {:.2} ms ({} statements each)\n\
         append (user + model ref + start + search entry): median of {RUNS} {:.2} ms ({} statements)\n\
         list ({} rows): median of {RUNS} {:.2} ms\n\
         search (limit 10): {:.2} ms, {search_statements} statements for {} hits in {hit_chats} chats\n\
         size: v5 file {v5_file} B, v6 file {v6_file} B, v6 live pages {live} B\n\
         raw: message text {text_bytes} B, v5 bodies {v5_body_bytes} B, v6 event bodies {event_bytes} B\n\
         ratios: v6 live / text {:.2}, v6 file / text {:.2}, v6 live / v5 bodies {:.2}",
        ms(upgrade),
        ms(open),
        open_statements[0],
        native[0].1,
        ms(native[0].2),
        native[1].1,
        ms(native[1].2),
        native[0].3,
        ms(append),
        append_statements[0],
        listed,
        ms(list_time),
        ms(search),
        found.results.len(),
        live as f64 / text_bytes as f64,
        v6_file as f64 / text_bytes as f64,
        live as f64 / v5_body_bytes as f64,
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
}
