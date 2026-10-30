use crate::{input::Input, project::safe, settings::Settings};
use blackwall_core::{
    agent::{Agent, RunReason},
    approvals::Approvals,
    command_tools::OUTSIDE_NOTE,
    connection::{normalize_endpoint, same_origin, served_budget},
    context::{ContextRun, ContextState},
    memory::{MemoryClient, MemoryTools},
    model::{self, HttpModel},
    protocol::{AgentEvent, ApprovalDecision, ResolveApprovalRequest, ServerDiagnostic},
    skills::SkillStore,
    storage::LocalStore,
    storage::MemorySource,
    tools::Workspace,
    ChatMessage, ChatRequest, TokenUsage,
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
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// A failed run and why it ended. Plain string errors are `RunReason::Error`.
#[derive(Debug)]
pub struct Failure {
    pub reason: RunReason,
    pub message: String,
}

impl Failure {
    pub fn new(reason: RunReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::new(RunReason::Error, message)
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self::new(RunReason::Error, message)
    }
}

/// The headless result line. `usage` counts the run's model tokens as the server reported them.
pub fn result_line(
    session: Option<&str>,
    answer: &str,
    failure: Option<&Failure>,
    reason: RunReason,
    denied: u32,
    usage: TokenUsage,
) -> Value {
    json!({
        "type": "result",
        "sessionId": session,
        "status": if failure.is_none() { "complete" } else { "error" },
        "answer": answer,
        "error": failure.map(|failure| &failure.message),
        "reason": reason,
        "denied": denied,
        "usage": {"input": usage.prompt_tokens, "output": usage.completion_tokens},
    })
}

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

    fn memory_tools(&self, directory: &Path) -> Option<MemoryTools> {
        let message_id = self.session["messages"]
            .as_array()?
            .iter()
            .rev()
            .find(|message| message["role"] == "user")?["id"]
            .as_str()?;
        Some(MemoryTools {
            directory: directory.into(),
            client: MemoryClient::Cli,
            workspace: Some(self.workspace.to_string_lossy().into_owned()),
            source: MemorySource {
                session_id: self.id().into(),
                message_id: message_id.into(),
            },
        })
    }
    fn saved_context(&self) -> Result<Option<ContextState>, String> {
        self.session
            .get("contextState")
            .filter(|v| !v.is_null())
            .map(|v| {
                serde_json::from_value(v.clone())
                    .map_err(|_| "The saved context is invalid.".into())
            })
            .transpose()
    }
    pub async fn context_report(
        &self,
        store: &LocalStore,
        directory: &Path,
        settings: &Settings,
    ) -> Result<(), String> {
        let messages = context(store, directory, settings, &self.session)?;
        let (instructions, source): (Vec<_>, Vec<_>) =
            messages.into_iter().partition(|m| m["role"] == "system");
        let state = ContextState::restore(&source, self.saved_context()?.as_ref())
            .map_err(|e| e.to_string())?;
        let mut messages = instructions;
        let workspace = execution_workspace(&self.workspace, directory, settings)?;
        let guidance = blackwall_core::instructions::Instructions::discover(
            &workspace,
            Some(directory),
            &settings.instructions,
        );
        if !guidance.prompt().is_empty() {
            messages.insert(0, json!({"role":"system","content":guidance.prompt()}));
        }
        messages.insert(0, blackwall_core::agent::default_instructions(&workspace));
        messages.extend(state.effective());
        let mut definitions = blackwall_core::tools::definitions(settings.web_enabled, true);
        if self.memory_tools(directory).is_some()
            && store
                .memory_enabled(MemoryClient::Cli)
                .map_err(|e| e.to_string())?
        {
            definitions.extend(blackwall_core::memory::definitions());
        }
        if self.memory_tools(directory).is_some()
            && store
                .recall_enabled(MemoryClient::Cli)
                .map_err(|e| e.to_string())?
        {
            definitions.push(blackwall_core::memory::recall_definition());
        }
        let skills = SkillStore::open(&directory.join("skills"))
            .and_then(|skills| skills.list())
            .map_err(|error| error.to_string())?;
        if skills.0.iter().any(|skill| skill.enabled) {
            definitions.push(blackwall_core::skills::definition());
        }
        let report = settings
            .budget
            .report(&messages, &definitions, state.server_usage);
        println!("{}", server_status(settings, false).await);
        println!("Estimated prompt: {} tokens; tools: {}; output reserve: {}; safety reserve: {}; configured context: {}.\nServer-reported last request: {}.\nCheckpoints: {}. Estimates use UTF-8 text bytes / 3 plus framing and 4,096 tokens per image; server counts are separate.", report.estimated_prompt_tokens, report.tool_tokens, report.output_tokens, report.safety_tokens, report.context_window, report.server_usage.map_or_else(|| "unavailable".into(), |u| format!("{} prompt, {} generated", u.prompt_tokens, u.completion_tokens)), state.checkpoints.len());
        Ok(())
    }
    pub async fn compact(
        &mut self,
        store: &LocalStore,
        directory: &Path,
        settings: &Settings,
        input: &mut Input,
    ) -> Result<(), String> {
        self.flush(store)?;
        let budget = served_budget(&settings.endpoint, &settings.model, settings.budget).await;
        let backend = HttpModel::new(
            &settings.endpoint,
            &settings.model,
            environment_key(&settings.endpoint),
        )
        .and_then(|m| m.with_budget(budget))
        .map(|m| m.with_text_tool_calls(settings.text_tool_calls))
        .map_err(|e| e.to_string())?;
        let messages = context(store, directory, settings, &self.session)?;
        let (instructions, source): (Vec<_>, Vec<_>) =
            messages.into_iter().partition(|m| m["role"] == "system");
        let state = ContextState::restore(&source, self.saved_context()?.as_ref())
            .map_err(|e| e.to_string())?;
        let memory = self.memory_tools(directory);
        let session = self.id().to_owned();
        let Executed {
            snapshot, result, ..
        } = execute(
            &backend,
            execution_workspace(&self.workspace, directory, settings)?,
            settings.web_enabled,
            ContextRun {
                source,
                state,
                budget,
                instructions,
                compact: true,
                trace: None,
                subagents: settings.subagents.clone(),
                instruction_settings: settings.instructions.clone(),
            },
            settings.trace,
            Some(input),
            memory.as_ref(),
            directory,
            &session,
            None,
        )
        .await;
        result.map_err(|failure| failure.message)?;
        if let Some(snapshot) = snapshot {
            let mut candidate = self.session.clone();
            candidate["contextState"] = json!(snapshot);
            candidate["updatedAt"] = json!(now()?);
            store.save_session(&candidate).map_err(|e| e.to_string())?;
            self.session = candidate;
        }
        println!("Context compacted. Original transcript retained.");
        Ok(())
    }

    pub async fn turn(
        &mut self,
        store: &LocalStore,
        directory: &Path,
        settings: &Settings,
        prompt: &str,
        input: Option<&mut Input>,
    ) -> Result<(), String> {
        self.run(store, directory, settings, prompt, input, None)
            .await
            .map(|_| ())
            .map_err(|failure| failure.message)
    }

    /// Run one turn and say why it ended: `Complete`, or `Denied` when it completed after
    /// denying an approval. `limit` bounds the whole run.
    pub async fn run(
        &mut self,
        store: &LocalStore,
        directory: &Path,
        settings: &Settings,
        prompt: &str,
        input: Option<&mut Input>,
        limit: Option<Duration>,
    ) -> Result<RunReason, Failure> {
        // Without input, the run is headless: JSON lines, and prompts are denied.
        let json = input.is_none();
        self.flush(store)?;
        let budget = served_budget(&settings.endpoint, &settings.model, settings.budget).await;
        let backend = HttpModel::new(
            &settings.endpoint,
            &settings.model,
            environment_key(&settings.endpoint),
        )
        .and_then(|backend| backend.with_budget(budget))
        .map(|backend| backend.with_text_tool_calls(settings.text_tool_calls))
        .map_err(|error| error.to_string())?;
        let workspace = execution_workspace(&self.workspace, directory, settings)?;
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
        let memory = self.memory_tools(directory);
        let saved = self.saved_context()?;
        let (instructions, source): (Vec<_>, Vec<_>) =
            messages.into_iter().partition(|m| m["role"] == "system");
        let state = ContextState::restore(&source, saved.as_ref()).map_err(|e| e.to_string())?;
        let session = self.id().to_owned();
        let Executed {
            answer,
            snapshot,
            result,
            denied,
            usage,
        } = execute(
            &backend,
            workspace,
            settings.web_enabled,
            ContextRun {
                source,
                state,
                budget,
                instructions,
                compact: false,
                trace: None,
                subagents: settings.subagents.clone(),
                instruction_settings: settings.instructions.clone(),
            },
            settings.trace,
            input,
            memory.as_ref(),
            directory,
            &session,
            limit,
        )
        .await;
        if let Some(snapshot) = snapshot {
            self.session["contextState"] = json!(snapshot);
        }
        let status = if result.is_ok() { "complete" } else { "error" };
        let content = if answer.is_empty() {
            result
                .as_ref()
                .err()
                .map(|failure| failure.message.clone())
                .unwrap_or_default()
        } else {
            answer.clone()
        };
        self.session["messages"].as_array_mut().ok_or("The conversation is invalid.")?.push(
            json!({"id":format!("assistant_{timestamp}_{}",SEQUENCE.fetch_add(1,Ordering::Relaxed)),"role":"assistant","content":content,"attachments":[],"status":status,"createdAt":timestamp})
        );
        self.session["updatedAt"] = json!(now()?);
        self.dirty = true;
        self.flush(store)?;
        let reason = match &result {
            Err(failure) => failure.reason,
            Ok(()) if denied > 0 => RunReason::Denied,
            Ok(()) => RunReason::Complete,
        };
        if json {
            let line = result_line(
                Some(self.id()),
                &answer,
                result.as_ref().err(),
                reason,
                denied,
                usage,
            );
            println!("{line}");
        } else {
            eprintln!("Saved conversation {}", self.id());
        }
        result.map(|()| reason)
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
    if !request.messages.is_empty() {
        request.validate().map_err(|error| error.to_string())?;
    }
    let mut messages = model::messages(&request);
    let memory = store
        .memory_context(MemoryClient::Cli, session["workspace"].as_str(), 4000)
        .map_err(|error| error.to_string())?;
    if !memory.is_empty() {
        messages.insert(0, json!({"role":"system","content":memory}));
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

pub(crate) fn environment_key(endpoint: &str) -> Option<String> {
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

/// The process's telemetry, started on the first run and flushed when bw exits.
static TELEMETRY: std::sync::OnceLock<blackwall_core::telemetry::Telemetry> =
    std::sync::OnceLock::new();
/// The saved export and retention settings, applied when a run starts.
static TELEMETRY_SETTINGS: std::sync::Mutex<
    Option<(
        blackwall_core::telemetry::ExportConfig,
        blackwall_core::telemetry::Retention,
    )>,
> = std::sync::Mutex::new(None);

/// Apply changed export settings, reading every header value again, such as after a header was
/// saved or removed.
pub fn reload_telemetry(settings: &crate::settings::Settings) {
    configure_telemetry(settings);
    if let Some(telemetry) = TELEMETRY.get() {
        telemetry.reload_export(
            &settings.export,
            &blackwall_core::telemetry::export::keychain_header,
        );
    }
}

/// Use these export and retention settings for runs from now on.
pub fn configure_telemetry(settings: &crate::settings::Settings) {
    if let Ok(mut saved) = TELEMETRY_SETTINGS.lock() {
        *saved = Some((settings.export.clone(), settings.retention));
    }
}

fn run_scope(
    directory: &Path,
    trace: blackwall_core::trace::TraceOptions,
    request_id: &str,
    compact: bool,
    labels: (&Path, &str),
) -> blackwall_core::telemetry::RunScope {
    use blackwall_core::telemetry::{Mode, Surface, Telemetry, TelemetryOptions};
    let (export, retention) = TELEMETRY_SETTINGS
        .lock()
        .ok()
        .and_then(|saved| saved.clone())
        .unwrap_or_default();
    let telemetry = TELEMETRY.get_or_init(|| {
        Telemetry::start(
            directory,
            TelemetryOptions {
                retention,
                ..TelemetryOptions::from_trace(trace, Surface::Cli)
            },
        )
    });
    telemetry.set_options(trace);
    // Reads header values from the credential store only when the settings change.
    telemetry.set_export(&export);
    // A compaction request is not a run: it only meters its summary calls.
    let scope = if compact {
        telemetry.compaction(Mode::Agent)
    } else {
        telemetry.run(request_id, Mode::Agent)
    };
    scope.labelled(Some(labels.0), Some(labels.1))
}

/// Write any buffered metrics before bw exits.
pub fn shutdown_telemetry() {
    if let Some(telemetry) = TELEMETRY.get() {
        telemetry.clone().shutdown();
    }
}

/// What one agent run produced.
struct Executed {
    answer: String,
    snapshot: Option<ContextState>,
    result: Result<(), Failure>,
    /// Actions denied during the run, by a rule, the user, or because the run is headless.
    denied: u32,
    /// Tokens the server reported for the run's model calls, including any before a failure.
    usage: TokenUsage,
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    backend: &HttpModel,
    workspace: Workspace,
    web_enabled: bool,
    mut run: ContextRun,
    trace: blackwall_core::trace::TraceOptions,
    mut input: Option<&mut Input>,
    memory: Option<&MemoryTools>,
    directory: &Path,
    session: &str,
    limit: Option<Duration>,
) -> Executed {
    let json = input.is_none();
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
    let project = agent.workspace.path.clone();
    run.trace = Some(run_scope(
        directory,
        trace,
        &request_id,
        run.compact,
        (&project, session),
    ));
    let work = agent.run_context_with_guidance(&request_id, run, memory, Some(directory));
    tokio::pin!(work);
    let mut answer = String::new();
    let mut snapshot = None;
    let mut pending: Option<ResolveApprovalRequest> = None;
    let mut denied = 0_u32;
    let mut usage = TokenUsage::default();
    let run = async {
        loop {
            tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => break Err(Failure::new(RunReason::Interrupted, "Task stopped. You can continue this conversation.")),
                line = async {
                    match input.as_deref_mut() {
                        Some(input) => input.read().await,
                        None => Ok(None),
                    }
                }, if pending.is_some() => {
                    match line {
                        Ok(line) => {
                            if let Some(mut request) = pending.take() {
                                request.decision = if line.as_deref().is_some_and(|line| line.trim() == "yes") { ApprovalDecision::Allow } else { ApprovalDecision::Deny };
                                if let Err(error) = approvals.resolve(request) { eprintln!("bw: {error}"); }
                            }
                        }
                        Err(error) => break Err(Failure::from(error)),
                    }
                }
                event = incoming.recv() => if let Some(event) = event {
                    count_usage(&event, &request_id, &mut usage);
                    count_denied(&event, &mut denied);
                    render_event(event, &mut answer, &mut pending, &mut snapshot, json);
                    // Nobody can answer a prompt in a headless run, so it is denied at once.
                    if let Some(request) = pending.take_if(|_| json) {
                        if let Err(error) = approvals.resolve(request) { eprintln!("bw: {error}"); }
                    }
                },
                result = &mut work => match result {
                    Ok(complete) => break Ok(complete),
                    Err(error) => break Err(Failure::new(error.reason(), error.to_string())),
                },
            }
        }
    };
    let result = match limit {
        Some(limit) => tokio::time::timeout(limit, run).await.unwrap_or_else(|_| {
            Err(Failure::new(
                RunReason::Timeout,
                format!(
                    "The run reached its {}-second time limit and was stopped. You can continue this conversation.",
                    limit.as_secs()
                ),
            ))
        }),
        None => run.await,
    };
    // A ready model future can enqueue events and complete in one poll. Render those
    // events before returning so fast completions retain text and instruction provenance.
    while let Ok(event) = incoming.try_recv() {
        count_usage(&event, &request_id, &mut usage);
        count_denied(&event, &mut denied);
        render_event(event, &mut answer, &mut pending, &mut snapshot, json);
    }
    let _ = io::stdout().flush();
    let result = result.map(|complete| {
        answer = complete;
    });
    if !json {
        println!();
    }
    Executed {
        answer,
        snapshot,
        result,
        denied,
        usage,
    }
}

/// Count actions that did not run because a rule, the user or a headless run denied them.
/// Each denial, prompted or not, ends in one denied tool result.
fn count_denied(event: &AgentEvent, denied: &mut u32) {
    if matches!(event, AgentEvent::ToolResult { denied: true, .. }) {
        *denied = denied.saturating_add(1);
    }
}

/// Sum each model call's usage as it arrives, so a run that fails or stops still reports the
/// tokens it spent. `TurnComplete.usage` repeats the same total and is not counted again.
fn count_usage(event: &AgentEvent, request_id: &str, usage: &mut TokenUsage) {
    if let AgentEvent::ModelUsage {
        request_id: id,
        usage: count,
    } = event
    {
        if id == request_id {
            usage.prompt_tokens = usage.prompt_tokens.saturating_add(count.prompt_tokens);
            usage.completion_tokens = usage
                .completion_tokens
                .saturating_add(count.completion_tokens);
            usage.total_tokens = usage.total_tokens.saturating_add(count.total_tokens);
        }
    }
}

/// The server kind found at the configured endpoint and the context window it reports.
/// `detect` asks afresh, including a cloud HTTPS service that requests never probe.
pub async fn server_status(settings: &Settings, detect: bool) -> String {
    let (kind, window) = if detect {
        blackwall_core::connection::detect_endpoint(&settings.endpoint, &settings.model).await
    } else {
        blackwall_core::connection::probe_endpoint(&settings.endpoint, &settings.model).await
    };
    format!(
        "server: {}\ndetected context: {}",
        kind.name(),
        window.map_or_else(
            || "not reported (using the configured limit)".into(),
            |window| format!("{} tokens", window.min(1_000_000))
        )
    )
}

/// "Thought for 3s", rounded to whole seconds and never below one.
fn thought_for(ms: u64) -> String {
    format!("Thought for {}s", (ms.saturating_add(500) / 1000).max(1))
}

fn render_event(
    event: AgentEvent,
    answer: &mut String,
    pending: &mut Option<ResolveApprovalRequest>,
    snapshot: &mut Option<ContextState>,
    json: bool,
) {
    if json {
        // Context snapshots are internal state; every other event is one JSON line.
        if let AgentEvent::ContextUpdated { state, .. } = event {
            *snapshot = Some(state);
            return;
        }
        let (line, details) = json_line(&event);
        for detail in details {
            eprintln!("{detail}");
        }
        println!("{line}");
        match event {
            AgentEvent::AssistantDelta { delta, .. } => answer.push_str(&delta),
            AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } => {
                *pending = Some(ResolveApprovalRequest {
                    request_id,
                    approval_id,
                    decision: ApprovalDecision::Deny,
                })
            }
            _ => {}
        }
        return;
    }
    match event {
        AgentEvent::ContextUpdated { state, .. } => *snapshot = Some(state),
        AgentEvent::AssistantDelta { delta, .. } => {
            print!("{}", safe(&delta));
            answer.push_str(&delta);
            let _ = io::stdout().flush();
        }
        AgentEvent::InstructionsLoaded {
            sources,
            warnings,
            diagnostics,
            agent_id,
            ..
        } => {
            eprintln!(
                "\n{}Instructions: {}",
                agent_id
                    .map(|id| format!("[agent {id}] "))
                    .unwrap_or_default(),
                if sources.is_empty() {
                    "none".into()
                } else {
                    safe(&sources.join(", "))
                }
            );
            for warning in warnings {
                eprintln!("Instruction warning: {}", safe(&warning));
            }
            // For the owner at this terminal only; the model never sees them.
            for line in detail_lines(&diagnostics) {
                eprintln!("{line}");
            }
        }
        AgentEvent::ToolCall { name, .. } => eprintln!("\n[{}]", safe(&name)),
        AgentEvent::ToolResult {
            success: false,
            output,
            ..
        } => eprintln!("{}", safe(&output)),
        AgentEvent::ApprovalRequest {
            request_id,
            approval_id,
            detail,
            outside_sandbox,
            ..
        } => {
            eprintln!("{}", approval_prompt(&detail, outside_sandbox));
            *pending = Some(ResolveApprovalRequest {
                request_id,
                approval_id,
                decision: ApprovalDecision::Deny,
            });
        }
        AgentEvent::SubagentStatus { state, summary, .. } => {
            eprintln!("\n[agent {state:?}] {}", safe(&summary.unwrap_or_default()))
        }
        // Reasoning stays collapsed: only how long it took is shown.
        AgentEvent::TurnComplete {
            reasoning: Some(_),
            reasoning_ms,
            ..
        } => eprintln!("\n[{}]", thought_for(reasoning_ms.unwrap_or(0))),
        _ => {}
    }
}

/// The JSON line for stdout, which may reach a model or a log, and the owner's details for the
/// terminal. Server details never go in the line.
fn json_line(event: &AgentEvent) -> (String, Vec<String>) {
    let mut event = event.clone();
    let mut details = vec![];
    if let AgentEvent::InstructionsLoaded { diagnostics, .. } = &mut event {
        details = detail_lines(diagnostics);
        diagnostics.clear();
    }
    (serde_json::to_string(&event).unwrap_or_default(), details)
}

/// The terminal lines for the owner, each prefixed and made safe to print.
fn detail_lines(diagnostics: &[ServerDiagnostic]) -> Vec<String> {
    diagnostics
        .iter()
        .flat_map(diagnostic_lines)
        .map(|line| format!("  details: {}", safe(&line)))
        .collect()
}

/// What to print under a server's warning: the variables it was not given, then its last output.
/// Every line starts with the server name, so output from several failed servers stays apart.
fn diagnostic_lines(diagnostic: &ServerDiagnostic) -> Vec<String> {
    let mut lines = vec![];
    if !diagnostic.not_passed.is_empty() {
        lines.push(format!(
            "{}: not passed: {}",
            diagnostic.server,
            diagnostic.not_passed.join(", ")
        ));
    }
    lines.extend(
        diagnostic
            .stderr
            .lines()
            .map(|line| format!("{}: {line}", diagnostic.server)),
    );
    lines
}

/// Model and tool text can carry terminal escapes, so it is made visible-only before printing.
/// A program that runs outside the sandbox is named so above the detail, which then does not
/// repeat it.
fn approval_prompt(detail: &str, outside_sandbox: bool) -> String {
    let detail = match outside_sandbox {
        true => format!(
            "{OUTSIDE_NOTE}\n\n{}",
            detail
                .strip_prefix(OUTSIDE_NOTE)
                .unwrap_or(detail)
                .trim_start()
        ),
        false => detail.to_owned(),
    };
    format!(
        "\n{}\n\nAllow this action once? Type yes to allow; anything else denies.",
        safe(&detail)
    )
}

fn execution_workspace(
    path: &Path,
    directory: &Path,
    settings: &Settings,
) -> Result<Workspace, String> {
    let mut workspace = Workspace::open(path).map_err(|error| error.to_string())?;
    workspace.execution = blackwall_core::sandbox::ExecutionPolicy {
        profile: settings.execution_profile,
        network: settings.command_network,
        data_directory: Some(directory.into()),
        command_tools: settings.command_tools_isolation,
    };
    Ok(workspace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outside_sandbox_prompts_say_so_above_the_detail() {
        let detail = format!(
            "{OUTSIDE_NOTE}\n\nProject: /p\nTool: review\nProgram: /usr/bin/claude\nGets these variables from your environment: GITHUB_TOKEN"
        );
        let text = approval_prompt(&detail, true);
        assert!(
            text.starts_with(&format!("\n{OUTSIDE_NOTE}\n\nProject: /p")),
            "{text}"
        );
        assert!(
            text.contains("\nProgram: /usr/bin/claude\nGets these variables from your environment: GITHUB_TOKEN\n"),
            "{text}"
        );
        assert_eq!(text.matches(OUTSIDE_NOTE).count(), 1, "{text}");
        // Only the event's mark adds it; text in the detail alone is printed as it is.
        let plain = approval_prompt("Program: /usr/bin/claude", false);
        assert!(!plain.contains(OUTSIDE_NOTE), "{plain}");
    }

    #[test]
    fn approval_prompts_cannot_carry_terminal_escapes() {
        let text = approval_prompt(
            "Delegate: \u{1b}[2J\u{1b}]8;;https://x.test\u{7}fix\u{202E}gnp.exe\r",
            false,
        );
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{7}') && !text.contains('\r'),
            "{text}"
        );
        assert!(text.contains("\\u{202E}gnp.exe"), "{text}");
        assert!(
            text.ends_with("Type yes to allow; anything else denies."),
            "{text}"
        );
    }

    #[test]
    fn server_details_list_unpassed_variables_then_stderr_lines() {
        let diagnostic = ServerDiagnostic {
            server: "docs".into(),
            stderr: "Traceback\nno module named x".into(),
            not_passed: vec!["TOKEN".into(), "HOME_KEY".into()],
        };
        assert_eq!(
            diagnostic_lines(&diagnostic),
            [
                "docs: not passed: TOKEN, HOME_KEY",
                "docs: Traceback",
                "docs: no module named x"
            ]
        );
        let quiet = ServerDiagnostic {
            not_passed: vec![],
            stderr: String::new(),
            ..diagnostic
        };
        assert!(diagnostic_lines(&quiet).is_empty());
    }

    #[test]
    fn json_events_carry_no_server_details_while_the_terminal_gets_them() {
        let event = AgentEvent::InstructionsLoaded {
            request_id: "r".into(),
            agent_id: None,
            sources: vec![],
            warnings: vec!["MCP server docs did not start: stopped.".into()],
            diagnostics: vec![ServerDiagnostic {
                server: "docs".into(),
                stderr: "no module \u{1b}[2Jx".into(),
                not_passed: vec!["TOKEN".into()],
            }],
        };
        let (line, details) = json_line(&event);
        assert!(line.contains("did not start"), "{line}");
        for secret in ["diagnostics", "no module", "TOKEN"] {
            assert!(!line.contains(secret), "{line}");
        }
        assert_eq!(
            details,
            [
                "  details: docs: not passed: TOKEN",
                "  details: docs: no module [2Jx"
            ]
        );
        assert!(details.iter().all(|line| !line.contains('\u{1b}')));
    }

    #[test]
    fn reasoning_is_shown_only_as_its_duration() {
        assert_eq!(thought_for(0), "Thought for 1s");
        assert_eq!(thought_for(2_600), "Thought for 3s");
    }

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
