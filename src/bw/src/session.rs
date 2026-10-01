use crate::{input::Input, settings::Settings};
use blackwall_core::{
    agent::Agent,
    approvals::Approvals,
    connection::{normalize_endpoint, same_origin},
    model::{self, HttpModel},
    protocol::{AgentEvent, ApprovalDecision, ResolveApprovalRequest},
    skills::SkillStore,
    storage::LocalStore,
    tools::Workspace,
    ChatMessage, ChatRequest,
};
use serde_json::{json, Value};
use std::{
    env,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn now() -> Result<u64, String> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "The system clock is invalid.")?
        .as_millis() as u64)
}

pub struct Conversation {
    session: Value,
    pub workspace: PathBuf,
    dirty: bool,
}

impl Conversation {
    pub fn new(workspace: PathBuf) -> Result<Self, String> {
        let timestamp = now()?;
        let id = format!(
            "cli_{}_{}_{}",
            std::process::id(),
            timestamp,
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        Ok(Self {
            session: json!({"id":id,"title":"CLI conversation","mode":"agent","workspace":workspace,"updatedAt":timestamp,"messages":[]}),
            workspace,
            dirty: false,
        })
    }

    pub fn id(&self) -> &str {
        self.session["id"].as_str().unwrap_or("")
    }

    pub fn load(store: &LocalStore, id: &str, fallback: &Path) -> Result<Self, String> {
        let mut session = store
            .load_session(id)
            .map_err(|error| error.to_string())?
            .ok_or("That conversation was not found.")?;
        // Old bw sessions have neither mode nor workspace. Do not turn desktop Chat histories
        // into Agent sessions or silently attach tools to unrelated legacy desktop conversations.
        if session["mode"].as_str() != Some("agent")
            && !(session["mode"].is_null() && id.starts_with("cli_"))
        {
            return Err("Only Agent conversations can be resumed in the CLI.".into());
        }
        let path =
            match session["workspace"].as_str() {
                Some(path) if Path::new(path).is_absolute() => Path::new(path),
                None if session["mode"].is_null()
                    && session["workspace"].is_null()
                    && id.starts_with("cli_") =>
                {
                    fallback
                }
                _ => return Err(
                    "This Agent conversation has no valid saved project. Start a new conversation."
                        .into(),
                ),
            };
        let workspace = Workspace::open(path)
            .map_err(|error| error.to_string())?
            .path;
        if !session["messages"].is_array() {
            return Err("The saved conversation is invalid.".into());
        }
        session["mode"] = json!("agent");
        session["workspace"] = json!(workspace);
        Ok(Self {
            session,
            workspace,
            dirty: false,
        })
    }

    pub fn flush(&mut self, store: &LocalStore) -> Result<(), String> {
        if self.dirty {
            store
                .save_session(&self.session)
                .map_err(|error| error.to_string())?;
            self.dirty = false;
        }
        Ok(())
    }

    pub async fn turn(
        &mut self,
        store: &LocalStore,
        directory: &Path,
        settings: &Settings,
        prompt: &str,
        input: &mut Input,
    ) -> Result<(), String> {
        self.flush(store)?;
        let backend = HttpModel::new(
            &settings.endpoint,
            &settings.model,
            environment_key(&settings.endpoint),
        )
        .map_err(|error| error.to_string())?;
        let workspace = Workspace::open(&self.workspace).map_err(|error| error.to_string())?;
        let timestamp = now()?;
        let mut candidate = self.session.clone();
        let history = candidate["messages"]
            .as_array_mut()
            .ok_or("The conversation is invalid.")?;
        if history.is_empty() {
            candidate["title"] = json!(prompt.chars().take(80).collect::<String>());
        }
        candidate["messages"].as_array_mut().ok_or("The conversation is invalid.")?.push(
            json!({"id":format!("user_{timestamp}_{}",SEQUENCE.fetch_add(1,Ordering::Relaxed)),"role":"user","content":prompt,"attachments":[],"status":"complete","createdAt":timestamp})
        );
        candidate["updatedAt"] = json!(timestamp);
        let messages = context(store, directory, settings, &candidate)?;
        // Save the user turn before any action so even a crashed/stopped task is discoverable.
        store
            .save_session(&candidate)
            .map_err(|error| error.to_string())?;
        self.session = candidate;
        let (answer, result) =
            execute(&backend, workspace, settings.web_enabled, messages, input).await;
        let status = if result.is_ok() { "complete" } else { "error" };
        let content = if answer.is_empty() {
            result.as_ref().err().cloned().unwrap_or_default()
        } else {
            answer
        };
        self.session["messages"].as_array_mut().ok_or("The conversation is invalid.")?.push(
            json!({"id":format!("assistant_{timestamp}_{}",SEQUENCE.fetch_add(1,Ordering::Relaxed)),"role":"assistant","content":content,"attachments":[],"status":status,"createdAt":timestamp})
        );
        self.session["updatedAt"] = json!(now()?);
        self.dirty = true;
        self.flush(store)?;
        eprintln!("Saved conversation {}", self.id());
        result
    }
}

fn context(
    store: &LocalStore,
    directory: &Path,
    settings: &Settings,
    session: &Value,
) -> Result<Vec<Value>, String> {
    let history: Vec<ChatMessage> = serde_json::from_value(session["messages"].clone())
        .map_err(|_| "The saved conversation is invalid.")?;
    let request = ChatRequest::new("cli-context", &settings.model, history);
    request.validate().map_err(|error| error.to_string())?;
    let mut messages = model::messages(&request);
    if settings.memory_enabled {
        let memory = blackwall_core::memory::context(
            &store.memories("").map_err(|error| error.to_string())?,
            4000,
        );
        if !memory.is_empty() {
            messages.insert(0, json!({"role":"system","content":memory}));
        }
    }
    let skills = SkillStore::open(&directory.join("skills")).map_err(|error| error.to_string())?;
    skills.seed().map_err(|error| error.to_string())?;
    let (enabled, warnings) = skills.list().map_err(|error| error.to_string())?;
    for warning in warnings {
        eprintln!("Skill: {warning}");
    }
    let workflows = blackwall_core::skills::prompt(&enabled);
    if !workflows.is_empty() {
        messages.insert(0, json!({"role":"system","content":workflows}));
    }
    Ok(messages)
}

fn environment_key(endpoint: &str) -> Option<String> {
    let configured = env::var("BLACKWALL_MODEL_ENDPOINT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            env::var("OLLAMA_HOST")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| blackwall_core::protocol::DEFAULT_MODEL_ENDPOINT.into());
    scoped_key(
        endpoint,
        &configured,
        env::var("BLACKWALL_MODEL_API_KEY").ok(),
    )
}

fn scoped_key(endpoint: &str, configured: &str, key: Option<String>) -> Option<String> {
    normalize_endpoint(configured)
        .ok()
        .filter(|origin| same_origin(endpoint, origin))
        .and(key)
}

async fn execute(
    backend: &HttpModel,
    workspace: Workspace,
    web_enabled: bool,
    messages: Vec<Value>,
    input: &mut Input,
) -> (String, Result<(), String>) {
    let approvals = Approvals::default();
    let (events, mut incoming) = tokio::sync::mpsc::unbounded_channel();
    let emit = Arc::new(move |event| {
        let _ = events.send(event);
    });
    let agent = Agent {
        backend,
        workspace,
        web_enabled,
        approvals: approvals.clone(),
        emit,
    };
    let request_id = format!("cli_run_{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let work = agent.run(&request_id, messages);
    tokio::pin!(work);
    let mut answer = String::new();
    let mut pending: Option<ResolveApprovalRequest> = None;
    let result = loop {
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => break Err("Task stopped. You can continue this conversation.".into()),
            line = input.read(), if pending.is_some() => {
                match line {
                    Ok(line) => {
                        if let Some(mut request) = pending.take() {
                            request.decision = if line.as_deref().is_some_and(|line| line.trim() == "yes") { ApprovalDecision::Allow } else { ApprovalDecision::Deny };
                            if let Err(error) = approvals.resolve(request) { eprintln!("bw: {error}"); }
                        }
                    }
                    Err(error) => break Err(error),
                }
            }
            event = incoming.recv() => if let Some(event) = event { match event {
                AgentEvent::AssistantDelta { delta, .. } => { answer.push_str(&delta); print!("{delta}"); let _ = io::stdout().flush(); }
                AgentEvent::ToolCall { name, .. } => eprintln!("\n[{name}]"),
                AgentEvent::ToolResult { success: false, output, .. } => eprintln!("{output}"),
                AgentEvent::ApprovalRequest { request_id, approval_id, detail, .. } => {
                    eprintln!("\n{detail}\n\nAllow this action once? Type yes to allow; anything else denies.");
                    pending = Some(ResolveApprovalRequest { request_id, approval_id, decision: ApprovalDecision::Deny });
                }
                AgentEvent::SubagentStatus { state, summary, .. } => eprintln!("\n[agent {state:?}] {}", summary.unwrap_or_default()),
                _ => {}
            } },
            result = &mut work => match result {
                Ok(complete) => break Ok(complete),
                Err(error) => break Err(error.to_string()),
            },
        }
    };
    // A ready model future can enqueue events and complete in one poll. Render those
    // deltas before returning so even a fast final response reaches stdout.
    while let Ok(event) = incoming.try_recv() {
        if let AgentEvent::AssistantDelta { delta, .. } = event {
            answer.push_str(&delta);
            print!("{delta}");
        }
    }
    let _ = io::stdout().flush();
    let result = result.map(|complete| {
        answer = complete;
    });
    println!();
    (answer, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_credentials_stay_at_their_origin() {
        let key = Some("fixture-key".into());
        assert_eq!(
            scoped_key(
                "https://model.example/api",
                "https://model.example/v1",
                key.clone()
            ),
            key
        );
        assert_eq!(
            scoped_key(
                "https://other.example/v1",
                "https://model.example/v1",
                key.clone()
            ),
            None
        );
        assert_eq!(
            scoped_key(
                "http://model.example/v1",
                "https://model.example/v1",
                key.clone()
            ),
            None
        );
        assert_eq!(
            scoped_key(
                "https://model.example/v1",
                "https://user:secret@model.example",
                key
            ),
            None
        );
    }
}
