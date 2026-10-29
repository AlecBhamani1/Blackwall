//! Agent runs against a scripted OpenAI-compatible server: reasoning and text tool calls.
use super::*;
use crate::protocol::{ApprovalDecision, ResolveApprovalRequest};
use std::sync::Mutex;

fn sse(frames: &[Value]) -> String {
    let mut body: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    body
}

/// A chat server answering request N with `replies[N]` and recording each request body.
async fn scripted(replies: Vec<String>) -> (crate::test_http::Fake, Arc<Mutex<Vec<String>>>) {
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = bodies.clone();
    let fake = crate::test_http::serve(move |method, path, body| {
        if method != "POST" || path != "/v1/chat/completions" {
            return None;
        }
        let mut seen = seen.lock().unwrap();
        seen.push(body.to_owned());
        replies
            .get(seen.len() - 1)
            .map(|reply| (200, reply.clone()))
    })
    .await;
    (fake, bodies)
}

fn folder(name: &str) -> std::path::PathBuf {
    let directory =
        std::env::temp_dir().join(format!("blackwall-{name}-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("notes.txt"), "hello").unwrap();
    directory
}

async fn run(
    backend: &crate::model::HttpModel,
    directory: &std::path::Path,
    approvals: Approvals,
    on_event: impl Fn(&AgentEvent) + Send + Sync + 'static,
) -> (Result<String, AgentError>, Vec<AgentEvent>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let agent = Agent {
        backend,
        workspace: Workspace::open(directory).unwrap(),
        web_enabled: false,
        approvals,
        emit: Arc::new(move |event| {
            on_event(&event);
            captured.lock().unwrap().push(event);
        }),
    };
    let source = vec![json!({"role":"user","content":"Look around."})];
    let result = agent
        .run_context_with_guidance(
            "local",
            crate::context::ContextRun {
                state: crate::context::ContextState::restore(&source, None).unwrap(),
                source,
                budget: Default::default(),
                instructions: vec![],
                compact: false,
                trace: None,
                subagents: Default::default(),
                instruction_settings: Default::default(),
            },
            None,
            None,
        )
        .await;
    drop(agent);
    let events = events.lock().unwrap().clone();
    (result, events)
}

fn shown(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::AssistantDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn reasoning_is_never_sent_back_to_the_model() {
    let (fake, bodies) = scripted(vec![
        sse(&[
            json!({"choices":[{"delta":{"reasoning_content":"FIELD-THOUGHT"}}]}),
            json!({"choices":[{"delta":{"content":"<think>INLINE-THOUGHT</think>Checking."}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"list_files","arguments":"{\"path\":\".\"}"}}]},"finish_reason":"tool_calls"}]}),
        ]),
        sse(&[json!({"choices":[{"delta":{"content":"Done."},"finish_reason":"stop"}]})]),
    ])
    .await;
    let directory = folder("reasoning");
    let backend =
        crate::model::HttpModel::new(&format!("{}/v1", fake.origin()), "m", None).unwrap();
    let (result, events) = run(&backend, &directory, Approvals::default(), |_| {}).await;
    assert_eq!(result.unwrap(), "Checking.\n\nDone.");
    let shown = shown(&events);
    assert!(!shown.contains("THOUGHT"), "{shown}");
    let bodies = bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2);
    assert!(!bodies[1].contains("THOUGHT"), "{}", bodies[1]);
    let complete = events.iter().find_map(|event| match event {
        AgentEvent::TurnComplete {
            reasoning,
            reasoning_ms,
            ..
        } => Some((reasoning.clone(), *reasoning_ms)),
        _ => None,
    });
    let (reasoning, ms) = complete.unwrap();
    assert_eq!(reasoning.as_deref(), Some("FIELD-THOUGHTINLINE-THOUGHT"));
    assert!(ms.is_some());
    std::fs::remove_dir_all(directory).unwrap();
}

/// A server that writes a `write_file` call as text, then finishes.
async fn text_call_server() -> (crate::test_http::Fake, Arc<Mutex<Vec<String>>>) {
    let call = r#"<tool_call>{"name":"write_file","arguments":{"path":"notes.txt","content":"changed"}}</tool_call>"#;
    scripted(vec![
        sse(&[
            json!({"choices":[{"delta":{"content":format!("Updating notes.\n{call}")},"finish_reason":"stop"}]}),
        ]),
        sse(&[json!({"choices":[{"delta":{"content":"Done."},"finish_reason":"stop"}]})]),
    ])
    .await
}

/// Answer every approval with `decision` and record each prompt's detail.
fn answering(
    approvals: &Approvals,
    decision: ApprovalDecision,
    asked: Arc<Mutex<Vec<String>>>,
) -> impl Fn(&AgentEvent) + Send + Sync + 'static {
    let approvals = approvals.clone();
    move |event| {
        if let AgentEvent::ApprovalRequest {
            request_id,
            approval_id,
            detail,
            ..
        } = event
        {
            asked.lock().unwrap().push(detail.clone());
            let approvals = approvals.clone();
            let request = ResolveApprovalRequest {
                request_id: request_id.clone(),
                approval_id: approval_id.clone(),
                decision,
            };
            // The prompt is registered before it is emitted, so it can be resolved now.
            approvals.resolve(request).unwrap();
        }
    }
}

async fn run_with_policy(
    backend: &crate::model::HttpModel,
    project: &std::path::Path,
    user: &std::path::Path,
    approvals: Approvals,
    on_event: impl Fn(&AgentEvent) + Send + Sync + 'static,
) -> (Result<String, AgentError>, Vec<AgentEvent>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let agent = Agent {
        backend,
        workspace: Workspace::open(project).unwrap(),
        web_enabled: false,
        approvals,
        emit: Arc::new(move |event| {
            on_event(&event);
            captured.lock().unwrap().push(event);
        }),
    };
    let source = vec![json!({"role":"user","content":"Update the notes."})];
    let result = agent
        .run_context_with_guidance(
            "text-call",
            crate::context::ContextRun {
                state: crate::context::ContextState::restore(&source, None).unwrap(),
                source,
                budget: Default::default(),
                instructions: vec![],
                compact: false,
                trace: None,
                subagents: Default::default(),
                instruction_settings: Default::default(),
            },
            None,
            Some(user),
        )
        .await;
    drop(agent);
    let events = events.lock().unwrap().clone();
    (result, events)
}

#[tokio::test]
async fn text_tool_call_always_asks() {
    for decision in [ApprovalDecision::AlwaysAllow, ApprovalDecision::Deny] {
        let (fake, _) = text_call_server().await;
        let project = folder("text-call-project");
        let user = folder("text-call-user");
        // A saved rule allows every file change, so a structured call would not ask.
        std::fs::write(
            user.join(crate::policy::POLICY_FILE),
            r#"{"rules":[{"decision":"allow","kind":"file","path":"."}]}"#,
        )
        .unwrap();
        let backend = crate::model::HttpModel::new(&format!("{}/v1", fake.origin()), "m", None)
            .unwrap()
            .with_text_tool_calls(true);
        let approvals = Approvals::default();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let (result, events) = run_with_policy(
            &backend,
            &project,
            &user,
            approvals.clone(),
            answering(&approvals, decision, asked.clone()),
        )
        .await;
        assert_eq!(result.unwrap(), "Updating notes.\n\nDone.");
        let asked = asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert!(
            asked[0].contains("wrote this tool call as text"),
            "{}",
            asked[0]
        );
        assert!(asked[0].contains("Tool: write_file"));
        let prompt = events.iter().find_map(|event| match event {
            AgentEvent::ApprovalRequest { kind, remember, .. } => Some((*kind, *remember)),
            _ => None,
        });
        // A file change asks as one, and cannot be remembered for the run.
        assert_eq!(prompt, Some((crate::protocol::ApprovalKind::File, false)));
        let shown = shown(&events);
        assert!(!shown.contains("<tool_call>"), "{shown}");
        let written = std::fs::read_to_string(project.join("notes.txt")).unwrap();
        let allowed = decision != ApprovalDecision::Deny;
        assert_eq!(written == "changed", allowed, "{written}");
        std::fs::remove_dir_all(project).unwrap();
        std::fs::remove_dir_all(user).unwrap();
    }
}

#[tokio::test]
async fn text_tool_calls_stay_text_when_the_setting_is_off() {
    let (fake, bodies) = text_call_server().await;
    let project = folder("text-off");
    let backend =
        crate::model::HttpModel::new(&format!("{}/v1", fake.origin()), "m", None).unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let approvals = Approvals::default();
    let (result, events) = run(
        &backend,
        &project,
        approvals.clone(),
        answering(&approvals, ApprovalDecision::Allow, asked.clone()),
    )
    .await;
    assert!(result.unwrap().contains("<tool_call>"));
    assert!(asked.lock().unwrap().is_empty());
    assert!(!events
        .iter()
        .any(|event| matches!(event, AgentEvent::ToolCall { .. })));
    assert_eq!(bodies.lock().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(project.join("notes.txt")).unwrap(),
        "hello"
    );
    std::fs::remove_dir_all(project).unwrap();
}
