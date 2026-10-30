//! Generated v5 chats, produced by the real agent, must upgrade to v6 and load back exactly,
//! with every text stored once (R4-7).
//!
//! `simulate_v5` runs the real `Agent::run_context_with_guidance` against a scripted model, then
//! saves the way each client did: the desktop at send, at each `ContextUpdated` and at the end
//! (`store.ts` `saveCurrentSession`); the CLI at send and at the end (`session.rs` `run`). A run
//! ends complete, stopped, failed or crashed, and every fourth run is a `/compact`. The UI rows
//! come from `RunFold`, the model source from `run_source`.
//!
//! The sessions are deterministic from their seed, so a failure names the seed to replay.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::{
    chats::{v6_in_memory, write_tx},
    convert::{convert_session_in_tx, validate_session, ConvertOutcome},
    fixtures::{ScriptedBackend, Step, WORKSPACE},
    LocalStore,
};
use crate::{
    agent::{Agent, AgentError},
    approvals::Approvals,
    context::{ContextRun, ContextState},
    model::{ModelError, ModelTurn, ToolCall, ToolFunction},
    protocol::{AgentEvent, TokenUsage},
    run_fold::{run_source, AssistantShape, RunEnd, RunFold, SourceRule},
    tools::Workspace,
};
use futures_util::future::{AbortHandle, Abortable};
use rand::{rngs::StdRng, RngExt, SeedableRng};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

/// The shortest marker. A7 references a text only when that saves space, so a shorter text may
/// legitimately be stored twice.
const MIN_MARKER_BYTES: usize = 24;
/// How many sessions CI runs unless `BLACKWALL_PROPERTY_SESSIONS` says otherwise.
const DEFAULT_SESSIONS: u64 = 1_000;

/// Text of every shape a chat holds: plain, emoji, CJK, a paragraph break, padding and tabs.
const FLAVORS: [&str; 7] = [
    "the build finished without warnings",
    "🦀 emoji 🚀 and a flag 🇯🇵",
    "日本語のテキストです。確認してください",
    "first paragraph\n\nsecond paragraph",
    "tabs\tand trailing spaces   ",
    "quote \" backslash \\ and <tags> & ampersand",
    "mixed 🦀 日本語 line\nbreak",
];

/// A unique text for the chat to carry. It starts with the seed and a counter, then eight random
/// hex digits, so no marker contains another.
struct Markers {
    seed: u64,
    all: Vec<String>,
}

impl Markers {
    fn make(&mut self, rng: &mut StdRng) -> String {
        let flavor = FLAVORS[rng.random_range(0..FLAVORS.len())];
        let marker = format!(
            "mk{:x}_{}_{:08x}: {flavor}",
            self.seed,
            self.all.len(),
            rng.random::<u32>()
        );
        assert!(marker.len() >= MIN_MARKER_BYTES, "{marker}");
        self.all.push(marker.clone());
        marker
    }
}

fn nonce() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// What the client has saved so far. A crash keeps exactly this.
#[derive(Clone, Default)]
struct Saved {
    messages: Vec<Value>,
    context: Option<ContextState>,
    report: Option<Value>,
}

/// How one run is told to end. Stop and crash name the event after which they happen; a run
/// that finishes sooner simply completes.
#[derive(Clone, Copy, Debug)]
enum Ending {
    Complete,
    Stop(usize),
    Crash(usize),
    Fail,
}

/// What a driven run produced.
struct Driven {
    events: Vec<AgentEvent>,
    /// `None` when the future was aborted.
    result: Option<Result<String, AgentError>>,
}

struct Simulation {
    rng: StdRng,
    markers: Markers,
    shape: AssistantShape,
    directory: PathBuf,
    clock: u64,
    saved: Saved,
    ids: u64,
    files: u64,
}

impl Simulation {
    fn tick(&mut self) -> u64 {
        self.clock += 1000;
        self.clock
    }

    fn id(&mut self, kind: &str) -> String {
        self.ids += 1;
        match self.shape {
            AssistantShape::Desktop => format!(
                "message_{:08x}-0000-4000-8000-{:012x}",
                self.ids,
                self.rng.random::<u64>() & 0xffff_ffff_ffff
            ),
            AssistantShape::Cli => format!("{kind}_{}_{}", self.clock, self.ids),
        }
    }

    fn rule(&self) -> SourceRule {
        match self.shape {
            AssistantShape::Desktop => SourceRule::Desktop,
            AssistantShape::Cli => SourceRule::Cli,
        }
    }

    fn usage(&mut self) -> Option<TokenUsage> {
        self.rng.random_bool(0.5).then(|| {
            let (prompt, completion) = (
                self.rng.random_range(10..4000),
                self.rng.random_range(1..400),
            );
            TokenUsage {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: prompt + completion,
                ..Default::default()
            }
        })
    }

    /// A model turn's reasoning, now and then.
    fn reasoning(&mut self, turn: &mut ModelTurn) {
        if self.rng.random_bool(0.2) {
            turn.reasoning = self.markers.make(&mut self.rng);
            turn.reasoning_ms = self.rng.random_range(1..5000);
        }
    }

    /// The scripted turns of one run: tool turns that read marker files, then a final answer, cut
    /// short by the failing step when the run is to fail. Returns the files the turns read.
    fn script(&mut self, ending: Ending, directory: &Path) -> Vec<Step> {
        let tool_turns = self.rng.random_range(0..=3);
        let fail_at = matches!(ending, Ending::Fail).then(|| self.rng.random_range(0..=tool_turns));
        let mut steps = vec![];
        for turn in 0..=tool_turns {
            if fail_at == Some(turn) {
                let partial = match self.rng.random_range(0..3) {
                    0 => String::new(),
                    _ => self.markers.make(&mut self.rng),
                };
                let error = match self.rng.random_range(0..3) {
                    0 => ModelError::Network,
                    1 => ModelError::Http(500, None),
                    _ => ModelError::Invalid,
                };
                steps.push(Step::Fail { partial, error });
                break;
            }
            let last = turn == tool_turns;
            // An empty turn is legal between tool batches; the final answer always has text.
            let content = if !last && self.rng.random_range(0..4) == 0 {
                String::new()
            } else {
                self.markers.make(&mut self.rng)
            };
            let mut model = ModelTurn {
                content,
                usage: self.usage(),
                ..Default::default()
            };
            if !last {
                for call in 0..self.rng.random_range(1..=3) {
                    // A file with a marker, or now and then a path that is not there.
                    self.files += 1;
                    let name = format!("note_{}.txt", self.files);
                    if self.rng.random_range(0..6) > 0 {
                        let text = self.markers.make(&mut self.rng);
                        std::fs::write(directory.join(&name), text).unwrap();
                    }
                    model.calls.push(ToolCall {
                        id: format!("call_{turn}_{call}"),
                        function: ToolFunction {
                            name: "read_file".into(),
                            arguments: json!({"path": name}).to_string(),
                        },
                    });
                }
            }
            self.reasoning(&mut model);
            steps.push(Step::Say(model));
        }
        steps
    }

    /// Runs the real agent for `run`, collecting its events as the client sees them. After the
    /// event numbered `stop_at` the future is aborted and later events are not seen.
    fn drive(
        &self,
        runtime: &tokio::runtime::Runtime,
        backend: &ScriptedBackend,
        run: ContextRun,
        stop_at: Option<usize>,
    ) -> Driven {
        let events = Arc::new(Mutex::new(vec![]));
        let (handle, registration) = AbortHandle::new_pair();
        let (sink, abort) = (events.clone(), handle.clone());
        let agent = Agent {
            backend,
            workspace: Workspace::open(&self.directory).unwrap(),
            web_enabled: false,
            approvals: Approvals::default(),
            emit: Arc::new(move |event| {
                let mut seen = sink.lock().unwrap();
                if abort.is_aborted() {
                    return;
                }
                seen.push(event);
                if stop_at == Some(seen.len() - 1) {
                    abort.abort();
                }
            }),
        };
        let work = Abortable::new(
            agent.run_context_with_guidance("sim", run, None, None),
            registration,
        );
        // A future that finished in the same poll as the abort still counts as aborted: the
        // client stopped listening at that event.
        let result = runtime.block_on(work).ok().filter(|_| !handle.is_aborted());
        drop(agent);
        let events = std::mem::take(&mut *events.lock().unwrap());
        Driven { events, result }
    }

    /// The run's input, as the client builds it from what it has saved plus the new rows.
    fn context_run(&self, messages: &[Value], compact: bool) -> ContextRun {
        let source = run_source(messages, self.rule()).expect("the saved rows form a source");
        let state = ContextState::restore(&source, self.saved.context.as_ref())
            .expect("v5 can continue this chat");
        ContextRun {
            source,
            state,
            budget: Default::default(),
            instructions: vec![],
            compact,
            trace: Default::default(),
            subagents: Default::default(),
            instruction_settings: Default::default(),
        }
    }

    /// A run of the agent for a new prompt.
    fn prompt_run(&mut self, runtime: &tokio::runtime::Runtime) {
        let prompt = self.markers.make(&mut self.rng);
        let created = self.tick();
        let user = json!({
            "id": self.id("user"), "role": "user", "content": prompt, "attachments": [],
            "status": "complete", "createdAt": created,
        });
        let ending = match self.rng.random_range(0..20) {
            0..=9 => Ending::Complete,
            10..=13 => Ending::Stop(self.rng.random_range(0..14)),
            14..=16 => Ending::Fail,
            _ => Ending::Crash(self.rng.random_range(0..14)),
        };
        let desktop = self.shape == AssistantShape::Desktop;
        if desktop && self.rng.random_range(0..4) == 0 {
            // The desktop reopened the chat: every saved row is historical.
            for message in &mut self.saved.messages {
                message["historical"] = json!(true);
            }
        }
        let created = self.tick();
        let assistant = self.id("assistant");
        let mut fold = RunFold::new(&assistant, created as i64, self.shape);
        let mut messages = self.saved.messages.clone();
        messages.push(user);
        let source_messages = messages.clone();
        if desktop {
            messages.push(fold.message().clone());
        }
        // Saved at send, before anything runs.
        self.saved.messages = messages.clone();

        let directory = self.directory.clone();
        let steps = self.script(ending, &directory);
        let backend = ScriptedBackend::new(steps);
        let run = self.context_run(&source_messages, false);
        let stop_at = match ending {
            Ending::Stop(at) | Ending::Crash(at) => Some(at),
            _ => None,
        };
        let driven = self.drive(runtime, &backend, run, stop_at);

        let mut context = self.saved.context.clone();
        let mut report = self.saved.report.clone();
        let mut snapshot = None;
        for event in &driven.events {
            fold.apply(event);
            match event {
                AgentEvent::ContextReport { report: next, .. } => report = Some(json!(next)),
                AgentEvent::ContextUpdated { state, .. } => {
                    context = Some(state.clone());
                    snapshot = Some(state.clone());
                    if desktop {
                        *messages.last_mut().unwrap() = fold.message().clone();
                        self.saved = Saved {
                            messages: messages.clone(),
                            context: context.clone(),
                            report: report.clone(),
                        };
                    }
                }
                _ => {}
            }
        }
        let end = match (&driven.result, ending) {
            // The process died: the client keeps what it last saved.
            (None, Ending::Crash(_)) => return,
            (None, _) => RunEnd::Stopped,
            (Some(Ok(_)), _) => RunEnd::Complete,
            (Some(Err(error)), _) => {
                let code = ["model_unavailable", "model_busy", "model_timeout"]
                    [self.rng.random_range(0..3)];
                let detail = self
                    .rng
                    .random_bool(0.3)
                    .then(|| self.markers.make(&mut self.rng));
                if desktop && self.rng.random_bool(0.5) {
                    // The shell reported the failure, and the webview prefers that event.
                    fold.apply(&AgentEvent::Error {
                        request_id: "sim".into(),
                        code: code.into(),
                        message: error.to_string(),
                        retryable: true,
                        detail: None,
                    });
                }
                RunEnd::Failed {
                    code: code.into(),
                    message: error.to_string(),
                    detail,
                }
            }
        };
        fold.finish(&end);
        if desktop {
            *messages.last_mut().unwrap() = fold.message().clone();
        } else {
            messages.push(fold.message().clone());
            // The CLI keeps the last snapshot even when the run failed.
            context = snapshot.or(context);
        }
        self.saved = Saved {
            messages,
            context,
            report,
        };
    }

    /// A `/compact` with a scripted summary. A compaction that finds nothing to do, or no space
    /// to save, changes nothing, as it does not in the clients.
    fn compact_run(&mut self, runtime: &tokio::runtime::Runtime) {
        let summary = json!({
            "taskRequirements": ["keep going"], "corrections": [], "decisions": [],
            "changedFiles": [], "checks": [], "unfinishedWork": [], "relevantContext": [],
        });
        let backend = ScriptedBackend::new(vec![Step::Say(ModelTurn {
            content: summary.to_string(),
            ..Default::default()
        })]);
        let messages = self.saved.messages.clone();
        let run = self.context_run(&messages, true);
        let driven = self.drive(runtime, &backend, run, None);
        if !matches!(driven.result, Some(Ok(_))) {
            return;
        }
        for event in driven.events {
            match event {
                AgentEvent::ContextReport { report, .. } => self.saved.report = Some(json!(report)),
                AgentEvent::ContextUpdated { state, .. } => self.saved.context = Some(state),
                _ => {}
            }
        }
        if self.shape == AssistantShape::Cli {
            // The CLI does not save the report.
            self.saved.report = None;
        }
    }

    /// The session body as the client wrote it.
    fn body(&mut self, id: &str) -> Value {
        let first = self.saved.messages[0]["content"].as_str().unwrap();
        let title = match self.shape {
            AssistantShape::Desktop => {
                let text = first.trim();
                if text.chars().count() > 42 {
                    let head: String = text.chars().take(41).collect();
                    format!("{}…", head.trim_end())
                } else {
                    text.to_owned()
                }
            }
            AssistantShape::Cli => first.chars().take(80).collect(),
        };
        let updated = self.tick() + 1;
        let mut session = json!({
            "id": id, "title": title, "updatedAt": updated, "mode": "agent",
            "workspace": WORKSPACE, "messages": self.saved.messages,
        });
        if let Some(state) = &self.saved.context {
            session["contextState"] = json!(state);
        }
        if let (AssistantShape::Desktop, Some(report)) = (self.shape, &self.saved.report) {
            session["contextReport"] = report.clone();
        }
        session
    }
}

fn simulate(seed: u64) -> (Value, Vec<String>) {
    let mut rng = StdRng::seed_from_u64(seed);
    let shape = if rng.random_range(0..5) == 0 {
        AssistantShape::Cli
    } else {
        AssistantShape::Desktop
    };
    let directory = std::env::temp_dir().join(format!(
        "blackwall-sim-{}-{seed}-{}",
        std::process::id(),
        nonce()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    if rng.random_range(0..3) == 0 {
        std::fs::write(directory.join("AGENTS.md"), "Keep changes small.").unwrap();
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut sim = Simulation {
        rng,
        markers: Markers { seed, all: vec![] },
        shape,
        directory: directory.clone(),
        clock: 1_700_000_000_000 + seed % 1_000_000 * 3_600_000,
        saved: Saved::default(),
        ids: 0,
        files: 0,
    };
    let id = match shape {
        AssistantShape::Desktop => format!("session_{seed:016x}"),
        AssistantShape::Cli => format!("cli_{}_{seed}_0", sim.clock),
    };
    for run in 0..sim.rng.random_range(1..=6) {
        if run % 4 == 3 {
            sim.compact_run(&runtime);
        } else {
            sim.prompt_run(&runtime);
        }
    }
    let body = sim.body(&id);
    drop(runtime);
    std::fs::remove_dir_all(&directory).unwrap();
    // A marker whose text never reached the chat (the run stopped first) is not the chat's.
    let text = body.to_string();
    let markers = sim
        .markers
        .all
        .into_iter()
        .filter(|marker| text.contains(&escaped(marker)))
        .collect();
    (body, markers)
}

/// A v5 chat produced by the real agent for `seed`, and the unique texts (each at least 24 bytes)
/// the chat holds. It runs on its own thread, so it is safe to call from any test.
pub(crate) fn simulate_v5(seed: u64) -> (Value, Vec<String>) {
    std::thread::scope(|scope| scope.spawn(|| simulate(seed)).join().unwrap())
}

/// `text` as it appears inside a JSON string.
fn escaped(text: &str) -> String {
    let quoted = serde_json::to_string(text).unwrap();
    quoted[1..quoted.len() - 1].to_owned()
}

fn sessions() -> u64 {
    std::env::var("BLACKWALL_PROPERTY_SESSIONS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(DEFAULT_SESSIONS)
}

/// Upgrades `body` in `store`, as the real upgrade would, and checks everything the contract
/// promises. The error names the first thing that does not hold.
fn upgrade_and_check(store: &LocalStore, body: &Value, markers: &[String]) -> Result<(), String> {
    let (id, title, updated_at, raw) = validate_session(body).map_err(|e| format!("v5: {e}"))?;
    let tx = write_tx(store.connection()).map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO sessions(id,title,updated_at,body) VALUES (?1,?2,?3,?4)",
        rusqlite::params![id, title, updated_at, raw],
    )
    .map_err(|e| e.to_string())?;
    let outcome = convert_session_in_tx(&tx, id, title, updated_at, &raw, 1_800_000_000_000)
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    let report = match outcome {
        ConvertOutcome::Kept(report) => report,
        ConvertOutcome::SetAside { reason } => return Err(format!("set aside: {reason}")),
    };
    let loaded = store
        .load_json_v6(id)
        .map_err(|e| e.to_string())?
        .ok_or("not loaded")?;
    if &loaded != body {
        return Err(format!("loads differently:\n v5: {body}\n v6: {loaded}"));
    }
    if report.inline_bytes != 0 {
        return Err(format!("stored text twice: {report:?}"));
    }
    let bodies: Vec<String> = {
        let mut statement = store
            .connection()
            .prepare("SELECT body FROM chat_events WHERE session_id=?1")
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([id], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())?
    };
    for marker in markers {
        let needle = escaped(marker);
        let count: usize = bodies.iter().map(|b| b.matches(&needle).count()).sum();
        if count != 1 {
            return Err(format!("{marker:?} is stored {count} times"));
        }
    }
    Ok(())
}

#[test]
fn ten_thousand_generated_v5_sessions_upgrade_and_load_exactly() {
    use super::chats::counter_repairs;
    use super::convert::upgrade_inline_values;
    let store = v6_in_memory();
    let (mut checkpointed, mut tooled, mut cut_short, mut errored, mut cli) = (0, 0, 0, 0, 0);
    for seed in 0..sessions() {
        let (body, markers) = simulate_v5(seed);
        assert!(!markers.is_empty(), "seed {seed}");
        if let Err(problem) = upgrade_and_check(&store, &body, &markers) {
            panic!("seed {seed}: {problem}");
        }
        let rows = body["messages"].as_array().unwrap();
        let has = |status: &str| rows.iter().any(|row| row["status"] == status);
        checkpointed += (!body["contextState"]["checkpoints"]
            .as_array()
            .is_none_or(Vec::is_empty)) as u32;
        tooled += rows.iter().any(|row| row.get("tools").is_some()) as u32;
        cut_short += (has("stopped") || has("streaming")) as u32;
        errored += has("error") as u32;
        cli += body["id"].as_str().unwrap().starts_with("cli_") as u32;
    }
    println!(
        "{} sessions: {checkpointed} with checkpoints, {tooled} with tools, {cut_short} cut short, \
         {errored} with error rows, {cli} from the CLI",
        sessions()
    );
    assert_eq!(upgrade_inline_values(), 0);
    assert_eq!(counter_repairs(), 0);
    // The generator must reach every shape, or the property proves nothing.
    if sessions() >= 200 {
        for (name, seen) in [
            ("checkpoints", checkpointed),
            ("tools", tooled),
            ("stopped or streaming rows", cut_short),
            ("error rows", errored),
            ("cli sessions", cli),
        ] {
            assert!(seen > 0, "no generated session had {name}");
        }
    }
}

#[test]
fn the_simulator_matches_v5_restore() {
    for seed in 0..200 {
        let (body, markers) = simulate_v5(seed);
        assert!(markers.iter().all(|m| m.len() >= MIN_MARKER_BYTES));
        let cli = body["id"].as_str().unwrap().starts_with("cli_");
        let mut messages = body["messages"].as_array().unwrap().clone();
        messages.push(json!({
            "id": "next", "role": "user", "content": "Continue please", "attachments": [],
            "status": "complete", "createdAt": 1,
        }));
        let rule = if cli {
            SourceRule::Cli
        } else {
            SourceRule::Desktop
        };
        let saved: Option<ContextState> = body
            .get("contextState")
            .map(|state| serde_json::from_value(state.clone()).unwrap());
        let source = run_source(&messages, rule).unwrap();
        assert!(
            ContextState::restore(&source, saved.as_ref()).is_ok(),
            "seed {seed}"
        );
    }
}

#[test]
fn the_same_seed_makes_the_same_chat() {
    for seed in [0, 1, 7, 42] {
        assert_eq!(simulate_v5(seed), simulate_v5(seed));
    }
}

/// `BLACKWALL_PROPERTY_SEED=n cargo test replay_one_seed -- --ignored --nocapture` prints the v5
/// chat for a seed and the events it upgrades to.
#[test]
#[ignore = "a debugging aid"]
fn replay_one_seed() {
    let seed: u64 = std::env::var("BLACKWALL_PROPERTY_SEED")
        .unwrap()
        .parse()
        .unwrap();
    let (body, markers) = simulate_v5(seed);
    println!("{}", serde_json::to_string_pretty(&body).unwrap());
    println!("markers: {markers:#?}");
    let store = v6_in_memory();
    let result = upgrade_and_check(&store, &body, &markers);
    let mut statement = store
        .connection()
        .prepare("SELECT seq,kind,body FROM chat_events ORDER BY seq")
        .unwrap();
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap();
    for row in rows {
        let (seq, kind, body) = row.unwrap();
        println!("{seq} {kind} {body}");
    }
    println!("{result:?}");
}
