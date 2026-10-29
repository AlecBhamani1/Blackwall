mod input;
mod mcp;
mod memory;
mod recall;
mod review;
mod session;
mod settings;
mod skills;
mod traces;
mod usage;

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
  /settings <name> <value>  Change connection, tools, memory, or budget
  /model <model-id>         Select a model
  /endpoint <url>           Set the OpenAI-compatible service address
  /web on|off               Enable/disable approved web tools
  /memory on|off            Include/exclude your saved memories
  /memory <action>          list, pending, approve, reject, edit, forget
  /workspace <path>         Start a new conversation in a project
  /review [target]          Inspect Git changes; --uncommitted, --commit <ref>, or --base <ref>
  /init                     Review a proposed AGENTS.md (preserves existing guidance)
  /recall <words> [filters]  Search saved messages; same filters as bw sessions search
  /settings recall on|off   Enable or disable the agent recall tool (default off)
  /learn                    Propose a reusable skill from this conversation for review
  /<skill> [request]        Run a saved skill now, including one that runs only when typed
  /context                  Show the detected server, estimated budget, and last token usage
  /compact                  Summarize older context; retain the original transcript
  /settings context <n>     Configure model context limit in tokens
  /settings output <n>      Reserve generated output tokens
  /settings compact on|off  Automatically compact near the context limit
  /settings iterations <n>  Limit model/tool iterations per Agent run
  /settings profile <read-only|workspace-write|unsandboxed>  Set command access (default read-only)
  /settings command-network on|off  Command network access (default off; unsandboxed always on)
  /settings timeout <n>     Limit each shell command in seconds
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
  bw status                  Show the detected model server, its context window, and settings
  bw sessions search <words> [--project <path>] [--after <unix-ms>] [--before <unix-ms>] [--limit <n>]
  bw commands                List interactive slash commands
  bw run [--json] <prompt>   Run one task; --json prints JSON lines and denies prompts
  bw resume <id> [--json] [prompt]
                             Continue a saved Agent conversation
  bw sessions                List saved conversations
  bw memory <action>         Review or manage saved memories (bw memory for list)
  bw skills <action>         Review learned skill proposals (bw skills for pending)
  bw skills import [<path>]  Import SKILL.md skills from other harnesses (list without a path)
  bw skills suggested        List suggested skills; bw skills get <name> downloads, scans, and imports one
  bw traces [show <id>]      List recent agent-run traces or show one as a tree
  bw usage [--days N] [--utc] [--json]
                             Show tokens, runs, and tools from local usage metrics
                             for the last N local days (1-400, default 7)
  bw mcp <action>            Add, list, enable, disable, or remove MCP servers
  bw review [--json] [--uncommitted | --commit <ref> | --base <ref>]
                             Review Git changes without agent tools
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
    session::shutdown_telemetry();
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
        "chat" | "setup" | "config" | "status" | "run" | "resume" | "sessions" | "request"
        | "memory" | "mcp" | "review" | "skills" | "traces" | "usage" => {}
        _ => return Err("Unknown command. Run bw --help.".into()),
    }
    let directory = match env::var_os("BLACKWALL_DATA_DIR") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => env::home_dir()
            .ok_or("Your home folder could not be found.")?
            .join(".blackwall"),
    };
    if command == "memory" {
        return memory::run(&directory, &arguments);
    }
    if command == "skills" {
        return skills::run(&directory, &arguments).await;
    }
    if command == "traces" {
        return traces::run(&directory, &arguments);
    }
    let store = LocalStore::open(&directory).map_err(|error| error.to_string())?;
    if command == "mcp" {
        return mcp::run(&directory, &arguments);
    }
    if command == "sessions" {
        if arguments.first().map(String::as_str) == Some("search") {
            return recall::search(&store, &arguments[1..]);
        }
        require_no_arguments(&arguments)?;
        return list_sessions(&store);
    }
    let mut settings = Settings::load(&store)?;
    if command == "usage" {
        return usage::run(&directory, &arguments, settings.trace.enabled);
    }
    if command == "config" {
        return configure(&store, &mut settings, &arguments);
    }
    if command == "status" {
        require_no_arguments(&arguments)?;
        println!(
            "{}\n{}",
            session::server_status(&settings).await,
            settings.describe()
        );
        return Ok(());
    }
    if command == "request" {
        let prompt = required_prompt(&arguments)?;
        let mut request = ChatRequest::new(
            "cli-preview",
            settings.model,
            vec![ChatMessage::new(MessageRole::User, prompt)],
        );
        request.context_budget = settings.budget;
        request.max_tokens = Some(settings.budget.output_tokens);
        request.endpoint = Some(settings.endpoint);
        println!(
            "{}",
            serde_json::to_string_pretty(&request).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    if command == "setup" {
        require_no_arguments(&arguments)?;
        return setup(&store, &mut settings, &mut Input::stdin()).await;
    }
    let workspace =
        Workspace::open(&env::current_dir().map_err(|_| "The current folder is unavailable.")?)
            .map_err(|error| error.to_string())?
            .path;
    if command == "review" {
        return review::run(&workspace, &settings, &arguments).await;
    }
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
        let (json, prompt_arguments) = match prompt_arguments.split_first() {
            Some((flag, rest)) if flag == "--json" => (true, rest),
            _ => (false, prompt_arguments),
        };
        let prompt = required_prompt(prompt_arguments)?;
        // A headless run never reads standard input, so it cannot take a script's later input.
        let mut input = (!json).then(Input::stdin);
        return conversation
            .turn(&store, &directory, &settings, &prompt, input.as_mut())
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
        &mut Input::stdin(),
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
                "Use bw config show or bw config set <model|endpoint|profile|command-network|web|memory|recall|context|output|compact|iterations|timeout> <value>.".into(),
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
        "Blackwall — {}\nProject: {}\nCommands: {}\nType /commands for settings and controls.",
        settings.model,
        conversation.workspace.display(),
        blackwall_core::sandbox::ExecutionPolicy {
            profile: settings.execution_profile,
            network: settings.command_network,
            data_directory: None
        }
        .description()
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
                match name {
                    "review" => {
                        let arguments = value
                            .split_whitespace()
                            .map(str::to_owned)
                            .collect::<Vec<_>>();
                        review::run(&conversation.workspace, settings, &arguments).await
                    }
                    "init" | "learn" if value.is_empty() => {
                        let command = format!("/{name}");
                        conversation
                            .turn(store, directory, settings, &command, Some(input))
                            .await
                    }
                    "compact" if value.is_empty() => {
                        conversation
                            .compact(store, directory, settings, input)
                            .await
                    }
                    "context" if value.is_empty() => {
                        conversation
                            .context_report(store, directory, settings)
                            .await
                    }
                    // A saved skill's name runs it; the agent loads the skill for this turn.
                    _ if saved_skill(directory, name) => {
                        conversation
                            .turn(store, directory, settings, line, Some(input))
                            .await
                    }
                    _ => slash(store, directory, settings, conversation, name, value),
                }
            } else {
                let prompt = if line.starts_with("//") {
                    &line[1..]
                } else {
                    line
                };
                conversation
                    .turn(store, directory, settings, prompt, Some(input))
                    .await
            };
        if let Err(error) = result {
            eprintln!("bw: {error}");
        }
    }
}

fn saved_skill(directory: &std::path::Path, name: &str) -> bool {
    !blackwall_core::skills::COMMANDS.contains(&name)
        && blackwall_core::skills::SkillStore::open(&directory.join("skills"))
            .is_ok_and(|skills| skills.contains(name))
}

fn slash(
    store: &LocalStore,
    directory: &std::path::Path,
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
            let (key, value) = value.split_once(char::is_whitespace).ok_or(
                "Use /settings <model|endpoint|profile|command-network|web|memory|recall|context|output|compact|iterations|timeout> <value>.",
            )?;
            settings.set(store, key, value.trim())?;
            println!("{}", settings.describe());
        }
        "memory" if !matches!(value, "on" | "off") => memory::run(
            directory,
            &value
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        )?,
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
        "recall" => {
            let search_store = LocalStore::open(directory).map_err(|e| e.to_string())?;
            recall::search(&search_store, &recall::line(value)?)?;
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
