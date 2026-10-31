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
    run_fold::{model_error_text, run_source, AssistantShape, RunEnd, RunFold, SourceRule},
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
    /// A unique text that is not made of a flavor, such as a data URL.
    fn raw(&mut self, text: String) -> String {
        assert!(text.len() >= MIN_MARKER_BYTES, "{text}");
        self.all.push(text.clone());
        text
    }

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
    /// The run completed, but its trailing events (from the last `ContextUpdated` on) never
    /// reached the webview, which stops listening when the invoke resolves (`ipc.ts`). The row is
    /// `complete` while the saved context is an earlier snapshot. Desktop only.
    Lost,
}

/// Which shapes a session actually reached, for the coverage floors.
#[derive(Clone, Copy, Default)]
struct Tally {
    stop: bool,
    fail: bool,
    crash: bool,
    lost: bool,
    empty_final: bool,
    text_attachment: bool,
    image_attachment: bool,
}

/// `store.ts` `loadSession`: opening a saved chat rewrites every row. A `streaming` row is
/// `stopped` with its content as it was, running tools are `stopped`, running children are
/// `interrupted`, and every row is `historical`.
fn reopen(messages: &mut [Value]) {
    for message in messages {
        message["historical"] = json!(true);
        if message["status"] == "streaming" {
            message["status"] = json!("stopped");
        }
        if let Some(tools) = message.get_mut("tools").and_then(Value::as_array_mut) {
            for tool in tools.iter_mut().filter(|tool| tool["status"] == "running") {
                tool["status"] = json!("stopped");
            }
        }
        if let Some(children) = message.get_mut("children").and_then(Value::as_array_mut) {
            for child in children
                .iter_mut()
                .filter(|child| child["state"] == "running")
            {
                child["state"] = json!("interrupted");
                child["summary"] = json!("Interrupted before this conversation was saved.");
            }
        }
    }
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
    images: u32,
    /// The desktop crashed in the last run, so it reopens the chat before sending again.
    crashed: bool,
    /// v5 cannot continue this chat (an empty answer it already covered became an error row).
    stuck: bool,
    tally: Tally,
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

    /// The scripted turns of one run: tool turns that read marker files (and now and then delegate
    /// to a subagent, whose one scripted answer follows its parent turn), then a final answer, cut
    /// short by the failing step when the run is to fail. An `empty_final` run ends on a turn with
    /// no text.
    fn script(&mut self, ending: Ending, empty_final: bool, directory: &Path) -> Vec<Step> {
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
            // An empty turn is legal between tool batches; the final answer has text unless the
            // run is the empty-answer case.
            let content = if (!last && self.rng.random_range(0..4) == 0) || (last && empty_final) {
                String::new()
            } else {
                self.markers.make(&mut self.rng)
            };
            let mut model = ModelTurn {
                content,
                usage: self.usage(),
                ..Default::default()
            };
            let mut child = false;
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
                // One delegation at most per turn keeps the order of the children fixed.
                child = self.rng.random_range(0..5) == 0;
                if child {
                    model.calls.push(ToolCall {
                        id: format!("call_{turn}_child"),
                        function: ToolFunction {
                            name: "spawn_agent".into(),
                            arguments: json!({"goal": "Look through the project notes"})
                                .to_string(),
                        },
                    });
                }
            }
            self.reasoning(&mut model);
            steps.push(Step::Say(model));
            if child {
                // The child asks the same backend for its one answer. Its text becomes both the
                // tool output and the child's summary, so it is not a marker.
                steps.push(Step::Say(ModelTurn {
                    content: "The child found nothing unusual in the notes.".into(),
                    ..Default::default()
                }));
            }
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

    /// The ending of a run, by chance. The empty-answer flavor of a complete run is separate.
    fn ending(&mut self) -> (Ending, bool) {
        let desktop = self.shape == AssistantShape::Desktop;
        match self.rng.random_range(0..100) {
            0..=43 => (Ending::Complete, false),
            44..=48 => (Ending::Complete, true),
            49..=66 => (Ending::Stop(self.rng.random_range(0..16)), false),
            67..=80 => (Ending::Fail, false),
            81..=94 => (Ending::Crash(self.rng.random_range(0..16)), false),
            _ if desktop => (Ending::Lost, false),
            _ => (Ending::Complete, false),
        }
    }

    /// The attachments of a desktop prompt, now and then: a text file, a small image, or both.
    fn attachments(&mut self) -> Vec<Value> {
        let mut attachments = vec![];
        if self.shape != AssistantShape::Desktop || self.rng.random_range(0..5) > 0 {
            return attachments;
        }
        let kinds = self.rng.random_range(1..=3);
        if kinds & 1 != 0 {
            let text = self.markers.make(&mut self.rng);
            self.tally.text_attachment = true;
            attachments.push(json!({
                "id": format!("attachment_{}", attachments.len() + 1),
                "name": format!("notes_{}.txt", self.ids), "mimeType": "text/plain",
                "sizeBytes": text.len(), "kind": "text", "textContent": text,
            }));
        }
        if kinds & 2 != 0 && self.images < 3 {
            self.images += 1;
            let url = self.markers.raw(format!(
                "data:image/png;base64,iVBORw0KGgo{:016x}AAAA",
                self.rng.random::<u64>()
            ));
            self.tally.image_attachment = true;
            attachments.push(json!({
                "id": format!("attachment_{}", attachments.len() + 1),
                "name": format!("pixel_{}.png", self.ids), "mimeType": "image/png",
                "sizeBytes": url.len(), "kind": "image", "dataUrl": url,
            }));
        }
        attachments
    }

    /// The saved rows as the client holds them when it sends again: a desktop that crashed, and
    /// now and then one that was simply reopened, has rewritten every row.
    fn reopened(&self, messages: &mut [Value], draw: bool) {
        if self.shape == AssistantShape::Desktop && (self.crashed || draw) {
            reopen(messages);
        }
    }

    /// A run of the agent for a new prompt.
    fn prompt_run(&mut self, runtime: &tokio::runtime::Runtime) {
        let prompt = self.markers.make(&mut self.rng);
        let attachments = self.attachments();
        let created = self.tick();
        let user = json!({
            "id": self.id("user"), "role": "user", "content": prompt, "attachments": attachments,
            "status": "complete", "createdAt": created,
        });
        let (ending, empty_final) = self.ending();
        let desktop = self.shape == AssistantShape::Desktop;
        let draw = self.rng.random_range(0..4) == 0;
        let mut messages = self.saved.messages.clone();
        self.reopened(&mut messages, draw);
        self.crashed = false;
        let created = self.tick();
        let assistant = self.id("assistant");
        let mut fold = RunFold::new(&assistant, created as i64, self.shape);
        messages.push(user);
        let source_messages = messages.clone();
        if desktop {
            messages.push(fold.message().clone());
        }
        // Saved at send, before anything runs.
        self.saved.messages = messages.clone();

        let directory = self.directory.clone();
        let steps = self.script(ending, empty_final, &directory);
        let backend = ScriptedBackend::new(steps);
        let run = self.context_run(&source_messages, false);
        let stop_at = match ending {
            Ending::Stop(at) | Ending::Crash(at) => Some(at),
            _ => None,
        };
        let mut driven = self.drive(runtime, &backend, run, stop_at);
        if matches!(ending, Ending::Lost) {
            // Everything from the last `ContextUpdated` on never arrived.
            let last = driven
                .events
                .iter()
                .rposition(|event| matches!(event, AgentEvent::ContextUpdated { .. }));
            if let Some(last) = last {
                driven.events.truncate(last);
                self.tally.lost = true;
            }
        }

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
            // The process died: the client keeps what it last saved, and reopens it later.
            (None, Ending::Crash(_)) => {
                self.crashed = true;
                self.tally.crash = true;
                return;
            }
            (None, _) => {
                self.tally.stop = true;
                RunEnd::Stopped
            }
            (Some(Ok(_)), _) => {
                self.tally.empty_final |= empty_final;
                RunEnd::Complete
            }
            (Some(Err(error)), _) => {
                self.tally.fail = true;
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
        // A completed run the webview turned into an error (no answer) covered an assistant row
        // that is no longer part of the source: v5 cannot continue the chat.
        self.stuck |=
            desktop && matches!(end, RunEnd::Complete) && fold.message()["status"] == "error";
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
        let draw = self.rng.random_range(0..4) == 0;
        let mut messages = self.saved.messages.clone();
        self.reopened(&mut messages, draw);
        let run = self.context_run(&messages, true);
        let driven = self.drive(runtime, &backend, run, None);
        if !matches!(driven.result, Some(Ok(_))) {
            return;
        }
        // Compacting saves the chat, reopened rows and all.
        self.saved.messages = messages;
        self.crashed = false;
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

/// A subagent's id is random. Numbering the ids in the order they appear makes the chat the
/// same for the same seed.
fn normalize_children(body: &mut Value) {
    let mut seen: Vec<String> = vec![];
    let messages = body["messages"].as_array_mut().unwrap();
    for child in messages
        .iter_mut()
        .filter_map(|message| message.get_mut("children")?.as_array_mut())
        .flatten()
    {
        let old = child["id"].as_str().unwrap().to_owned();
        let position = seen.iter().position(|id| *id == old).unwrap_or_else(|| {
            seen.push(old);
            seen.len() - 1
        });
        child["id"] = json!(format!("child_{position:016x}"));
    }
}

fn simulate(seed: u64, tag: u64) -> (Value, Vec<String>, Tally) {
    let mut rng = StdRng::seed_from_u64(seed);
    let shape = if rng.random_range(0..5) == 0 {
        AssistantShape::Cli
    } else {
        AssistantShape::Desktop
    };
    // Fixed width: the path is in the model's instructions, so its length is in `contextReport`.
    let directory = std::env::temp_dir().join(format!(
        "blackwall-sim-{:010}-{seed:016x}-{tag:020}",
        std::process::id()
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
        images: 0,
        crashed: false,
        stuck: false,
        tally: Tally::default(),
    };
    let id = match shape {
        AssistantShape::Desktop => format!("session_{seed:016x}"),
        AssistantShape::Cli => format!("cli_{}_{seed}_0", sim.clock),
    };
    for run in 0..sim.rng.random_range(1..=10) {
        if sim.stuck {
            break;
        }
        if run % 4 == 3 {
            sim.compact_run(&runtime);
        } else {
            sim.prompt_run(&runtime);
        }
    }
    let mut body = sim.body(&id);
    normalize_children(&mut body);
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
    (body, markers, sim.tally)
}

/// `tag` makes the workspace directory unique. Its spelling must not change the chat.
fn simulate_tagged(seed: u64, tag: u64) -> (Value, Vec<String>, Tally) {
    std::thread::scope(|scope| scope.spawn(|| simulate(seed, tag)).join().unwrap())
}

fn simulate_tallied(seed: u64) -> (Value, Vec<String>, Tally) {
    simulate_tagged(seed, nonce())
}

/// A v5 chat produced by the real agent for `seed`, and the unique texts (each at least 24 bytes)
/// the chat holds. It runs on its own thread, so it is safe to call from any test.
pub(crate) fn simulate_v5(seed: u64) -> (Value, Vec<String>) {
    let (body, markers, _) = simulate_tallied(seed);
    (body, markers)
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
            .prepare(
                "SELECT body FROM chat_events WHERE sid=(SELECT key FROM sessions WHERE id=?1)",
            )
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

/// Whether v5 can continue this chat: a desktop run that covered an empty answer and then showed
/// it as an error leaves a context that covers a row the next source drops.
fn v5_can_continue(body: &Value) -> bool {
    let last = body["messages"].as_array().unwrap().last().unwrap();
    let empty = model_error_text("empty_model_response", None);
    !(last["status"] == "error" && last["content"] == "" && last["error"] == empty.as_str())
}

/// The shapes a session reached, as named flags.
fn shapes(body: &Value, tally: Tally) -> Vec<&'static str> {
    let rows = body["messages"].as_array().unwrap();
    let tools = || {
        rows.iter()
            .filter_map(|row| row["tools"].as_array())
            .flatten()
    };
    let checkpoints = body["contextState"]["checkpoints"]
        .as_array()
        .map_or(0, Vec::len);
    [
        ("stop", tally.stop),
        ("fail", tally.fail),
        ("crash", tally.crash),
        ("lost trailing events", tally.lost),
        ("empty final turn", tally.empty_final),
        ("cli", body["id"].as_str().unwrap().starts_with("cli_")),
        ("checkpoints", checkpoints > 0),
        ("several checkpoints", checkpoints > 1),
        ("read_file", tools().any(|tool| tool["name"] == "read_file")),
        (
            "tool cut mid-call",
            tools().any(|tool| tool["status"] == "stopped"),
        ),
        ("text attachment", tally.text_attachment),
        ("image attachment", tally.image_attachment),
        (
            "attachments",
            tally.text_attachment || tally.image_attachment,
        ),
        (
            "children",
            rows.iter()
                .any(|row| row["children"].as_array().is_some_and(|c| !c.is_empty())),
        ),
        (
            "trailing streaming row",
            rows.last().is_some_and(|row| row["status"] == "streaming"),
        ),
        (
            "error rows",
            rows.iter().any(|row| row["status"] == "error"),
        ),
    ]
    .into_iter()
    .filter_map(|(name, seen)| seen.then_some(name))
    .collect()
}

/// A saved chat has at most one `streaming` row, and only the last: reopening a chat after a
/// crash rewrites it (`store.ts` `loadSession`).
fn only_the_last_row_streams(body: &Value) -> bool {
    let rows = body["messages"].as_array().unwrap();
    rows[..rows.len() - 1]
        .iter()
        .all(|row| row["status"] != "streaming")
}

/// Shapes that must each be at least this percent of the sessions at 1,000 sessions or more.
const FLOOR_PERCENT: u32 = 3;
const FLOORED: [&str; 10] = [
    "stop",
    "fail",
    "crash",
    "cli",
    "checkpoints",
    "read_file",
    "tool cut mid-call",
    "lost trailing events",
    "attachments",
    "children",
];

#[test]
fn ten_thousand_generated_v5_sessions_upgrade_and_load_exactly() {
    use super::chats::counter_repairs;
    use super::convert::upgrade_inline_values;
    let store = v6_in_memory();
    let mut seen = std::collections::BTreeMap::<&str, u32>::new();
    for seed in 0..sessions() {
        let (body, markers, tally) = simulate_tallied(seed);
        assert!(!markers.is_empty(), "seed {seed}");
        assert!(only_the_last_row_streams(&body), "seed {seed}");
        if let Err(problem) = upgrade_and_check(&store, &body, &markers) {
            panic!("seed {seed}: {problem}");
        }
        for shape in shapes(&body, tally) {
            *seen.entry(shape).or_default() += 1;
        }
    }
    let total = sessions();
    println!("{total} sessions:");
    for (shape, count) in &seen {
        println!(
            "  {shape:<24} {count:>6}  {:>5.1}%",
            f64::from(*count) * 100.0 / total as f64
        );
    }
    assert_eq!(upgrade_inline_values(), 0);
    assert_eq!(counter_repairs(), 0);
    // The generator must reach every shape, or the property proves nothing.
    if total >= 1000 {
        for shape in FLOORED {
            let count = u64::from(seen.get(shape).copied().unwrap_or(0));
            assert!(
                count * 100 >= total * u64::from(FLOOR_PERCENT),
                "only {count} of {total} generated sessions had {shape}"
            );
        }
        for shape in [
            "empty final turn",
            "several checkpoints",
            "trailing streaming row",
        ] {
            assert!(seen.contains_key(shape), "no generated session had {shape}");
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
        // The one shape v5 cannot continue is the desktop's empty answer; every other chat can.
        assert_eq!(
            ContextState::restore(&source, saved.as_ref()).is_ok(),
            v5_can_continue(&body),
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

/// The workspace path is in the model's instructions, so its length reaches `contextReport`
/// (a token estimate of the prompt). Directory names that differ in length, as the process-wide
/// counter's do at 10, 100, 1,000..., made the same seed produce different chats.
#[test]
fn the_directory_name_does_not_change_the_chat() {
    for seed in 0..30 {
        for (before, after) in [(9, 10), (99, 100), (999, 1000), (999_999, 1_000_000)] {
            let (one, two) = (simulate_tagged(seed, before), simulate_tagged(seed, after));
            assert_eq!(
                (&one.0, &one.1),
                (&two.0, &two.1),
                "seed {seed}, directories {before} and {after}"
            );
        }
    }
}
