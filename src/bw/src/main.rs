mod input;
mod session;
mod settings;

use blackwall_core::{
    storage::LocalStore, tools::Workspace, ChatMessage, ChatRequest, MessageRole,
};
use input::Input;
use session::Conversation;
use settings::Settings;
use std::{env, path::PathBuf, process::ExitCode};

const COMMANDS: &str = "SLASH COMMANDS (settings are saved for future CLI launches):
  /commands, /help           Show these commands
  /settings                 Show effective settings and workspace
  /settings <name> <value>  Change model, endpoint, web, or memory
  /model <model-id>         Select a model
  /endpoint <url>           Set the OpenAI-compatible service address
  /web on|off               Enable/disable approved web tools
  /memory on|off            Include/exclude your saved memories
  /workspace <path>         Start a new conversation in a project
  /init                     Review a proposed AGENTS.md (preserves existing guidance)
  /new                      Start a new conversation in this project
  /sessions                 List saved conversations
  /resume <session-id>      Resume an Agent conversation and its project
  /quit, /exit              Exit Blackwall

Type a message to ask the agent. Type // to start a message with a literal /.
File tools stay in the project; edits, shell commands, and web requests need approval.
At an approval, type yes to allow once; other input denies. Ctrl+C stops a turn.";

const HELP: &str = "bw — Blackwall project assistant

USAGE:
  bw                         Start interactive chat in the current project
  bw chat                    Start interactive chat
  bw setup                   Configure endpoint, model, web, and memory
  bw config [show]           Show effective settings
  bw config set <name> <value>
  bw commands                List interactive slash commands
  bw run <prompt>            Run one task
  bw resume <id> [prompt]    Continue a saved Agent conversation
  bw sessions                List saved conversations
  bw request <prompt>        Print a request preview without a model call

Install: cargo install --locked --path src/bw
Settings and sessions live in ~/.blackwall/ (BLACKWALL_DATA_DIR overrides the folder).
Saved CLI settings take precedence over desktop settings, then environment defaults.
BLACKWALL_MODEL and BLACKWALL_MODEL_ENDPOINT (or OLLAMA_HOST) supply initial defaults.
BLACKWALL_MODEL_API_KEY is used only for the environment-configured service origin.
Run bw commands for /commands and the other interactive controls.";

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("bw: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run());
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
    let command = arguments.next().unwrap_or_else(|| "chat".into());
    let arguments = arguments.collect::<Vec<_>>();
    match command.as_str() {
        "help" | "--help" | "-h" => {
            println!("{HELP}");
            return Ok(());
        }
        "commands" => {
            println!("{COMMANDS}");
            return Ok(());
        }
        "--version" | "-V" => {
            println!("bw {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        "chat" | "setup" | "config" | "run" | "resume" | "sessions" | "request" => {}
        _ => return Err("Unknown command. Run bw --help.".into()),
    }
    let directory = match env::var_os("BLACKWALL_DATA_DIR") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => env::home_dir()
            .ok_or("Your home folder could not be found.")?
            .join(".blackwall"),
    };
    let store = LocalStore::open(&directory).map_err(|error| error.to_string())?;
    if command == "sessions" {
        require_no_arguments(&arguments)?;
        return list_sessions(&store);
    }
    let mut settings = Settings::load(&store)?;
    if command == "config" {
        return configure(&store, &mut settings, &arguments);
    }
    if command == "request" {
        let prompt = required_prompt(&arguments)?;
        let mut request = ChatRequest::new(
            "cli-preview",
            settings.model,
            vec![ChatMessage::new(MessageRole::User, prompt)],
        );
        request.endpoint = Some(settings.endpoint);
        println!(
            "{}",
            serde_json::to_string_pretty(&request).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    let mut input = Input::stdin();
    if command == "setup" {
        require_no_arguments(&arguments)?;
        return setup(&store, &mut settings, &mut input).await;
    }
    let workspace =
        Workspace::open(&env::current_dir().map_err(|_| "The current folder is unavailable.")?)
            .map_err(|error| error.to_string())?
            .path;
    let mut conversation = if command == "resume" {
        Conversation::load(
            &store,
            arguments
                .first()
                .ok_or("A session identifier is required.")?,
            &workspace,
        )?
    } else {
        Conversation::new(workspace)?
    };
    if command == "run" || (command == "resume" && arguments.len() > 1) {
        let prompt_arguments = if command == "resume" {
            &arguments[1..]
        } else {
            &arguments[..]
        };
        let prompt = required_prompt(prompt_arguments)?;
        return conversation
            .turn(&store, &directory, &settings, &prompt, &mut input)
            .await;
    }
    if command == "chat" {
        require_no_arguments(&arguments)?;
    }
    chat(
        &store,
        &directory,
        &mut settings,
        &mut conversation,
        &mut input,
    )
    .await
}

fn require_no_arguments(arguments: &[String]) -> Result<(), String> {
    if arguments.is_empty() {
        Ok(())
    } else {
        Err("Unexpected arguments. Run bw --help.".into())
    }
}

fn required_prompt(arguments: &[String]) -> Result<String, String> {
    let prompt = arguments.join(" ");
    if prompt.trim().is_empty() {
        Err("A prompt is required.".into())
    } else {
        Ok(prompt)
    }
}

fn configure(
    store: &LocalStore,
    settings: &mut Settings,
    arguments: &[String],
) -> Result<(), String> {
    match arguments {
        [] => {}
        [show] if show == "show" => {}
        [set, key, values @ ..] if set == "set" && !values.is_empty() => {
            settings.set(store, key, &values.join(" "))?;
        }
        _ => {
            return Err(
                "Use bw config show or bw config set <model|endpoint|web|memory> <value>.".into(),
            )
        }
    }
    println!("{}", settings.describe());
    Ok(())
}

async fn setup(
    store: &LocalStore,
    settings: &mut Settings,
    input: &mut Input,
) -> Result<(), String> {
    println!(
        "Blackwall setup. Enter keeps the displayed value. Use an already-running model service."
    );
    let mut candidate = settings.clone();
    for key in ["endpoint", "model", "web", "memory"] {
        let current = match key {
            "endpoint" => candidate.endpoint.clone(),
            "model" => candidate.model.clone(),
            "web" => if candidate.web_enabled { "on" } else { "off" }.into(),
            _ => if candidate.memory_enabled {
                "on"
            } else {
                "off"
            }
            .into(),
        };
        input.prompt(&format!("{key} [{current}]: "))?;
        let value = input
            .next()
            .await?
            .ok_or("Setup cancelled. No settings were changed.")?;
        let value = if value.trim().is_empty() {
            current
        } else {
            value.trim().into()
        };
        match key {
            "endpoint" => candidate.endpoint = value,
            "model" => candidate.model = value,
            "web" => candidate.web_enabled = parse_switch(&value)?,
            _ => candidate.memory_enabled = parse_switch(&value)?,
        }
    }
    candidate.validate()?;
    candidate.save(store)?;
    *settings = candidate;
    println!(
        "Saved CLI settings. Use bw to chat or bw config to review them.\n{}",
        settings.describe()
    );
    Ok(())
}

fn parse_switch(value: &str) -> Result<bool, String> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("Choose on or off. Setup cancelled; no settings were changed.".into()),
    }
}

fn list_sessions(store: &LocalStore) -> Result<(), String> {
    let sessions = store.list_sessions().map_err(|error| error.to_string())?;
    if sessions.is_empty() {
        println!("No saved conversations yet.");
    }
    for session in sessions {
        println!(
            "{}  {}  {}",
            session["id"].as_str().unwrap_or(""),
            session["title"].as_str().unwrap_or(""),
            session["workspace"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

async fn chat(
    store: &LocalStore,
    directory: &std::path::Path,
    settings: &mut Settings,
    conversation: &mut Conversation,
    input: &mut Input,
) -> Result<(), String> {
    println!(
        "Blackwall — {}\nProject: {}\nType /commands for settings and controls.",
        settings.model,
        conversation.workspace.display()
    );
    loop {
        input.prompt("you> ")?;
        let Some(line) = input.next().await? else {
            return conversation.flush(store);
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let result =
            if let Some(command) = line.strip_prefix('/').filter(|_| !line.starts_with("//")) {
                let (name, value) = command
                    .split_once(char::is_whitespace)
                    .unwrap_or((command, ""));
                let value = value.trim();
                if matches!(name, "quit" | "exit") && value.is_empty() {
                    match conversation.flush(store) {
                        Ok(()) => return Ok(()),
                        Err(error) => {
                            eprintln!("bw: {error}");
                            continue;
                        }
                    }
                }
                if name == "init" && value.is_empty() {
                    conversation
                        .turn(store, directory, settings, "/init", input)
                        .await
                } else {
                    slash(store, settings, conversation, name, value)
                }
            } else {
                let prompt = if line.starts_with("//") {
                    &line[1..]
                } else {
                    line
                };
                conversation
                    .turn(store, directory, settings, prompt, input)
                    .await
            };
        if let Err(error) = result {
            eprintln!("bw: {error}");
        }
    }
}

fn slash(
    store: &LocalStore,
    settings: &mut Settings,
    conversation: &mut Conversation,
    name: &str,
    value: &str,
) -> Result<(), String> {
    match name {
        "commands" | "help" if value.is_empty() => println!("{COMMANDS}"),
        "settings" if value.is_empty() => println!(
            "{}\nworkspace: {}",
            settings.describe(),
            conversation.workspace.display()
        ),
        "settings" => {
            let (key, value) = value
                .split_once(char::is_whitespace)
                .ok_or("Use /settings <model|endpoint|web|memory> <value>.")?;
            settings.set(store, key, value.trim())?;
            println!("{}", settings.describe());
        }
        "model" | "endpoint" | "web" | "memory" => {
            settings.set(store, name, value)?;
            println!("{}", settings.describe());
        }
        "new" if value.is_empty() => {
            conversation.flush(store)?;
            *conversation = Conversation::new(conversation.workspace.clone())?;
            println!("Started a new conversation.");
        }
        "workspace" if !value.is_empty() => {
            conversation.flush(store)?;
            let path = PathBuf::from(value);
            let path = if path.is_absolute() {
                path
            } else {
                conversation.workspace.join(path)
            };
            let workspace = Workspace::open(&path)
                .map_err(|error| error.to_string())?
                .path;
            *conversation = Conversation::new(workspace)?;
            println!("Project: {}", conversation.workspace.display());
        }
        "sessions" if value.is_empty() => list_sessions(store)?,
        "resume" if !value.is_empty() => {
            conversation.flush(store)?;
            *conversation = Conversation::load(store, value, &conversation.workspace)?;
            println!(
                "Resumed {}\nProject: {}",
                conversation.id(),
                conversation.workspace.display()
            );
        }
        _ => return Err("Unknown command or invalid arguments. Type /commands.".into()),
    }
    Ok(())
}
