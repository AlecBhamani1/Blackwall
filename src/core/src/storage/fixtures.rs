//! v5 chat fixtures, the exact v5 schema and the v5 recall goldens.
//!
//! Every later upgrade-equality test starts from these, so each fixture is produced the way v5
//! produced it. A small simulator replays the real agent loop (`agent.rs` `run_context_with_guidance`)
//! and the real desktop store (`ui/src/lib/store.ts`) against the real `ContextState`:
//! `restore` before a run, `complete_source` only when a run completed, and the context the UI keeps
//! is the last `ContextUpdated` snapshot. Stopped, crashed and failed runs therefore leave exactly the
//! partial state v5 left. A few fixtures are deliberately edited afterwards (missing ids, a bad role,
//! a null context, a content mismatch); those edits are named where they happen.
//!
//! `system_in_transcript` is different: it is a synthetic legacy/imported shape. v5 never writes it
//! and cannot continue it (the agent and CLI paths partition `system` messages out of the source,
//! and the desktop computes its source before adding its own), so its saved hash covers an entry no
//! later source holds. It exists only to prove verbatim copy of non-user/assistant/tool roles.
//! `NON_CONTINUABLE` lists every fixture a v5 continuation cannot restore.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::{
    context::{fingerprint, Checkpoint, ContextBudget, ContextState, Summary},
    model::{self, ToolCall, ToolFunction},
    protocol::{ChatRequest, TokenUsage},
};
use std::path::PathBuf;

/// Fixtures that v5 cannot continue: restoring the saved context against the rows plus a new user
/// message fails. Continuation tests must skip them. `system_in_transcript` is a synthetic
/// legacy/imported shape (v5 never writes it); `text_tool_mismatch` was edited after its run, so its
/// rows no longer match the saved hash; `bad_role` is refused by v5 validation.
pub(crate) const NON_CONTINUABLE: &[&str] =
    &["system_in_transcript", "text_tool_mismatch", "bad_role"];

pub(crate) const RECALL_QUERIES: [&str; 7] = [
    "needle",
    "attachmentsecret",
    "toolsecret",
    "approvalsecret",
    "checkpoint",
    "unicode",
    "stopped",
];

/// The agent fixtures' workspace. v6 refuses a relative workspace, and `/work/project` is not
/// absolute on Windows, so each platform gets its own absolute spelling. The goldens always say
/// [`GOLDEN_WORKSPACE`]; compare through [`platform_neutral`].
#[cfg(not(windows))]
pub(crate) const WORKSPACE: &str = "/work/project";
#[cfg(windows)]
pub(crate) const WORKSPACE: &str = r"C:\work\project";
/// The workspace as the goldens spell it on every platform.
pub(crate) const GOLDEN_WORKSPACE: &str = "/work/project";

/// A test path written from the root, POSIX-style, as an absolute path on this platform:
/// `/p` stays `/p`, and on Windows becomes `C:\p`. A path already in this platform's form (such
/// as [`WORKSPACE`]) is returned as it is.
pub(crate) fn abs(path: &str) -> String {
    if cfg!(windows) {
        to_windows(path)
    } else {
        path.to_owned()
    }
}

/// The Windows spelling of a POSIX absolute path: `/a/b` is `C:\a\b`. Anything else, including
/// a path already spelled this way, is unchanged, so it is idempotent.
fn to_windows(path: &str) -> String {
    if path.starts_with('/') {
        format!("C:{}", path.replace('/', "\\"))
    } else {
        path.to_owned()
    }
}

/// The exact inverse of [`to_windows`] for what it produced: `C:\a\b` is `/a/b`.
fn from_windows(text: &str) -> Option<String> {
    text.strip_prefix("C:\\")
        .map(|rest| format!("/{}", rest.replace('\\', "/")))
}

/// `value` with every path [`abs`] spelled for this platform spelled as the goldens spell it, so
/// one golden serves every platform: the inverse of [`abs`]. Identity off Windows.
pub(crate) fn platform_neutral(value: Value) -> Value {
    match value {
        Value::String(text) if cfg!(windows) => Value::String(from_windows(&text).unwrap_or(text)),
        Value::Array(items) => Value::Array(items.into_iter().map(platform_neutral).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, platform_neutral(value)))
                .collect(),
        ),
        other => other,
    }
}

// ---------------------------------------------------------------------------------------------
// The simulator
// ---------------------------------------------------------------------------------------------

/// One tool call in a model turn. `output: None` means the tool was still running.
struct Call {
    id: &'static str,
    name: &'static str,
    arguments: &'static str,
    output: Option<&'static str>,
}
fn call(
    id: &'static str,
    name: &'static str,
    arguments: &'static str,
    output: &'static str,
) -> Call {
    Call {
        id,
        name,
        arguments,
        output: Some(output),
    }
}
fn running(id: &'static str, name: &'static str, arguments: &'static str) -> Call {
    Call {
        id,
        name,
        arguments,
        output: None,
    }
}

/// One model response: streamed text, then zero or more tool calls.
struct Turn {
    text: &'static str,
    calls: Vec<Call>,
    usage: Option<TokenUsage>,
    reasoning: Option<(&'static str, u64)>,
}
fn turn(text: &'static str, calls: Vec<Call>) -> Turn {
    Turn {
        text,
        calls,
        usage: None,
        reasoning: None,
    }
}
fn say(text: &'static str) -> Turn {
    turn(text, vec![])
}
impl Turn {
    fn usage(mut self, prompt: u64, completion: u64) -> Self {
        self.usage = Some(TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            ..Default::default()
        });
        self
    }
    fn reasoning(mut self, text: &'static str, ms: u64) -> Self {
        self.reasoning = Some((text, ms));
        self
    }
}

/// How the run ended.
enum End {
    /// The last turn has no tool calls: `complete_source` runs, status `complete`.
    Complete,
    /// The user pressed stop while a tool of the last turn was running.
    Stopped,
    /// The process died after the last `ContextUpdated`; the UI still says `streaming`.
    Crashed,
    /// The model failed in the next iteration after streaming `partial`.
    Failed {
        partial: &'static str,
        error: &'static str,
    },
}

struct Outcome {
    /// The last `ContextUpdated` state, if the run emitted one.
    emitted: Option<ContextState>,
    answer: String,
    tools: Vec<Value>,
    /// The answer and tools as the UI last saved them, after the last `ContextUpdated`.
    saved: Option<(String, Vec<Value>)>,
    reasoning: Option<(&'static str, u64)>,
}

/// The agent loop of `agent.rs`, one `ContextUpdated` per finished tool batch.
fn simulate(source: &[Value], saved: Option<&ContextState>, turns: &[Turn], end: &End) -> Outcome {
    let mut state = ContextState::restore(source, saved).unwrap();
    let mut out = Outcome {
        emitted: None,
        answer: String::new(),
        tools: vec![],
        saved: None,
        reasoning: None,
    };
    for (iteration, turn) in turns.iter().enumerate() {
        // agent.rs: a later turn is separated by a "\n\n" delta, even when it turns out empty.
        if iteration > 0 && !out.answer.is_empty() {
            out.answer.push_str("\n\n");
        }
        out.answer.push_str(turn.text);
        state.server_usage = turn.usage;
        if turn.calls.is_empty() {
            assert_eq!(
                iteration,
                turns.len() - 1,
                "only the last turn may be final"
            );
            state
                .transcript
                .push(json!({"role":"assistant","content":turn.text}));
            if matches!(end, End::Complete) {
                state.complete_source(source, &out.answer);
            }
            out.emitted = Some(state.clone());
            out.reasoning = turn.reasoning;
            return out;
        }
        let calls: Vec<Value> = turn
            .calls
            .iter()
            .map(|c| {
                ToolCall {
                    id: c.id.into(),
                    function: ToolFunction {
                        name: c.name.into(),
                        arguments: c.arguments.into(),
                    },
                }
                .as_json()
            })
            .collect();
        state
            .transcript
            .push(json!({"role":"assistant","content":turn.text,"tool_calls":calls}));
        // store.ts: the offset is the response length (UTF-16 units) when the call arrived.
        let offset = out.answer.encode_utf16().count();
        for (index, c) in turn.calls.iter().enumerate() {
            let mut tool = json!({
                "id": format!("{iteration}_{index}_{}", c.id),
                "name": c.name,
                "arguments": c.arguments,
                "contentOffset": offset,
                "status": "running",
            });
            if let Some(output) = c.output {
                tool["output"] = json!(output);
                tool["status"] = json!("complete");
            }
            out.tools.push(tool);
        }
        if turn.calls.iter().all(|c| c.output.is_some()) {
            state.transcript.extend(
                turn.calls.iter().map(
                    |c| json!({"role":"tool","tool_call_id":c.id,"content":c.output.unwrap()}),
                ),
            );
            out.emitted = Some(state.clone());
            out.saved = Some((out.answer.clone(), out.tools.clone()));
        } else {
            assert!(
                matches!(end, End::Stopped),
                "only a stopped run ends with a running tool"
            );
            assert_eq!(iteration, turns.len() - 1);
            return out;
        }
    }
    if let End::Failed { partial, .. } = end {
        if !out.answer.is_empty() {
            out.answer.push_str("\n\n");
        }
        out.answer.push_str(partial);
    }
    out
}

struct Chat {
    messages: Vec<Value>,
    state: Option<ContextState>,
    clock: u64,
    counter: u64,
    /// A system entry the source starts with (only `system_in_transcript`).
    system: Option<Value>,
}

impl Chat {
    fn new(index: u64) -> Self {
        Self {
            messages: vec![],
            state: None,
            clock: 1_700_000_000_000 + index * 3_600_000,
            counter: index * 1000,
            system: None,
        }
    }
    fn tick(&mut self) -> u64 {
        self.clock += 1000;
        self.clock
    }
    /// `createId('message')`: a prefix and a UUID.
    fn id(&mut self) -> String {
        self.counter += 1;
        format!(
            "message_{:08x}-0000-4000-8000-{:012x}",
            self.counter, self.counter
        )
    }
    /// What the desktop sends as `messages` (error and sending rows dropped), or what the CLI
    /// sends (every saved row), turned into model messages by `model::messages`.
    fn source(&self, cli: bool) -> Vec<Value> {
        let history: Vec<Value> = self
            .messages
            .iter()
            .filter(|m| cli || !matches!(m["status"].as_str(), Some("error" | "sending")))
            .map(
                |m| json!({"role":m["role"],"content":m["content"],"attachments":m["attachments"]}),
            )
            .collect();
        let request: ChatRequest = serde_json::from_value(json!({"messages": history})).unwrap();
        let mut source = model::messages(&request);
        if let Some(system) = &self.system {
            source.insert(0, system.clone());
        }
        source
    }

    // ----- desktop -----
    fn user(&mut self, content: &str, attachments: Vec<Value>) {
        let id = self.id();
        let created = self.tick();
        self.messages.push(json!({
            "id": id, "role": "user", "content": content, "attachments": attachments,
            "status": "complete", "createdAt": created,
        }));
    }
    /// Runs the agent for the newest user message and saves the assistant row; returns its index.
    fn run(&mut self, turns: Vec<Turn>, end: End) -> usize {
        let source = self.source(false);
        let out = simulate(&source, self.state.as_ref(), &turns, &end);
        let mut message = json!({
            "id": self.id(), "role": "assistant", "attachments": [], "createdAt": self.tick(),
        });
        let mut tools = out.tools.clone();
        let (content, status) = match &end {
            End::Complete => {
                if let Some((text, ms)) = out.reasoning {
                    message["reasoning"] = json!(text);
                    message["reasoningMs"] = json!(ms);
                }
                (out.answer.clone(), "complete")
            }
            End::Stopped => {
                for tool in &mut tools {
                    if tool["status"] == "running" {
                        tool["status"] = json!("stopped");
                    }
                }
                let text = if out.answer.is_empty() {
                    "Response stopped.".to_owned()
                } else {
                    out.answer.clone()
                };
                (text, "stopped")
            }
            End::Crashed => {
                let (text, saved) = out.saved.clone().expect("a crash needs a saved update");
                tools = saved;
                (text, "streaming")
            }
            End::Failed { error, .. } => {
                message["error"] = json!(error);
                (out.answer.clone(), "error")
            }
        };
        message["content"] = json!(content);
        message["status"] = json!(status);
        if !tools.is_empty() {
            message["tools"] = json!(tools);
        }
        if let Some(emitted) = out.emitted {
            self.state = Some(emitted);
        }
        self.messages.push(message);
        self.messages.len() - 1
    }
    /// A user message and a completed run in one step.
    fn exchange(&mut self, content: &str, turns: Vec<Turn>) -> usize {
        self.user(content, vec![]);
        self.run(turns, End::Complete)
    }
    /// The desktop loading a saved chat again: every row becomes `historical`.
    fn reopen(&mut self) {
        for message in &mut self.messages {
            message["historical"] = json!(true);
        }
    }
    /// A `/compact` that the model answered with `summary`: `ContextState::compact`'s bookkeeping.
    fn compact(&mut self, summary: Summary) {
        let source = self.source(false);
        let mut state = ContextState::restore(&source, self.state.as_ref()).unwrap();
        let users: Vec<_> = state
            .transcript
            .iter()
            .enumerate()
            .filter(|(_, m)| m["role"] == "user")
            .map(|(i, _)| i)
            .collect();
        let through = users[users.len() - 2];
        state.checkpoints.push(Checkpoint {
            through,
            source_hash: fingerprint(&state.transcript[..through]),
            summary,
        });
        self.state = Some(state);
    }

    // ----- CLI (src/bw/src/session.rs) -----
    /// A completed CLI run: the UI rows carry no tools, the context carries the tool pairs.
    fn cli_run(&mut self, prompt: &str, turns: Vec<Turn>) {
        let timestamp = self.tick();
        let number = self.messages.len();
        self.messages.push(json!({
            "id": format!("user_{timestamp}_{number}"), "role": "user", "content": prompt,
            "attachments": [], "status": "complete", "createdAt": timestamp,
        }));
        let source = self.source(true);
        let out = simulate(&source, self.state.as_ref(), &turns, &End::Complete);
        self.state = out.emitted;
        self.messages.push(json!({
            "id": format!("assistant_{timestamp}_{}", number + 1), "role": "assistant",
            "content": out.answer, "attachments": [], "status": "complete", "createdAt": timestamp,
        }));
        self.clock += 500;
    }
    /// A failed CLI run with no events: the context is untouched and the row's content is the
    /// failure message.
    fn cli_failure(&mut self, prompt: &str, message: &str) {
        let timestamp = self.tick();
        let number = self.messages.len();
        self.messages.push(json!({
            "id": format!("user_{timestamp}_{number}"), "role": "user", "content": prompt,
            "attachments": [], "status": "complete", "createdAt": timestamp,
        }));
        // The run restores before it fails, so the history must have been valid.
        ContextState::restore(&self.source(true), self.state.as_ref()).unwrap();
        self.messages.push(json!({
            "id": format!("assistant_{timestamp}_{}", number + 1), "role": "assistant",
            "content": message, "attachments": [], "status": "error", "createdAt": timestamp,
        }));
    }

    // ----- the saved session -----
    fn title(&self) -> String {
        let first = self.messages.iter().find(|m| m["role"] == "user").unwrap();
        let text = first["content"].as_str().unwrap().trim();
        let source = if text.is_empty() {
            first["attachments"][0]["name"]
                .as_str()
                .unwrap_or("New chat")
        } else {
            text
        };
        if source.chars().count() > 42 {
            let head: String = source.chars().take(41).collect();
            format!("{}…", head.trim_end())
        } else {
            source.to_owned()
        }
    }
    /// `saveCurrentSession`: mode, workspace for agent chats, rows, `contextState`, `contextReport`.
    fn desktop(&self, id: &str, agent: bool) -> Value {
        let mut session = json!({
            "id": id, "title": self.title(), "updatedAt": self.clock + 1,
            "mode": if agent { "agent" } else { "chat" }, "messages": self.messages,
        });
        if agent {
            session["workspace"] = json!(WORKSPACE);
        }
        if let Some(state) = &self.state {
            session["contextState"] = json!(state);
            session["contextReport"] =
                json!(ContextBudget::default().report(&state.effective(), &[], state.server_usage));
        }
        session
    }
    /// `Conversation::new` plus what `run` saves: no `contextReport`.
    fn cli(&self, id: &str) -> Value {
        let first = self.messages[0]["content"].as_str().unwrap();
        let mut session = json!({
            "id": id, "title": first.chars().take(80).collect::<String>(), "mode": "agent",
            "workspace": WORKSPACE, "updatedAt": self.clock + 1, "messages": self.messages,
        });
        if let Some(state) = &self.state {
            session["contextState"] = json!(state);
        }
        session
    }
}

fn summary(text: &str) -> Summary {
    Summary {
        task_requirements: vec![text.into()],
        corrections: vec![],
        decisions: vec![format!("Decided: {text}")],
        changed_files: vec![],
        checks: vec![],
        unfinished_work: vec!["Continue the conversation".into()],
        relevant_context: vec![],
    }
}

/// store.ts `instructions_loaded`: the synthetic `project_instructions` tool, first in the row.
fn add_instructions(message: &mut Value, sources: &[&str], warnings: &[&str]) {
    let mut output = vec![format!("Loaded instructions: {}", sources.join(", "))];
    output.extend(warnings.iter().map(|w| format!("Warning: {w}")));
    let tool = json!({
        "id": "instructions", "name": "project_instructions", "contentOffset": 0,
        "arguments": json!({"path": sources.join(", ")}).to_string(),
        "output": output.join("\n"),
        "status": if warnings.is_empty() { "complete" } else { "error" },
    });
    message["tools"].as_array_mut().unwrap().insert(0, tool);
}
/// store.ts `subagent_status`: the child leaves and re-enters at the end of the list.
fn set_child(message: &mut Value, id: &str, state: &str, summary: &str) {
    let mut children: Vec<Value> = message["children"].as_array().cloned().unwrap_or_default();
    children.retain(|child| child["id"] != id);
    children.push(json!({"id": id, "state": state, "summary": summary}));
    message["children"] = json!(children);
}

// ---------------------------------------------------------------------------------------------
// The fixtures
// ---------------------------------------------------------------------------------------------

fn desktop_plain() -> Value {
    let mut chat = Chat::new(0);
    chat.exchange(
        "Can you explain how a needle in a haystack test works?",
        vec![say("It hides one short fact, the needle, inside a long stretch of filler text and asks the model to find it.").usage(96, 31)],
    );
    chat.exchange(
        "Thanks. Résumé of naïve unicode handling: 日本語, emoji 🦀, and the word unicode itself.",
        vec![
            say("Unicode text is stored as UTF-8, so a single character can take several bytes.")
                .usage(168, 22),
        ],
    );
    chat.exchange(
        "What was the first thing I asked?",
        vec![say("You asked how a needle in a haystack test works.").usage(231, 12)],
    );
    chat.desktop("desktop_plain", false)
}

fn cli_session() -> Value {
    let mut chat = Chat::new(0);
    chat.clock = 1_700_000_000_000;
    chat.cli_run(
        "Find the needle in src/main.rs",
        vec![
            turn(
                "",
                vec![call(
                    "call_1",
                    "read_file",
                    r#"{"path":"src/main.rs"}"#,
                    "fn main() {\n    println!(\"needle\");\n}\n",
                )],
            ),
            say("The needle is the println on line 2 of src/main.rs."),
        ],
    );
    chat.cli_run(
        "Thanks, and is there anything else in that file?",
        vec![say("No, src/main.rs only contains main.")],
    );
    chat.cli("cli_1700000000000_1_0")
}

fn desktop_attachments() -> Value {
    let mut chat = Chat::new(2);
    chat.user(
        "Please summarize these files",
        vec![
            json!({"id":"attachment_1","name":"notes.txt","mimeType":"text/plain","sizeBytes":49,"kind":"text","textContent":"attachmentsecret: the launch plan lives in the notes"}),
            json!({"id":"attachment_2","name":"pixel.png","mimeType":"image/png","sizeBytes":68,"kind":"image","dataUrl":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="}),
            json!({"id":"attachment_3","name":"report.bin","mimeType":"application/octet-stream","sizeBytes":2048,"kind":"file"}),
        ],
    );
    chat.run(
        vec![say("The text file describes a launch plan, the image is a single pixel and the binary file could not be read.").usage(412, 27)],
        End::Complete,
    );
    chat.exchange(
        "Which file could you not read?",
        vec![say("report.bin, because its contents were not available.").usage(470, 11)],
    );
    chat.desktop("desktop_attachments", false)
}

fn agent_tools() -> Value {
    let mut chat = Chat::new(3);
    chat.exchange(
        "Fix the typo in the README and check the project layout",
        vec![
            turn(
                "Looking at the 🦀 crate first.",
                vec![
                    call("c1", "list_files", r#"{"path":"."}"#, "Cargo.toml\nREADME.md\nsrc"),
                    call("c2", "read_file", r#"{"path":"README.md"}"#, "# Demo\nThis toolsecret file has a typpo.\n"),
                ],
            )
            .usage(300, 40),
            turn(
                "Applying the fix.",
                vec![call(
                    "c3",
                    "write_file",
                    r##"{"path":"README.md","content":"# Demo\nThis approvalsecret file has a typo.\n"}"##,
                    "Wrote README.md",
                )],
            )
            .usage(420, 55),
            say("Fixed the typo in README.md and confirmed the layout.")
                .usage(510, 20)
                .reasoning("The README had a misspelling and nothing else needed changing.", 842),
        ],
    );
    chat.exchange(
        "Thanks. Anything else?",
        vec![say("No, that was the only change.").usage(560, 9)],
    );
    chat.desktop("agent_tools", true)
}

fn compacted() -> Value {
    let mut chat = Chat::new(4);
    chat.exchange(
        "Let's plan a small tool for renaming photos.",
        vec![say(
            "A rename tool needs a pattern, a dry run and an undo list.",
        )],
    );
    chat.exchange(
        "Use the date taken as the name.",
        vec![say(
            "Reading the date from EXIF and falling back to the file date works.",
        )],
    );
    chat.reopen();
    chat.exchange(
        "Make a checkpoint of what we decided so far.",
        vec![say(
            "Decided: date-taken names, a dry run and an undo list.",
        )],
    );
    chat.compact(summary("Plan a photo renaming tool"));
    chat.exchange(
        "Add support for burst photos.",
        vec![say(
            "Suffix burst photos with a counter so names stay unique.",
        )],
    );
    chat.compact(summary("Photo renaming tool with burst support"));
    chat.exchange(
        "Finally, how should errors be reported?",
        vec![say("List every skipped file with the reason after the run.").usage(2048, 14)],
    );
    chat.compact(summary(
        "Photo renaming tool, error reporting after the run",
    ));
    chat.desktop("compacted", false)
}

fn plain_chat(index: u64) -> Chat {
    let mut chat = Chat::new(index);
    chat.exchange(
        "First question about the schedule",
        vec![say("The schedule starts on Monday.")],
    );
    chat.exchange("And the second question?", vec![say("It ends on Friday.")]);
    chat.exchange(
        "Is Wednesday included?",
        vec![say("Yes, Wednesday is in the middle.")],
    );
    chat
}

/// A desktop chat of `2 * exchanges` messages, made by the same simulator as the fixtures: every
/// third exchange runs a tool (a call, its output and a final answer) and the rest answer in
/// plain text. `seed` varies the texts, so no two seeds share a message. It leaks its few
/// thousand short strings, which is fine in a test.
pub(crate) fn long_chat(id: &str, seed: u64, exchanges: usize) -> Value {
    fn leak(text: String) -> &'static str {
        Box::leak(text.into_boxed_str())
    }
    let mut chat = Chat::new(seed);
    for n in 0..exchanges {
        let ask = format!(
            "Question {n} of chat {seed}: how does step {n} of the pipeline handle a retry after a timeout?"
        );
        let answer = leak(format!(
            "Step {n} retries the request once after a timeout, then reports the failure to the caller. \
             It keeps the partial result so the next attempt resumes where this one stopped (chat {seed})."
        ));
        let turns = if n % 3 == 2 {
            vec![
                turn(
                    "Checking the file first.",
                    vec![call(
                        leak(format!("c{seed}_{n}")),
                        "read_file",
                        leak(format!(r#"{{"path":"src/step_{n}.rs"}}"#)),
                        leak(format!(
                            "fn step_{n}() {{\n    retry_once();\n    keep_partial_result();\n}}\n"
                        )),
                    )],
                )
                .usage(300, 40),
                say(answer).usage(420, 55),
            ]
        } else {
            vec![say(answer).usage(200, 30)]
        };
        chat.exchange(&ask, turns);
    }
    chat.desktop(id, false)
}

fn ids_missing_and_duplicate() -> Value {
    let mut session = plain_chat(5).desktop("ids_missing_and_duplicate", false);
    // v5 accepted data from before rows had ids, and a duplicated id.
    let messages = session["messages"].as_array_mut().unwrap();
    messages[0].as_object_mut().unwrap().remove("id");
    messages[5].as_object_mut().unwrap().remove("id");
    messages[2]["id"] = json!("a");
    messages[3]["id"] = json!("a");
    session
}

fn bad_role() -> Value {
    let mut session = plain_chat(6).desktop("bad_role", false);
    // The one thing v5 validation rejects; reachable only through a raw insert.
    session["messages"][1]["role"] = json!("tool");
    session
}

fn null_context_extra_key() -> Value {
    let mut session = plain_chat(7).desktop("null_context_extra_key", false);
    session["contextState"] = Value::Null;
    session["contextReport"] = Value::Null;
    session["pinned"] = json!(true);
    session
}

fn empty_middle_turn() -> Value {
    let mut chat = Chat::new(8);
    chat.exchange(
        "Run the checks",
        vec![
            turn(
                "A",
                vec![call("c1", "list_files", r#"{"path":"."}"#, "src")],
            ),
            turn(
                "",
                vec![call(
                    "c2",
                    "read_file",
                    r#"{"path":"src/lib.rs"}"#,
                    "pub fn run() {}\n",
                )],
            ),
            say("B"),
        ],
    );
    chat.desktop("empty_middle_turn", true)
}

fn stopped_then_continued() -> Value {
    let mut chat = Chat::new(9);
    chat.user("List the project files and read the build script", vec![]);
    chat.run(
        vec![
            turn(
                "Let me look.",
                vec![call("c1", "list_files", r#"{"path":"."}"#, "build.rs\nsrc")],
            ),
            turn(
                "Reading the build script",
                vec![running("c2", "read_file", r#"{"path":"build.rs"}"#)],
            ),
        ],
        End::Stopped,
    );
    chat.user("You stopped partway through, please continue", vec![]);
    chat.run(
        vec![
            turn(
                "",
                vec![call(
                    "c3",
                    "read_file",
                    r#"{"path":"build.rs"}"#,
                    "fn main() {}\n",
                )],
            ),
            say("The build script is empty apart from main."),
        ],
        End::Complete,
    );
    chat.desktop("stopped_then_continued", true)
}

fn crashed_streaming() -> Value {
    let mut chat = Chat::new(10);
    chat.exchange("Hello there", vec![say("Hello, how can I help?")]);
    chat.user("Search the project for TODO markers", vec![]);
    chat.run(
        vec![turn(
            "Searching the project",
            vec![call(
                "c1",
                "search_files",
                r#"{"path":".","query":"TODO"}"#,
                "src/lib.rs:12: TODO tidy",
            )],
        )],
        End::Crashed,
    );
    chat.desktop("crashed_streaming", true)
}

fn instructions_and_children() -> Value {
    let mut chat = Chat::new(11);
    chat.user("Review the project with two helpers", vec![]);
    let at = chat.run(
        vec![
            turn(
                "Delegating the reading.",
                vec![
                    call(
                        "c1",
                        "spawn_agent",
                        r#"{"goal":"Read the docs"}"#,
                        "The docs describe a CLI.",
                    ),
                    call(
                        "c2",
                        "spawn_agent",
                        r#"{"goal":"Read the tests"}"#,
                        "The tests cover parsing.",
                    ),
                ],
            ),
            say("The project is a CLI whose tests cover parsing."),
        ],
        End::Complete,
    );
    let message = &mut chat.messages[at];
    add_instructions(
        message,
        &["AGENTS.md"],
        &["Skipped oversized guidance in docs/AGENTS.md."],
    );
    let (one, two) = ("child_00000000000000a1", "child_00000000000000a2");
    set_child(message, one, "running", "Read the docs");
    set_child(message, two, "running", "Read the tests");
    set_child(message, one, "completed", "The docs describe a CLI.");
    set_child(message, two, "completed", "The tests cover parsing.");
    chat.desktop("instructions_and_children", true)
}

/// SYNTHETIC legacy/imported shape: v5 never writes it and cannot continue it; used only to prove
/// verbatim copy of non-user/assistant/tool roles. See `NON_CONTINUABLE`.
fn system_in_transcript() -> Value {
    let mut chat = Chat::new(12);
    chat.system =
        Some(json!({"role":"system","content":"You are a careful assistant. Answer briefly."}));
    chat.exchange("What is two plus two?", vec![say("Four.")]);
    chat.exchange("And times three?", vec![say("Twelve.")]);
    chat.desktop("system_in_transcript", false)
}

fn text_tool_mismatch() -> Value {
    let mut chat = Chat::new(13);
    chat.user("Check the folder", vec![]);
    let at = chat.run(
        vec![
            turn(
                "Checking",
                vec![call("c1", "list_files", r#"{"path":"."}"#, "src")],
            ),
            say("All good"),
        ],
        End::Complete,
    );
    // The row no longer equals the model turns joined with a blank line.
    chat.messages[at]["content"] = json!("Checking\n\nAll good, edited after the run");
    chat.desktop("text_tool_mismatch", true)
}

fn no_context_with_history() -> Value {
    let mut chat = Chat::new(14);
    chat.exchange("Hello", vec![say("Hi there")]);
    chat.exchange(
        "Tell me a joke",
        vec![say("Why did the byte cross the road?")],
    );
    // A chat saved before `contextState` existed.
    chat.state = None;
    chat.desktop("no_context_with_history", false)
}

fn error_rows_both_shapes() -> Value {
    let mut chat = Chat::new(15);
    chat.user("Check the build status", vec![]);
    chat.run(
        vec![turn(
            "Checking the build",
            vec![call("c1", "list_files", r#"{"path":"."}"#, "Cargo.toml")],
        )],
        End::Failed {
            partial: "The build is",
            error: "The model stream ended unexpectedly.",
        },
    );
    chat.cli_failure(
        "Try again from the CLI",
        &crate::context::ContextError::Limit.to_string(),
    );
    chat.desktop("error_rows_both_shapes", true)
}

/// Every v5 fixture, in a fixed order. Deterministic: ids, dates and text are constants.
pub(crate) fn v5_sessions() -> Vec<Value> {
    vec![
        desktop_plain(),
        cli_session(),
        desktop_attachments(),
        agent_tools(),
        compacted(),
        ids_missing_and_duplicate(),
        bad_role(),
        null_context_extra_key(),
        empty_middle_turn(),
        stopped_then_continued(),
        crashed_streaming(),
        instructions_and_children(),
        system_in_transcript(),
        text_tool_mismatch(),
        no_context_with_history(),
        error_rows_both_shapes(),
    ]
}

// ---------------------------------------------------------------------------------------------
// The exact v5 database
// ---------------------------------------------------------------------------------------------

/// Builds a database file shaped like v5 by running `v5_schema.sql` and inserting every body with
/// raw SQL (no validation, no 500 cap, no search index). `version` is what `user_version` says:
/// 5 is the current shape, 3 and 4 drop `sessions_updated_at` like the v4 upgrade test, and
/// anything below 3 has the `storage.rs` pairing-upgrade shape (old `memory`, no pairing tables).
pub(crate) fn build_v5(directory: &Path, bodies: &[Value], version: i64) -> PathBuf {
    assert!(
        (0..=5).contains(&version),
        "no v5 shape for version {version}"
    );
    fs::create_dir_all(directory).unwrap();
    // A data folder is private whatever the umask; the backups folder check relies on it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = directory.join(DATABASE_FILE);
    let connection = Connection::open(&path).unwrap();
    // `sqlite_master` records the file's whitespace, so a CRLF checkout must not change it.
    connection
        .execute_batch(&include_str!("fixtures/v5_schema.sql").replace("\r\n", "\n"))
        .unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    for body in bodies {
        transaction
            .execute(
                "INSERT INTO sessions(id,title,updated_at,body) VALUES (?1,?2,?3,?4)",
                params![
                    body["id"].as_str().unwrap(),
                    body["title"].as_str().unwrap(),
                    body["updatedAt"].as_i64().unwrap(),
                    serde_json::to_string(body).unwrap(),
                ],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    if version < 3 {
        connection
            .execute_batch("DROP TABLE paired_devices; DROP TABLE memory; DROP TABLE memory_proposals; CREATE TABLE memory (id TEXT PRIMARY KEY, content TEXT NOT NULL, updated_at INTEGER NOT NULL);")
            .unwrap();
    } else if version < 5 {
        connection
            .execute_batch("DROP INDEX sessions_updated_at;")
            .unwrap();
    }
    connection
        .pragma_update(None, "user_version", version)
        .unwrap();
    path
}

/// Every schema object and the user version of a database file.
pub(crate) fn master(database: &Path) -> Value {
    let connection = Connection::open(database).unwrap();
    let mut statement = connection
        .prepare("SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY type,name")
        .unwrap();
    let objects: Vec<Value> = statement
        .query_map([], |row| {
            Ok(json!({
                "type": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "tblName": row.get::<_, String>(2)?,
                "sql": row.get::<_, Option<String>>(3)?,
            }))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    json!({"userVersion": version, "objects": objects})
}

pub(crate) fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/storage/fixtures")
        .join(name)
}
pub(crate) fn golden(name: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(golden_path(name)).unwrap()).unwrap()
}

/// One scripted model response.
pub(crate) enum Step {
    /// A reply: its text streams as deltas, then the turn is returned.
    Say(model::ModelTurn),
    /// The model streams `partial` and then fails.
    Fail {
        partial: String,
        error: model::ModelError,
    },
}

/// A model backend that answers from a fixed script, one step per `generate` call. The real
/// agent loop runs against it, so everything it records is what v5 records.
pub(crate) struct ScriptedBackend {
    steps: std::sync::Mutex<std::collections::VecDeque<Step>>,
}

impl ScriptedBackend {
    pub(crate) fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: std::sync::Mutex::new(steps.into()),
        }
    }
}

impl model::ModelBackend for ScriptedBackend {
    fn generate<'a>(
        &'a self,
        _: &'a [Value],
        _: &'a [Value],
        delta: &'a mut (dyn FnMut(String) + Send),
    ) -> futures_util::future::BoxFuture<'a, Result<model::ModelTurn, model::ModelError>> {
        Box::pin(async move {
            let step = self.steps.lock().unwrap().pop_front();
            // A text arrives in two deltas when it can be split, as a stream would deliver it.
            let mut stream = |text: &str| {
                let middle = text.char_indices().nth(text.chars().count() / 2);
                match middle {
                    Some((at, _)) if at > 0 => {
                        delta(text[..at].to_owned());
                        delta(text[at..].to_owned());
                    }
                    _ if !text.is_empty() => delta(text.to_owned()),
                    _ => {}
                }
            };
            match step {
                Some(Step::Say(turn)) => {
                    stream(&turn.content);
                    Ok(turn)
                }
                Some(Step::Fail { partial, error }) => {
                    stream(&partial);
                    Err(error)
                }
                None => Err(model::ModelError::Invalid),
            }
        })
    }
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "blackwall-fixtures-{name}-{:016x}",
        rand::random::<u64>()
    ))
}

fn fixture(id: &str) -> Value {
    v5_sessions()
        .into_iter()
        .find(|s| s["id"] == id)
        .unwrap_or_else(|| panic!("no fixture {id}"))
}

#[test]
fn v5_schema_file_builds_the_exact_v5_master() {
    let directory = scratch("master");
    let built = build_v5(&directory, &v5_sessions(), 5);
    assert_eq!(master(&built), golden("v5_master.json"));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn every_fixture_but_bad_role_passes_v5_validation() {
    let sessions = v5_sessions();
    assert_eq!(sessions.len(), 16);
    for session in &sessions {
        let valid = validate_session(session);
        if session["id"] == "bad_role" {
            assert!(valid.is_err(), "bad_role must be refused");
        } else {
            assert!(valid.is_ok(), "{} must validate", session["id"]);
        }
    }
    let mut ids: Vec<_> = sessions.iter().map(|s| s["id"].as_str().unwrap()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), sessions.len());
}

#[test]
fn fixtures_are_deterministic() {
    assert_eq!(v5_sessions(), v5_sessions());
}

#[test]
fn build_v5_opens_in_v5_at_every_version_with_the_bodies_intact() {
    let sessions = v5_sessions();
    for version in 0..=5 {
        let directory = scratch(&format!("open{version}"));
        build_v5(&directory, &sessions, version);
        let store = LocalStore::open(&directory).unwrap();
        for session in &sessions {
            let id = session["id"].as_str().unwrap();
            assert_eq!(store.load_session(id).unwrap().as_ref(), Some(session));
        }
        let current: i64 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(current, SCHEMA_VERSION);
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn recall_golden_finds_messages_and_never_hidden_text() {
    let golden = golden("v5_recall.json");
    let hits = |query: &str| golden[query]["results"].as_array().unwrap().len();
    for found in ["needle", "checkpoint", "unicode", "stopped"] {
        assert!(hits(found) > 0, "{found} must have hits");
    }
    for hidden in ["attachmentsecret", "toolsecret", "approvalsecret"] {
        assert_eq!(hits(hidden), 0, "{hidden} lives outside message text");
    }
    assert_eq!(golden.as_object().unwrap().len(), RECALL_QUERIES.len());
}

fn assistants(session: &Value) -> Vec<&Value> {
    session["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .collect()
}
fn transcript(session: &Value) -> &Vec<Value> {
    session["contextState"]["transcript"].as_array().unwrap()
}

#[test]
fn fixtures_have_the_real_agent_shapes() {
    // empty_middle_turn: "\n\n" before every later turn, empty ones included.
    let empty = fixture("empty_middle_turn");
    assert_eq!(assistants(&empty)[0]["content"], "A\n\n\n\nB");
    let turns: Vec<_> = transcript(&empty)
        .iter()
        .filter(|m| m["role"] == "assistant")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(turns, ["A", "", "B"]);
    assert_eq!(assistants(&empty)[0]["tools"][1]["contentOffset"], 3);

    // agent_tools: UI tool ids are iteration_index_callid, three pairs.
    let tools = fixture("agent_tools");
    let ids: Vec<_> = assistants(&tools)[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["0_0_c1", "0_1_c2", "1_0_c3"]);
    assert_eq!(
        assistants(&tools)[0]["tools"][0]["contentOffset"],
        "Looking at the 🦀 crate first.".encode_utf16().count()
    );
    assert_eq!(
        transcript(&tools)
            .iter()
            .filter(|m| m["role"] == "tool")
            .count(),
        3
    );

    // stopped_then_continued: the stopped row's text is echoed into the transcript.
    let stopped = fixture("stopped_then_continued");
    let rows = assistants(&stopped);
    assert_eq!(rows[0]["status"], "stopped");
    assert_eq!(rows[0]["tools"][1]["status"], "stopped");
    let echo = rows[0]["content"].as_str().unwrap();
    assert_eq!(echo, "Let me look.\n\nReading the build script");
    let entries = transcript(&stopped);
    let at = entries
        .iter()
        .position(|m| {
            m["role"] == "assistant" && m["content"] == echo && m.get("tool_calls").is_none()
        })
        .expect("the restore-appended echo");
    assert_eq!(entries[at - 1]["role"], "tool");
    assert_eq!(entries[at + 1]["role"], "user");

    // crashed_streaming: the UI is still streaming, the context stops after the tool pair.
    let crashed = fixture("crashed_streaming");
    assert_eq!(assistants(&crashed).last().unwrap()["status"], "streaming");
    assert_eq!(transcript(&crashed).last().unwrap()["role"], "tool");

    // compacted: three checkpoints and server usage.
    let compacted = fixture("compacted");
    assert_eq!(
        compacted["contextState"]["checkpoints"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert!(compacted["contextState"]["serverUsage"].is_object());
    assert_eq!(compacted["messages"][0]["historical"], true);

    // instructions_and_children.
    let children = fixture("instructions_and_children");
    let row = assistants(&children)[0];
    assert_eq!(row["tools"][0]["name"], "project_instructions");
    assert_eq!(row["tools"][0]["status"], "error");
    assert_eq!(row["children"].as_array().unwrap().len(), 2);

    // system_in_transcript.
    assert_eq!(
        transcript(&fixture("system_in_transcript"))[0]["role"],
        "system"
    );

    // text_tool_mismatch: the row differs from the turns joined.
    let mismatch = fixture("text_tool_mismatch");
    assert_ne!(assistants(&mismatch)[0]["content"], "Checking\n\nAll good");

    // no_context_with_history and null_context_extra_key.
    let none = fixture("no_context_with_history");
    assert!(none.get("contextState").is_none());
    assert_eq!(none["messages"].as_array().unwrap().len(), 4);
    let null = fixture("null_context_extra_key");
    assert!(null["contextState"].is_null());
    assert_eq!(null["pinned"], true);

    // error_rows_both_shapes: desktop row with partial text and error, CLI row with the message.
    let errors = fixture("error_rows_both_shapes");
    let rows = assistants(&errors);
    assert_eq!(rows[0]["status"], "error");
    assert_eq!(rows[0]["content"], "Checking the build\n\nThe build is");
    assert_eq!(rows[0]["error"], "The model stream ended unexpectedly.");
    assert_eq!(rows[1]["status"], "error");
    assert_eq!(
        rows[1]["content"],
        crate::context::ContextError::Limit.to_string()
    );
    assert!(rows[1].get("error").is_none());

    // ids_missing_and_duplicate and bad_role.
    let ids = fixture("ids_missing_and_duplicate");
    assert!(ids["messages"][0].get("id").is_none());
    assert_eq!(ids["messages"][2]["id"], ids["messages"][3]["id"]);
    assert_eq!(fixture("bad_role")["messages"][1]["role"], "tool");

    // The CLI session: an agent session whose rows have no tools but whose context does.
    let cli = fixture("cli_1700000000000_1_0");
    assert_eq!(cli["mode"], "agent");
    assert!(assistants(&cli).iter().all(|m| m.get("tools").is_none()));
    assert!(transcript(&cli).iter().any(|m| m["role"] == "tool"));
    assert!(cli.get("contextReport").is_none());
}

#[test]
fn completed_runs_cover_the_ui_and_stopped_runs_do_not() {
    // complete_source is called only where a run completed: its hash covers the UI rows.
    for id in [
        "desktop_plain",
        "agent_tools",
        "empty_middle_turn",
        "compacted",
    ] {
        let session = fixture(id);
        let rows: Vec<Value> = session["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| json!({"role":m["role"],"content":m["content"]}))
            .collect();
        let state = &session["contextState"];
        assert_eq!(state["coveredMessages"], rows.len(), "{id}");
        assert_eq!(state["sourceHash"], fingerprint(&rows), "{id}");
    }
    // A crashed or failed run leaves only the earlier update: shorter than the rows.
    for id in ["crashed_streaming", "error_rows_both_shapes"] {
        let session = fixture(id);
        let covered = session["contextState"]["coveredMessages"].as_u64().unwrap();
        assert!(
            covered < session["messages"].as_array().unwrap().len() as u64,
            "{id}"
        );
    }
}

#[test]
fn non_continuable_fixtures_are_exactly_those_v5_cannot_restore() {
    // What a v5 desktop or CLI continuation does: the rows plus a new user message become the
    // source, and the saved context must restore against it.
    for session in v5_sessions() {
        let id = session["id"].as_str().unwrap();
        let cli = id.starts_with("cli_");
        let mut chat = Chat::new(99);
        chat.messages = session["messages"].as_array().unwrap().clone();
        chat.user("Continue please", vec![]);
        let source = chat.source(cli);
        let saved: Option<ContextState> = session
            .get("contextState")
            .filter(|v| !v.is_null())
            .map(|v| serde_json::from_value(v.clone()).unwrap());
        let restored = ContextState::restore(&source, saved.as_ref());
        assert_eq!(
            restored.is_err(),
            NON_CONTINUABLE.contains(&id),
            "{id}: continuable={}",
            restored.is_ok()
        );
    }
    for id in NON_CONTINUABLE {
        assert!(v5_sessions().iter().any(|s| s["id"] == *id));
    }
}

#[test]
fn test_paths_are_absolute_on_this_platform_and_goldens_are_neutral() {
    // v6 refuses a relative workspace (`chats::check_mode_workspace`), so every workspace the
    // fixtures and the A5-A7 tests store must be absolute here.
    assert!(Path::new(WORKSPACE).is_absolute());
    // Every POSIX path the A5-A7 tests store, and the fixture workspace itself, which the A6
    // recall test passes through `abs` a second time (CI run 41131851570 on Windows).
    let paths = [
        "/p",
        "/w",
        "/work",
        "/work/a",
        "/work/b",
        "/project",
        "/first",
        "/second",
        "/old",
        GOLDEN_WORKSPACE,
    ];
    for path in paths.into_iter().chain([WORKSPACE]) {
        assert!(Path::new(&abs(path)).is_absolute(), "{path}");
        assert_eq!(abs(&abs(path)), abs(path), "{path}");
    }
    assert_eq!(abs(GOLDEN_WORKSPACE), WORKSPACE);
    // The Windows mapping itself, checked on every platform: idempotent, and undone exactly.
    for path in paths {
        let windows = to_windows(path);
        assert!(
            windows.starts_with("C:\\") && !windows.contains('/'),
            "{windows}"
        );
        assert_eq!(to_windows(&windows), windows);
        assert_eq!(from_windows(&windows).as_deref(), Some(path));
    }
    assert_eq!(to_windows("relative/dir"), "relative/dir");
    assert_eq!(from_windows("/work/project"), None);
    assert_eq!(
        platform_neutral(json!({"hits": [{"project": WORKSPACE}], "other": "/elsewhere"})),
        json!({"hits": [{"project": GOLDEN_WORKSPACE}], "other": "/elsewhere"})
    );
    assert!(golden("v5_recall.json")
        .to_string()
        .contains(GOLDEN_WORKSPACE));
}

#[cfg(windows)]
#[test]
fn windows_spells_test_paths_with_a_drive() {
    assert_eq!(WORKSPACE, r"C:\work\project");
    assert_eq!(abs("/work/a"), r"C:\work\a");
    assert_eq!(abs(WORKSPACE), WORKSPACE);
    assert_eq!(platform_neutral(json!(abs("/p"))), json!("/p"));
    assert!(!Path::new(GOLDEN_WORKSPACE).is_absolute());
    assert_ne!(platform_neutral(json!(WORKSPACE)), json!(WORKSPACE));
}
