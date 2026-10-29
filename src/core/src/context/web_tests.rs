#![allow(clippy::unwrap_used)]
use super::*;
use crate::{
    approvals::Approvals,
    model::{ModelTurn, ToolCall, ToolFunction},
    protocol::{AgentEvent, ApprovalDecision, ResolveApprovalRequest},
};
use futures_util::future::BoxFuture;
use std::{collections::VecDeque, sync::Mutex};

struct Scripted {
    turns: Mutex<VecDeque<ModelTurn>>,
    requests: Mutex<Vec<(Vec<Value>, Vec<Value>)>>,
}

impl Scripted {
    fn new(turns: Vec<ModelTurn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            requests: Mutex::new(vec![]),
        }
    }
}

impl ModelBackend for Scripted {
    fn generate<'a>(
        &'a self,
        messages: &'a [Value],
        tools: &'a [Value],
        delta: &'a mut (dyn FnMut(String) + Send),
    ) -> BoxFuture<'a, Result<ModelTurn, ModelError>> {
        Box::pin(async move {
            validate_history(messages).unwrap();
            self.requests
                .lock()
                .unwrap()
                .push((messages.to_vec(), tools.to_vec()));
            let turn = self.turns.lock().unwrap().pop_front().unwrap();
            delta(turn.content.clone());
            Ok(turn)
        })
    }
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: format!("call_{name}"),
        function: ToolFunction {
            name: name.into(),
            arguments: arguments.to_string(),
        },
    }
}

fn research(calls: Vec<ToolCall>) -> ModelTurn {
    ModelTurn {
        content: "Checking the web.".into(),
        calls,
        usage: Some(TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 2,
            total_tokens: 12,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn answer() -> ModelTurn {
    ModelTurn {
        content: "Here is the result.".into(),
        usage: Some(TokenUsage {
            prompt_tokens: 20,
            completion_tokens: 3,
            total_tokens: 23,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn run() -> ContextRun {
    let source = vec![json!({"role":"user","content":"Check this page."})];
    ContextRun {
        state: ContextState::restore(&source, None).unwrap(),
        source,
        budget: Default::default(),
        instructions: vec![],
        compact: false,
        trace: Default::default(),
        subagents: Default::default(),
    }
}

fn decide(approvals: &Approvals, event: &AgentEvent, decision: ApprovalDecision) {
    if let AgentEvent::ApprovalRequest {
        request_id,
        approval_id,
        ..
    } = event
    {
        approvals
            .resolve(ResolveApprovalRequest {
                request_id: request_id.clone(),
                approval_id: approval_id.clone(),
                decision,
            })
            .unwrap();
    }
}

#[tokio::test]
async fn web_chat_approves_then_blocks_private_hosts_and_resumes_tool_history() {
    // Approval must not bypass SSRF protection; this makes no HTTP request.
    let model = Scripted::new(vec![
        research(vec![call(
            "web_fetch",
            json!({"url":"http://127.0.0.1/","purpose":"Read the supplied page"}),
        )]),
        answer(),
    ]);
    let approvals = Approvals::default();
    let events = Mutex::new(vec![]);
    let initial = run();
    let mut source = initial.source.clone();
    let result = chat_with_web(&model, "web", initial, &approvals, None, &|event| {
        decide(&approvals, &event, ApprovalDecision::Allow);
        events.lock().unwrap().push(event);
    })
    .await
    .unwrap();
    assert_eq!(result, "Checking the web.\n\nHere is the result.");
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let names: Vec<_> = requests[0]
        .1
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["web_fetch", "web_search"]);
    assert!(requests[1].0.last().unwrap()["content"]
        .as_str()
        .unwrap()
        .contains("private"));
    let events = events.lock().unwrap();
    assert!(events
        .iter()
        .any(|event| matches!(event, AgentEvent::ApprovalRequest { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, AgentEvent::ToolResult { success: false, .. })));
    assert!(events.iter().any(|event| matches!(event, AgentEvent::TurnComplete { usage: Some(usage), .. } if usage.total_tokens == 35)));
    let saved = events
        .iter()
        .rev()
        .find_map(|event| match event {
            AgentEvent::ContextUpdated { state, .. } => Some(state),
            _ => None,
        })
        .unwrap();
    source.push(json!({"role":"assistant","content":result}));
    source.push(json!({"role":"user","content":"What happened?"}));
    let restored = ContextState::restore(&source, Some(saved)).unwrap();
    assert!(restored
        .transcript
        .iter()
        .any(|message| message["role"] == "tool"));
    validate_history(&restored.transcript).unwrap();
}

#[tokio::test]
async fn denied_search_returns_an_error_to_the_model_without_network_access() {
    let model = Scripted::new(vec![
        research(vec![call(
            "web_search",
            json!({"query":"current news","purpose":"Find sources"}),
        )]),
        answer(),
    ]);
    let approvals = Approvals::default();
    chat_with_web(&model, "deny", run(), &approvals, None, &|event| {
        decide(&approvals, &event, ApprovalDecision::Deny)
    })
    .await
    .unwrap();
    let requests = model.requests.lock().unwrap();
    assert!(requests[1].0.last().unwrap()["content"]
        .as_str()
        .unwrap()
        .contains("denied"));
}

#[tokio::test]
async fn web_chat_rejects_every_other_capability_without_asking_for_approval() {
    let calls = [
        "read_file",
        "write_file",
        "shell",
        "spawn_agent",
        "memory_list",
        "read_skill",
        "extension__tool",
    ]
    .into_iter()
    .map(|name| call(name, json!({})))
    .collect();
    let model = Scripted::new(vec![research(calls), answer()]);
    let events = Mutex::new(vec![]);
    chat_with_web(
        &model,
        "restricted",
        run(),
        &Approvals::default(),
        None,
        &|event| events.lock().unwrap().push(event),
    )
    .await
    .unwrap();
    let events = events.lock().unwrap();
    assert!(!events
        .iter()
        .any(|event| matches!(event, AgentEvent::ApprovalRequest { .. })));
    assert_eq!(events.iter().filter(|event| matches!(event, AgentEvent::ToolResult { success: false, output, .. } if output.contains("denied"))).count(), 7);
}

#[tokio::test]
async fn disabled_web_rejects_unexpected_calls_and_exposes_no_tools() {
    let model = Scripted::new(vec![research(vec![call("web_search", json!({}))])]);
    assert!(matches!(
        chat(&model, "off", run(), &|_| {}).await,
        Err(ContextError::History)
    ));
    assert!(model.requests.lock().unwrap()[0].1.is_empty());
}

#[tokio::test]
async fn research_stops_at_the_iteration_limit_without_completing_the_turn() {
    let model = Scripted::new(vec![research(vec![call(
        "web_fetch",
        json!({"url":"file:///etc/passwd","purpose":"read"}),
    )])]);
    let mut run = run();
    run.budget.max_iterations = 1;
    let events = Mutex::new(vec![]);
    assert!(matches!(
        chat_with_web(
            &model,
            "limit",
            run,
            &Approvals::default(),
            None,
            &|event| events.lock().unwrap().push(event)
        )
        .await,
        Err(ContextError::Iterations)
    ));
    assert!(!events.lock().unwrap().iter().any(|event| matches!(
        event,
        AgentEvent::TurnComplete { .. } | AgentEvent::ApprovalRequest { .. }
    )));
}

#[tokio::test]
async fn cancellation_invalidates_pending_web_approvals() {
    let model = Scripted::new(vec![research(vec![call(
        "web_fetch",
        json!({"url":"https://example.com/","purpose":"Read example"}),
    )])]);
    let approvals = Approvals::default();
    let pending = Mutex::new(None);
    let emit = |event| {
        if let AgentEvent::ApprovalRequest {
            request_id,
            approval_id,
            ..
        } = event
        {
            *pending.lock().unwrap() = Some(ResolveApprovalRequest {
                request_id,
                approval_id,
                decision: ApprovalDecision::Allow,
            });
        }
    };
    let mut future = Box::pin(chat_with_web(
        &model,
        "cancel",
        run(),
        &approvals,
        None,
        &emit,
    ));
    tokio::select! {
        result = &mut future => panic!("Approval should wait: {result:?}"),
        () = async { while pending.lock().unwrap().is_none() { tokio::task::yield_now().await; } } => {}
    }
    drop(future);
    assert!(approvals
        .resolve(pending.lock().unwrap().take().unwrap())
        .is_err());
}
