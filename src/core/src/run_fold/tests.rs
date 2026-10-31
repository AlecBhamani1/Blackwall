#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::{
    agent::Agent,
    approvals::Approvals,
    context::{ContextRun, ContextState},
    model::{ModelBackend, ModelError, ModelTurn, ToolCall, ToolFunction},
    storage::fixtures::{v5_sessions, NON_CONTINUABLE},
    tools::Workspace,
};
use futures_util::future::BoxFuture;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[test]
fn accumulate_matches_the_agent_separator_rule() {
    assert_eq!(accumulate(["A", "", "B"]), "A\n\n\n\nB");
    assert_eq!(accumulate(["", "B"]), "B");
    assert_eq!(accumulate(["A", ""]), "A\n\n");
    assert_eq!(accumulate(["", "", ""]), "");
    assert_eq!(accumulate(["A"]), "A");
    assert_eq!(accumulate([]), "");
}

fn end_of(value: &Value) -> Option<RunEnd> {
    match value["type"].as_str()? {
        "complete" => Some(RunEnd::Complete),
        "stopped" => Some(RunEnd::Stopped),
        "failed" => Some(RunEnd::Failed {
            code: value["code"].as_str().unwrap().into(),
            message: value["message"].as_str().unwrap().into(),
            detail: value["detail"].as_str().map(Into::into),
        }),
        other => panic!("unknown end {other}"),
    }
}

#[test]
fn fold_run_matches_the_shared_fixture() {
    let fixture: Value = serde_json::from_str(include_str!("assistant_runs.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    for name in [
        "text_only",
        "tools_with_emoji_offsets",
        "instructions_replaced",
        "children_upserted_in_order",
        "reasoning",
        "empty_middle_turn",
        "stopped_mid_tool",
        "failed_with_detail",
        "failed_after_error_event",
        "complete_after_error_event",
        "empty_answer",
        "cli_error",
        "cli_complete",
    ] {
        assert!(cases.iter().any(|case| case["name"] == name), "{name}");
    }
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let events: Vec<AgentEvent> = case["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|wire| {
                let event: AgentEvent = serde_json::from_value(wire.clone()).unwrap();
                // Stored exactly as the backend sends it.
                assert_eq!(&serde_json::to_value(&event).unwrap(), wire, "{name}");
                event
            })
            .collect();
        let shape: AssistantShape = serde_json::from_value(case["shape"].clone()).unwrap();
        let end = end_of(&case["end"]);
        let folded = fold_run(
            case["id"].as_str().unwrap(),
            case["createdAt"].as_i64().unwrap(),
            shape,
            &events,
            end.as_ref(),
        );
        assert_eq!(folded, case["expected"], "{name}");
        // The step-by-step fold is the same fold.
        let mut fold = RunFold::new(
            case["id"].as_str().unwrap(),
            case["createdAt"].as_i64().unwrap(),
            shape,
        );
        events.iter().for_each(|event| fold.apply(event));
        if let Some(end) = &end {
            fold.finish(end);
        }
        assert_eq!(fold.message(), &case["expected"], "{name}");
    }
}

#[test]
fn a_finished_fold_ignores_later_events_and_ends() {
    let delta = AgentEvent::AssistantDelta {
        request_id: "r".into(),
        delta: "Late".into(),
    };
    for shape in [AssistantShape::Desktop, AssistantShape::Cli] {
        let mut fold = RunFold::new("m", 1, shape);
        fold.apply(&AgentEvent::AssistantDelta {
            request_id: "r".into(),
            delta: "Done".into(),
        });
        fold.finish(&RunEnd::Complete);
        let finished = fold.message().clone();
        fold.apply(&delta);
        fold.finish(&RunEnd::Stopped);
        fold.finish(&RunEnd::Failed {
            code: "model_busy".into(),
            message: "busy".into(),
            detail: None,
        });
        assert_eq!(fold.message(), &finished);
        assert_eq!(finished["status"], "complete");
    }
}

#[test]
fn reasoning_is_clipped_like_the_webview() {
    let turn = |reasoning: String| AgentEvent::TurnComplete {
        request_id: "r".into(),
        message: ChatMessage::new(crate::MessageRole::Assistant, "A"),
        finish_reason: None,
        usage: None,
        reasoning: Some(reasoning),
        reasoning_ms: Some(1),
    };
    let fold = |reasoning: String| {
        fold_run(
            "m",
            1,
            AssistantShape::Desktop,
            &[turn(reasoning)],
            Some(&RunEnd::Stopped),
        )["reasoning"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    // ipc.ts keeps the first 200,000 UTF-16 units.
    let exact = "🦀".repeat(100_000);
    assert_eq!(fold(exact.clone()), exact);
    assert_eq!(fold(format!("{exact}x")), exact);
    // A cut through a surrogate pair leaves a lone surrogate in JavaScript, which shows as U+FFFD.
    let odd = format!("x{exact}");
    assert_eq!(fold(odd), format!("x{}\u{FFFD}", "🦀".repeat(99_999)));
}

// ---------------------------------------------------------------------------------------------
// The webview's error copy
// ---------------------------------------------------------------------------------------------

fn ui_file(name: &str) -> String {
    let path = format!("{}/../../ui/src/lib/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .unwrap()
        .replace("\r\n", "\n")
}

/// The quoted literal at the start of `text` (`'…'` or `` `…` ``) and what follows it.
fn literal(text: &str) -> (&str, &str) {
    let quote = text.chars().next().unwrap();
    assert!(quote == '\'' || quote == '`', "{text}");
    let end = text[1..].find(quote).unwrap() + 1;
    let value = &text[1..end];
    assert!(!value.contains('\\'), "escapes are not parsed: {value}");
    (value, &text[end + 1..])
}

/// `platform.storeHelp` from `platform.ts` for the OS these tests run on.
fn webview_store_help() -> String {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    let source = ui_file("platform.ts");
    let block = &source[source.find(&format!("\n  {os}: {{")).unwrap()..];
    let field = &block[block.find("storeHelp:").unwrap() + "storeHelp:".len()..];
    literal(field.trim_start()).0.to_owned()
}

/// Every `code: 'copy'` entry of `nativeModelError` in `modelError.ts`.
fn webview_messages() -> Vec<(String, String)> {
    let source = ui_file("modelError.ts");
    let start = source
        .find("const messages: Record<string, string> = {")
        .unwrap();
    let block = &source[start..];
    let mut rest = block[block.find('{').unwrap() + 1..block.find("\n  };").unwrap()].trim_start();
    let help = webview_store_help();
    let mut messages = vec![];
    while !rest.is_empty() {
        let colon = rest.find(':').unwrap();
        let code = rest[..colon].trim().to_owned();
        let (copy, after) = literal(rest[colon + 1..].trim_start());
        messages.push((code, copy.replace("${platform.storeHelp}", &help)));
        rest = after.trim_start().trim_start_matches(',').trim_start();
    }
    messages
}

#[test]
fn model_error_text_matches_the_webview_copy() {
    let messages = webview_messages();
    assert_eq!(messages.len(), 17);
    for (code, copy) in &messages {
        assert_eq!(model_error_text(code, None), *copy, "{code}");
        assert_eq!(model_error_text(code, Some("")), *copy, "{code}");
        assert_eq!(
            model_error_text(code, Some("llama.cpp: Loading model")),
            format!("{copy} (llama.cpp: Loading model)"),
            "{code}"
        );
    }
    assert!(model_error_text("credential_error", None).contains(&webview_store_help()));
    let generic = "The model could not complete the request. Try again or check the model service.";
    assert_eq!(model_error_text("model_http_error", None), generic);
    assert_eq!(model_error_text("", None), generic);
    // `Object.hasOwn`: an inherited name is not a known code.
    assert_eq!(model_error_text("toString", None), generic);
    let memory =
        "Ollama: model requires more system memory (24.1 GiB) than is available (12.0 GiB)";
    assert_eq!(model_error_text("model_http_error", Some(memory)), memory);
    // serverDetail: control runs, then whitespace runs, become one space; then trim and clip.
    let noisy = format!("bad\u{1b}[31m\nline {}", "x".repeat(500));
    assert_eq!(
        model_error_text("model_http_error", Some(&noisy)),
        format!("bad [31m line {}…", "x".repeat(285))
    );
    assert_eq!(
        model_error_text("model_http_error", Some(&"y".repeat(400))),
        format!("{}…", "y".repeat(299))
    );
    let exact = "z".repeat(300);
    assert_eq!(model_error_text("model_http_error", Some(&exact)), exact);
    // JavaScript `\s` has U+FEFF but not U+0085; U+0085 is a C1 control, so it is a space too.
    assert_eq!(
        model_error_text("model_http_error", Some("\u{feff}a\u{85}b\u{a0}\u{2028}c ")),
        "a b c"
    );
    assert_eq!(
        model_error_text("model_busy", Some(" \u{0}\n\t ")),
        model_error_text("model_busy", None)
    );
    // The clip counts UTF-16 units; a cut surrogate pair shows as U+FFFD.
    let crab = format!("{}🦀yy", "x".repeat(298));
    assert_eq!(
        model_error_text("model_http_error", Some(&crab)),
        format!("{}\u{FFFD}…", "x".repeat(298))
    );
}

// ---------------------------------------------------------------------------------------------
// Run sources
// ---------------------------------------------------------------------------------------------

#[test]
fn run_source_keeps_each_clients_rule() {
    let rows = [
        json!({"id":"u1","role":"user","content":"Hi","status":"complete","createdAt":1,
            "attachments":[{"id":"t","name":"t.txt","mimeType":"text/plain","sizeBytes":1,"kind":"text","textContent":"T"}]}),
        json!({"id":"a1","role":"assistant","content":"Bad","attachments":[],"status":"error","error":"x","createdAt":2}),
        json!({"id":"u2","role":"user","content":"Pending","attachments":[],"status":"sending","createdAt":3}),
        json!({"id":"a2","role":"assistant","content":"Response stopped.","attachments":[],"status":"stopped","createdAt":4,
            "tools":[{"id":"0_0_a","name":"shell","arguments":"{}","contentOffset":0,"status":"stopped"}]}),
        json!({"id":"s1","role":"system","content":"Sys","attachments":[],"status":"complete","createdAt":5}),
        json!({"role":"user","content":"No status"}),
    ];
    let hi = json!({"role":"user","content":"Hi\n<attachment name=\"t.txt\">\nT\n</attachment>"});
    let stopped = json!({"role":"assistant","content":"Response stopped."});
    let last = json!({"role":"user","content":"No status"});
    // store.ts asModelMessages: error and sending rows are dropped.
    assert_eq!(
        run_source(&rows, SourceRule::Desktop).unwrap(),
        [
            hi.clone(),
            stopped.clone(),
            json!({"role":"system","content":"Sys"}),
            last.clone()
        ]
    );
    // session.rs: every row is sent; system entries are instructions, not source.
    assert_eq!(
        run_source(&rows, SourceRule::Cli).unwrap(),
        [
            hi,
            json!({"role":"assistant","content":"Bad"}),
            json!({"role":"user","content":"Pending"}),
            stopped,
            last
        ]
    );
    let broken = [json!({"role":"user","content":5})];
    for rule in [SourceRule::Desktop, SourceRule::Cli] {
        assert!(matches!(
            run_source(&broken, rule),
            Err(ContextError::History)
        ));
    }
    // The Desktop rule never reads a row it drops.
    let dropped = [json!({"role":"user","content":5,"status":"error"})];
    assert_eq!(
        run_source(&dropped, SourceRule::Desktop).unwrap(),
        Vec::<Value>::new()
    );
}

#[test]
fn run_source_continues_every_continuable_v5_fixture() {
    for session in v5_sessions() {
        let id = session["id"].as_str().unwrap();
        if NON_CONTINUABLE.contains(&id) {
            continue;
        }
        let rule = if id.starts_with("cli_") {
            SourceRule::Cli
        } else {
            SourceRule::Desktop
        };
        let mut rows = session["messages"].as_array().unwrap().clone();
        rows.push(json!({"id":"next","role":"user","content":"Continue please","attachments":[],"status":"complete","createdAt":1}));
        let saved: Option<ContextState> = session
            .get("contextState")
            .filter(|state| !state.is_null())
            .map(|state| serde_json::from_value(state.clone()).unwrap());
        let source = run_source(&rows, rule).unwrap();
        assert!(
            ContextState::restore(&source, saved.as_ref()).is_ok(),
            "{id}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Live runs: the fold against the transcript the run itself records
// ---------------------------------------------------------------------------------------------

struct Scripted {
    turns: Mutex<VecDeque<ModelTurn>>,
}

impl Scripted {
    fn new(turns: Vec<ModelTurn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
        }
    }
}

impl ModelBackend for Scripted {
    fn generate<'a>(
        &'a self,
        _: &'a [Value],
        _: &'a [Value],
        delta: &'a mut (dyn FnMut(String) + Send),
    ) -> BoxFuture<'a, Result<ModelTurn, ModelError>> {
        Box::pin(async move {
            let turn = self.turns.lock().unwrap().pop_front().unwrap();
            if !turn.content.is_empty() {
                delta(turn.content.clone());
            }
            Ok(turn)
        })
    }
}

fn model_turn(content: &str, calls: &[(&str, &str)]) -> ModelTurn {
    ModelTurn {
        content: content.into(),
        calls: calls
            .iter()
            .map(|(id, name)| ToolCall {
                id: (*id).into(),
                function: ToolFunction {
                    name: (*name).into(),
                    arguments: json!({"path": format!("{id}-missing.txt"), "command": "true"})
                        .to_string(),
                },
            })
            .collect(),
        ..Default::default()
    }
}

/// Two tool turns (the second with no text) and a final answer.
fn three_turns() -> Vec<ModelTurn> {
    let mut first = model_turn("Looking 🦀", &[("read1", "read_file")]);
    first.reasoning = "Plan the reads.".into();
    first.reasoning_ms = 7;
    vec![
        first,
        model_turn("", &[("read2", "read_file")]),
        model_turn("Done.", &[]),
    ]
}

/// The assistant contents this run added to the transcript, in order.
fn run_turns(state: &ContextState, before: usize) -> Vec<String> {
    state.transcript[before..]
        .iter()
        .filter(|entry| entry["role"] == "assistant")
        .map(|entry| entry["content"].as_str().unwrap().to_owned())
        .collect()
}

/// For the whole run: the completed fold is the accumulated turns and matches `TurnComplete`.
/// For every prefix: whatever a stopped or failed fold shows starts with the turns the last
/// `ContextUpdated` holds, so it is `accumulate(turns) + tail`.
fn assert_turns_and_tail(events: &[AgentEvent], before: usize) -> Value {
    let folded = fold_run(
        "m",
        1,
        AssistantShape::Desktop,
        events,
        Some(&RunEnd::Complete),
    );
    let last_state = |events: &[AgentEvent]| {
        events.iter().rev().find_map(|event| match event {
            AgentEvent::ContextUpdated { state, .. } => Some(state.clone()),
            _ => None,
        })
    };
    let state = last_state(events).unwrap();
    let turns = run_turns(&state, before);
    assert_eq!(turns.len(), 3, "{turns:?}");
    assert_eq!(
        folded["content"],
        accumulate(turns.iter().map(String::as_str))
    );
    assert_eq!(folded["status"], "complete");
    let finished = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::TurnComplete {
                message, reasoning, ..
            } => Some((message.content.clone(), reasoning.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(folded["content"], finished.0.as_str());
    assert_eq!(folded["reasoning"], json!(finished.1));
    for cut in 0..=events.len() {
        let prefix = &events[..cut];
        let covered = last_state(prefix)
            .map(|state| accumulate(run_turns(&state, before).iter().map(String::as_str)))
            .unwrap_or_default();
        for (shape, end) in [
            (AssistantShape::Desktop, RunEnd::Stopped),
            (
                AssistantShape::Cli,
                RunEnd::Failed {
                    code: "model_busy".into(),
                    message: String::new(),
                    detail: None,
                },
            ),
        ] {
            let shown = fold_run("m", 1, shape, prefix, Some(&end));
            let content = shown["content"].as_str().unwrap();
            assert!(
                content.starts_with(&covered),
                "cut {cut}: {content:?} vs {covered:?}"
            );
        }
    }
    folded
}

fn scratch() -> std::path::PathBuf {
    let directory =
        std::env::temp_dir().join(format!("blackwall-run-fold-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

#[tokio::test]
async fn a_live_agent_run_folds_to_turns_and_tail() {
    let directory = scratch();
    let backend = Scripted::new(three_turns());
    let events = Arc::new(Mutex::new(vec![]));
    let captured = events.clone();
    let agent = Agent {
        backend: &backend,
        workspace: Workspace::open(&directory).unwrap(),
        web_enabled: false,
        approvals: Approvals::default(),
        emit: Arc::new(move |event| captured.lock().unwrap().push(event)),
    };
    let answer = agent
        .run(
            "live",
            vec![json!({"role":"user","content":"Read two files"})],
        )
        .await
        .unwrap();
    let events = events.lock().unwrap().clone();
    let folded = assert_turns_and_tail(&events, 1);
    assert_eq!(folded["content"], answer.as_str());
    assert_eq!(answer, "Looking 🦀\n\n\n\nDone.");
    let tools = folded["tools"].as_array().unwrap();
    let offsets: Vec<_> = tools
        .iter()
        .map(|tool| (tool["id"].as_str().unwrap(), tool["contentOffset"].clone()))
        .collect();
    assert_eq!(
        offsets,
        [
            ("instructions", json!(0)),
            ("0_0_read1", json!(10)),
            ("1_0_read2", json!(12))
        ]
    );
    assert!(tools[1..].iter().all(|tool| tool["status"] == "error"));
    drop(agent);
    std::fs::remove_dir_all(directory).unwrap();
}

/// Chat mode runs `context::chat_traced`, not the agent loop; its separator, transcript and
/// events fold the same way (A1's open point).
#[tokio::test]
async fn a_live_chat_run_folds_to_turns_and_tail() {
    // Tools that chat never offers are refused without approval or network.
    let mut turns = three_turns();
    turns[1] = model_turn("", &[("shell1", "shell")]);
    let backend = Scripted::new(turns);
    let source = vec![json!({"role":"user","content":"Read two files"})];
    let run = ContextRun {
        state: ContextState::restore(&source, None).unwrap(),
        source,
        budget: Default::default(),
        instructions: vec![],
        compact: false,
        trace: Default::default(),
        subagents: Default::default(),
        instruction_settings: Default::default(),
    };
    let events = Mutex::new(vec![]);
    let approvals = Approvals::default();
    let answer = crate::context::chat_with_web(&backend, "chat", run, &approvals, None, &|event| {
        events.lock().unwrap().push(event)
    })
    .await
    .unwrap();
    let events = events.into_inner().unwrap();
    let folded = assert_turns_and_tail(&events, 1);
    assert_eq!(folded["content"], answer.as_str());
    let ids: Vec<_> = folded["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| {
            (
                tool["id"].as_str().unwrap(),
                tool["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(ids, [("0_0_read1", "error"), ("1_0_shell1", "error")]);

    // Plain chat: one turn, no tools, the same fold.
    let backend = Scripted::new(vec![model_turn("Plain 🦀 answer.", &[])]);
    let source = vec![json!({"role":"user","content":"Hi"})];
    let run = ContextRun {
        state: ContextState::restore(&source, None).unwrap(),
        source,
        budget: Default::default(),
        instructions: vec![],
        compact: false,
        trace: Default::default(),
        subagents: Default::default(),
        instruction_settings: Default::default(),
    };
    let events = Mutex::new(vec![]);
    let answer = crate::context::chat(&backend, "plain", run, &|event| {
        events.lock().unwrap().push(event)
    })
    .await
    .unwrap();
    let events = events.into_inner().unwrap();
    let folded = fold_run(
        "m",
        1,
        AssistantShape::Desktop,
        &events,
        Some(&RunEnd::Complete),
    );
    assert_eq!(folded["content"], answer.as_str());
    let state = events
        .iter()
        .rev()
        .find_map(|event| match event {
            AgentEvent::ContextUpdated { state, .. } => Some(state),
            _ => None,
        })
        .unwrap();
    assert_eq!(run_turns(state, 1), std::slice::from_ref(&answer));
    assert!(folded.get("tools").is_none());
}
