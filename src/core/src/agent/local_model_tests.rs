//! Agent runs against a scripted OpenAI-compatible server: reasoning and text tool calls.
use super::*;
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
