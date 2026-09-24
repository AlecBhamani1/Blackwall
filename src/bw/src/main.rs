use blackwall_core::{
    agent::Agent,
    approvals::Approvals,
    model::HttpModel,
    protocol::{AgentEvent, ApprovalDecision, ResolveApprovalRequest},
    storage::LocalStore,
    tools::Workspace,
    ChatMessage, ChatRequest, MessageRole,
};
use serde_json::{json, Value};
use std::{
    env,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("bw: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run());
    // A cancelled terminal approval may leave a stdin read waiting. It must not hold shutdown open.
    runtime.shutdown_background();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bw: {error}");
            ExitCode::FAILURE
        }
    }
}
async fn run() -> Result<(), String> {
    let mut arguments = env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| "help".into());
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        println!("bw — Blackwall project assistant\n\nUSAGE:\n  bw run <prompt>\n  bw resume <session-id> <prompt>\n  bw sessions\n  bw request <prompt>\n\nRuns in the current project folder. File reads stay inside it; writes and shell commands require approval.\nWeb access is disabled in the CLI. Ctrl+C stops the whole run.\n\nENVIRONMENT:\n  BLACKWALL_MODEL          model id (default llama3.2)\n  BLACKWALL_MODEL_ENDPOINT model service address (or OLLAMA_HOST)\n  BLACKWALL_MODEL_API_KEY  optional access key for that service");
        return Ok(());
    }
    let model = env::var("BLACKWALL_MODEL").unwrap_or_else(|_| "llama3.2".into());
    if command == "request" {
        let prompt = arguments.collect::<Vec<_>>().join(" ");
        if prompt.trim().is_empty() {
            return Err("A prompt is required.".into());
        }
        let request = ChatRequest::new(
            "cli-preview",
            model,
            vec![ChatMessage::new(MessageRole::User, prompt)],
        );
        println!(
            "{}",
            serde_json::to_string_pretty(&request).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    if !matches!(command.as_str(), "run" | "resume" | "sessions") {
        return Err("Unknown command. Run bw --help.".into());
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("Your home folder could not be found.")?;
    let store = LocalStore::open(&home.join(".blackwall")).map_err(|e| e.to_string())?;
    if command == "sessions" {
        for session in store.list_sessions().map_err(|e| e.to_string())? {
            println!(
                "{}  {}",
                session["id"].as_str().unwrap_or(""),
                session["title"].as_str().unwrap_or("")
            );
        }
        return Ok(());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "The system clock is invalid.")?
        .as_millis() as u64;
    let mut session = if command == "resume" {
        let id = arguments
            .next()
            .ok_or("A session identifier is required.")?;
        store
            .load_session(&id)
            .map_err(|e| e.to_string())?
            .ok_or("That conversation was not found.")?
    } else {
        json!({"id":format!("cli_{}_{}",std::process::id(),now),"title":"CLI conversation","updatedAt":now,"messages":[]})
    };
    let prompt = arguments.collect::<Vec<_>>().join(" ");
    if prompt.trim().is_empty() {
        return Err("A prompt is required.".into());
    }
    if command == "run" {
        session["title"] = json!(prompt.chars().take(80).collect::<String>());
    }
    let history = session["messages"]
        .as_array_mut()
        .ok_or("The saved conversation is invalid.")?;
    history.push(json!({"id":format!("message_{now}"),"role":"user","content":prompt,"attachments":[],"status":"complete","createdAt":now}));
    let mut messages = history
        .iter()
        .map(|message| json!({"role":message["role"],"content":message["content"]}))
        .collect::<Vec<_>>();
    let preferences: Value = store
        .setting("preferences")
        .map_err(|error| error.to_string())?
        .map(|text| serde_json::from_str(&text))
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_else(|| json!({}));
    if preferences["memoryEnabled"].as_bool() == Some(true) {
        let context = blackwall_core::memory::context(
            &store.memories("").map_err(|error| error.to_string())?,
            4000,
        );
        if !context.is_empty() {
            messages.insert(0, json!({"role":"system","content":context}));
        }
    }
    let skills = blackwall_core::skills::SkillStore::open(&home.join(".blackwall/skills"))
        .map_err(|error| error.to_string())?;
    skills.seed().map_err(|error| error.to_string())?;
    let (enabled, warnings) = skills.list().map_err(|error| error.to_string())?;
    for warning in warnings {
        eprintln!("Skill: {warning}");
    }
    let workflows = blackwall_core::skills::prompt(&enabled);
    if !workflows.is_empty() {
        messages.insert(0, json!({"role":"system","content":workflows}));
    }
    let endpoint = env::var("BLACKWALL_MODEL_ENDPOINT")
        .or_else(|_| env::var("OLLAMA_HOST"))
        .unwrap_or_else(|_| "http://localhost:11434/v1".into());
    let backend = HttpModel::new(&endpoint, &model, env::var("BLACKWALL_MODEL_API_KEY").ok())
        .map_err(|e| e.to_string())?;
    let approvals = Approvals::default();
    let decisions = approvals.clone();
    let (events, mut incoming) = tokio::sync::mpsc::unbounded_channel();
    let emit = Arc::new(move |event| {
        let _ = events.send(event);
    });
    let directory = env::current_dir().map_err(|_| "The current project folder is unavailable.")?;
    let agent = Agent {
        backend: &backend,
        workspace: Workspace::open(&directory).map_err(|e| e.to_string())?,
        web_enabled: false,
        approvals,
        emit,
    };
    let request_id = format!("cli_run_{now}");
    let work = agent.run(&request_id, messages);
    tokio::pin!(work);
    let answer = loop {
        tokio::select! {
            biased;
            _=tokio::signal::ctrl_c()=>return Err("Task stopped.".into()),
            event=incoming.recv()=>if let Some(event)=event{match event{
                AgentEvent::AssistantDelta{delta,..}=>{print!("{delta}");let _=io::stdout().flush();},
                AgentEvent::ToolCall{name,..}=>eprintln!("\n[{name}]"),
                AgentEvent::ToolResult{success,output,..}=>if !success{eprintln!("{output}");},
                AgentEvent::ApprovalRequest{request_id,approval_id,detail,..}=>{
                    let decisions=decisions.clone();
                    tokio::task::spawn_blocking(move||{
                        eprintln!("\n{detail}\n\nAllow this action once? Type yes to allow; anything else denies.");
                        let mut line=String::new();let _=io::stdin().read_line(&mut line);
                        let decision=if line.trim()=="yes"{ApprovalDecision::Allow}else{ApprovalDecision::Deny};
                        let _=decisions.resolve(ResolveApprovalRequest{request_id,approval_id,decision});
                    });
                },_=>{}
            }},
            result=&mut work=>break result.map_err(|e|e.to_string())?,
        }
    };
    println!();
    session["messages"].as_array_mut().ok_or("The conversation is invalid.")?.push(json!({"id":format!("assistant_{now}"),"role":"assistant","content":answer,"attachments":[],"status":"complete","createdAt":now}));
    session["updatedAt"] = Value::from(now);
    store.save_session(&session).map_err(|e| e.to_string())?;
    eprintln!(
        "Saved conversation {}",
        session["id"].as_str().unwrap_or("")
    );
    Ok(())
}
