//! Owner-configured MCP servers, local processes or streamable HTTP endpoints, whose tools join
//! Agent runs behind Blackwall approvals.
use crate::{
    approvals::Approvals,
    protocol::{AgentEvent, ApprovalTool, ServerDiagnostic},
    tools::ToolError,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path, sync::Mutex as StdMutex, time::Duration};
use stderr::StderrTail;
use tokio::{io::BufReader, sync::Mutex};
use transport::{StdioTransport, Transport};

mod http;
mod stderr;
mod transport;

pub const MCP_FILE: &str = "mcp.json";
/// Credential store service for server secrets; each account is `server/NAME`.
pub const SECRET_SERVICE: &str = "com.blackwall.mcp";
const MAX_FILE_BYTES: usize = 64 * 1024;
const MAX_SERVERS: usize = 16;
const MAX_TOOLS: usize = 128;
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
/// Requests a server may send back while answering one of ours.
const MAX_SERVER_REQUESTS: usize = 32;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
/// Package runners may need to download a server immediately after it is added.
const FIRST_START_TIMEOUT: Duration = Duration::from_secs(120);
const SECRET_TIMEOUT: Duration = Duration::from_secs(30);
const PROTOCOL_VERSION: &str = "2025-06-18";
const EDIT_LOCK: &str = ".mcp.lock";
const RESERVED_PREFIX: &str =
    "MCP server names starting with p- are reserved for project servers; rename it.";

fn timeout_failure(timeout: Duration) -> String {
    format!(
        "didn't answer within {} seconds — the first start may still be downloading the package; try again.",
        timeout.as_secs()
    )
}

/// One server entry in `mcp.json`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ServerConfig {
    pub name: String,
    /// The program and arguments of a local server. Empty for a `url` server.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// A streamable HTTP endpoint, used instead of `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Headers sent to a `url` server. Each value names a saved secret as `{secret:NAME}`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_tools: Vec<String>,
    /// Names of environment variables the server receives from the platform credential store.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    /// Names of the owner's own environment variables a local server also receives. Everything
    /// else, including credentials the owner has set, stays out of its environment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    /// For a `url` server, the origin each secret was saved for. A header is sent only when its
    /// secret was saved for the URL's current origin.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secret_origins: BTreeMap<String, String>,
}

impl ServerConfig {
    /// The credential store account of one of this server's secrets.
    fn account(&self, key: &str) -> String {
        match self.secret_origins.get(key) {
            Some(origin) => format!("{}/{origin}/{key}", self.name),
            None => format!("{}/{key}", self.name),
        }
    }

    /// Secrets of a URL server saved for another origin, with that origin. They are not sent
    /// until saved again.
    pub fn secrets_saved_elsewhere(&self) -> Vec<(&str, &str)> {
        let origin = self.endpoint();
        self.secret_origins
            .iter()
            .filter(|(_, saved)| self.url.is_some() && Some(saved.as_str()) != origin.as_deref())
            .map(|(key, saved)| (key.as_str(), saved.as_str()))
            .collect()
    }

    /// What a header secret is bound to: the URL's origin, or `None` for a local command.
    fn endpoint(&self) -> Option<String> {
        self.url.as_deref().and_then(http::origin)
    }
}

fn enabled() -> bool {
    true
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct McpFile {
    servers: Vec<ServerConfig>,
}

fn parse(body: &[u8]) -> Result<Vec<ServerConfig>, &'static str> {
    if body.len() > MAX_FILE_BYTES {
        return Err("The MCP file is too large.");
    }
    serde_json::from_slice::<McpFile>(body)
        .map_err(|_| "The MCP file is not valid.")
        .and_then(|file| validate(file.servers))
}

/// Read the owner's servers for editing. A missing file has no servers.
pub fn read_config(directory: &Path) -> Result<Vec<ServerConfig>, &'static str> {
    use std::io::Read;
    let mut body = vec![];
    match std::fs::File::open(directory.join(MCP_FILE)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(_) => return Err("The MCP file could not be read."),
        Ok(file) => file
            .take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|_| "The MCP file could not be read.")?,
    };
    parse(&body)
}

/// Validate and replace the owner's servers, so a failed write leaves the old file in place.
pub fn write_config(directory: &Path, servers: Vec<ServerConfig>) -> Result<(), &'static str> {
    let failed = "The MCP file could not be saved.";
    // Approvals were given for a server at one address or command; one that moves starts asking
    // again.
    let moved: Vec<String> = read_config(directory)
        .unwrap_or_default()
        .into_iter()
        .filter(|old| {
            servers.iter().any(|new| {
                new.name == old.name
                    && (new.endpoint() != old.endpoint() || new.command != old.command)
            })
        })
        .map(|old| old.name)
        .collect();
    let mut body = serde_json::to_vec_pretty(&McpFile {
        servers: validate(servers)?,
    })
    .map_err(|_| failed)?;
    body.push(b'\n');
    if body.len() > MAX_FILE_BYTES {
        return Err("The MCP file is too large.");
    }
    let path = directory.join(MCP_FILE);
    // Like the data folder and database, the file must not be a link that a save would replace.
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("The MCP file must not be a symbolic link.");
    }
    crate::policy::replace_owner_file(directory, &path, &body).map_err(|_| failed)?;
    for name in moved {
        crate::policy::remove_server_rules(directory, &name)
            .map_err(|_| "The MCP file was saved, but approval rules for a server whose address changed could not be removed.")?;
    }
    Ok(())
}

/// Owner edits shared by the CLI and desktop. Each one validates before anything is saved.
fn edit(
    directory: &Path,
    name: &str,
    change: impl FnOnce(&mut Vec<ServerConfig>, usize) -> Result<(), String>,
) -> Result<(), String> {
    let _editing = crate::policy::lock_owner_file(directory, EDIT_LOCK)
        .map_err(|_| "The MCP file could not be saved.")?;
    let mut servers = read_config(directory)?;
    let index = servers
        .iter()
        .position(|server| server.name == name)
        .ok_or_else(|| format!("No MCP server is named {name:?}."))?;
    change(&mut servers, index)?;
    Ok(write_config(directory, servers)?)
}

fn store_error(error: crate::secrets::SecretError) -> String {
    error.explain(format!(
        "The credential store could not be updated. {}, then try again.",
        crate::secrets::RECOVER
    ))
}

/// Names are lowercased and spaces or underscores become hyphens, so `My Jira` is saved and
/// found as `my-jira`.
fn saved_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .split(|c: char| c.is_whitespace() || c == '_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Add a server under its saved name, which is returned.
pub fn add_server(directory: &Path, name: &str, command: Vec<String>) -> Result<String, String> {
    add(directory, name, command, None, BTreeMap::new())
}

/// Whether a command can be launched through the same resolver used by MCP servers. This only
/// searches for the program; it never executes it.
pub fn program_found(program: &str) -> bool {
    crate::command_tools::resolve_program(program, Path::new("")).is_ok()
}

pub fn missing_program(program: &str) -> String {
    match program {
        "npx" => "Needs Node.js (npx), which isn't on this computer.".into(),
        "uvx" => "Needs uv (uvx), which isn't on this computer.".into(),
        _ => format!("{program} was not found on this computer."),
    }
}

/// Add a streamable HTTP server. Each header is `(header name, secret name)`; the secret's value
/// is saved for this server with [`save_secret`].
pub fn add_http_server(
    directory: &Path,
    name: &str,
    url: &str,
    headers: Vec<(String, String)>,
) -> Result<String, String> {
    let mut references = BTreeMap::new();
    for (header, secret) in headers {
        if references
            .insert(
                header.trim().to_owned(),
                format!("{{secret:{}}}", secret.trim()),
            )
            .is_some()
        {
            return Err(format!("The {header} header is given twice."));
        }
    }
    add(
        directory,
        name,
        vec![],
        Some(url.trim().to_owned()),
        references,
    )
}

fn add(
    directory: &Path,
    name: &str,
    command: Vec<String>,
    url: Option<String>,
    headers: BTreeMap<String, String>,
) -> Result<String, String> {
    let name = saved_name(name);
    let name = name.as_str();
    if name.starts_with("p-") {
        return Err(RESERVED_PREFIX.into());
    }
    let _editing = crate::policy::lock_owner_file(directory, EDIT_LOCK)
        .map_err(|_| "The MCP file could not be saved.")?;
    let mut servers = read_config(directory)?;
    if servers.iter().any(|server| server.name == name) {
        return Err(format!("An MCP server named {name:?} already exists."));
    }
    servers.push(ServerConfig {
        name: name.into(),
        command,
        url,
        headers,
        enabled: true,
        allow_tools: None,
        deny_tools: vec![],
        secrets: vec![],
        secret_origins: BTreeMap::new(),
        env: vec![],
    });
    write_config(directory, servers)?;
    Ok(name.into())
}

pub fn set_enabled(directory: &Path, name: &str, enabled: bool) -> Result<(), String> {
    let name = &saved_name(name);
    edit(directory, name, |servers, index| {
        servers[index].enabled = enabled;
        Ok(())
    })
}

/// Change the command or URL of a server, which keeps its kind, secrets and other settings.
pub fn set_endpoint(
    directory: &Path,
    name: &str,
    command: Vec<String>,
    url: Option<String>,
) -> Result<(), String> {
    let name = &saved_name(name);
    edit(directory, name, |servers, index| {
        if servers[index].url.is_some() != url.is_some() {
            return Err("A server cannot change between a command and a URL.".into());
        }
        servers[index].command = command;
        servers[index].url = url.map(|url| url.trim().to_owned());
        Ok(())
    })
}

/// Commit the server removal before deleting credentials and rules, so a failed configuration
/// write cannot leave a configured server without its credentials.
pub fn remove_server(directory: &Path, name: &str) -> Result<(), String> {
    let name = &saved_name(name);
    let _editing = crate::policy::lock_owner_file(directory, EDIT_LOCK)
        .map_err(|_| "The MCP file could not be saved.")?;
    let mut servers = read_config(directory)?;
    let index = servers
        .iter()
        .position(|server| server.name == name.as_str())
        .ok_or_else(|| format!("No MCP server is named {name:?}."))?;
    let removed = servers.remove(index);
    write_config(directory, servers)?;
    // The removal is saved; a keychain prompt below must not hold up other processes' edits.
    drop(_editing);
    for key in &removed.secrets {
        crate::secrets::delete(SECRET_SERVICE, &removed.account(key)).map_err(store_error)?;
    }
    crate::policy::remove_server_rules(directory, name)
}

/// The value is saved first, so the file never names a secret that failed to save. Blocking.
pub fn save_secret(directory: &Path, name: &str, key: &str, value: &str) -> Result<(), String> {
    save_secret_with(directory, name, key, value, crate::secrets::set)
}

/// [`save_secret`] with the secret write supplied by the caller.
pub fn save_secret_with(
    directory: &Path,
    name: &str,
    key: &str,
    value: &str,
    set: crate::secrets::SetSecret,
) -> Result<(), String> {
    let name = &saved_name(name);
    if !valid_secret_name(key) {
        return Err("Use an environment variable name, such as JIRA_API_TOKEN.".into());
    }
    if value.is_empty() || value.len() > 16 * 1024 {
        return Err("Provide a value of up to 16 KiB.".into());
    }
    edit(directory, name, |servers, index| {
        let server = &mut servers[index];
        // A URL server's secrets are bound to its origin, so a value saved for one address is
        // never sent to another.
        let previous = server
            .secrets
            .iter()
            .any(|existing| existing == key)
            .then(|| server.account(key));
        match server.endpoint() {
            Some(origin) => server.secret_origins.insert(key.into(), origin),
            None => server.secret_origins.remove(key),
        };
        let account = server.account(key);
        set(SECRET_SERVICE, &account, value).map_err(|error| {
            error.explain_save(format!(
                "The credential store could not be updated. {}, then try again.",
                crate::secrets::RECOVER
            ))
        })?;
        if let Some(previous) = previous.filter(|previous| *previous != account) {
            crate::secrets::delete(SECRET_SERVICE, &previous).map_err(store_error)?;
        }
        if !server.secrets.iter().any(|existing| existing == key) {
            server.secrets.push(key.into());
        }
        Ok(())
    })
}

/// Blocking: the credential store may ask the owner for access.
pub fn delete_secret(directory: &Path, name: &str, key: &str) -> Result<(), String> {
    let name = &saved_name(name);
    edit(directory, name, |servers, index| {
        let server = &mut servers[index];
        crate::secrets::delete(SECRET_SERVICE, &server.account(key)).map_err(store_error)?;
        server.secrets.retain(|existing| existing != key);
        server.secret_origins.remove(key);
        Ok(())
    })
}

struct Tool {
    exposed: String,
    name: String,
    description: String,
    input_schema: Value,
}

struct Server {
    name: String,
    /// Defined by the trusted project rather than the owner.
    project: bool,
    /// The host of a URL server, shown when asking to call its tools.
    host: Option<String>,
    connection: Mutex<Connection>,
    tools: Vec<Tool>,
}

/// MCP servers started for one run. Dropping them stops every server process and ends every HTTP
/// session.
#[derive(Default)]
pub struct McpServers {
    servers: Vec<Server>,
    pub warnings: Vec<String>,
    /// What a server that did not start printed, for the owner only: never in `warnings`, which
    /// are shown to the owner as they stand, and never traced. Neither reaches the model
    /// (`Instructions::prompt()` leaves warnings out).
    pub diagnostics: Vec<ServerDiagnostic>,
}

impl McpServers {
    /// Start enabled servers from owner data and, when `workspace` is given and its
    /// `.blackwall/project.json` is trusted, the project's URL servers as `p-<name>`. Failures
    /// become warnings and never stop the run.
    pub async fn start(directory: &Path, workspace: Option<&Path>, reserved: &[Value]) -> Self {
        let mut started = Self::default();
        let path = directory.join(MCP_FILE);
        let read = crate::policy::read_bounded(&path, MAX_FILE_BYTES);
        let configs = match tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into()))
        {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(_) => Err("The MCP file could not be read."),
            Ok(body) => parse(&body),
        };
        // A problem with the owner's file does not hold back the project's servers.
        let configs = configs.unwrap_or_else(|error| {
            started
                .warnings
                .push(format!("{error} MCP servers are unavailable for this run."));
            vec![]
        });
        let mut names: Vec<String> = reserved
            .iter()
            .filter_map(|definition| definition["function"]["name"].as_str())
            .map(str::to_owned)
            .collect();
        // Children never search the project for programs; without a project nothing is excluded.
        let project = workspace
            .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
            .unwrap_or_default();
        started
            .add_servers(
                configs,
                Source::Owner,
                directory,
                &project,
                &mut names,
                STARTUP_TIMEOUT,
            )
            .await;
        if let Some(workspace) = workspace {
            let projects = project_servers(directory, workspace).await;
            if !projects.is_empty() {
                started
                    .add_servers(
                        projects,
                        Source::Project,
                        directory,
                        &project,
                        &mut names,
                        STARTUP_TIMEOUT,
                    )
                    .await;
            }
        }
        started
    }

    /// Start enabled servers from one file. `names` holds every tool name already taken.
    async fn add_servers(
        &mut self,
        configs: Vec<ServerConfig>,
        source: Source,
        directory: &Path,
        project: &Path,
        names: &mut Vec<String>,
        startup_timeout: Duration,
    ) {
        let configs = match validate_for(configs, source) {
            Ok(configs) => configs,
            Err(error) => {
                self.warnings
                    .push(format!("{error} MCP servers are unavailable for this run."));
                return;
            }
        };
        let defined = match source {
            Source::Owner => "",
            Source::Project => "Project ",
        };
        for config in configs.into_iter().filter(|config| config.enabled) {
            if source == Source::Owner && config.name.starts_with("p-") {
                self.warnings.push(format!(
                    "MCP server {} was skipped: {RESERVED_PREFIX}",
                    config.name
                ));
                continue;
            }
            // A credential store prompt gets its own, longer wait than the server's startup.
            let environment =
                match tokio::time::timeout(SECRET_TIMEOUT, read_secrets(&config)).await {
                    Ok(Ok(environment)) => environment,
                    Ok(Err(error)) => {
                        self.warnings
                            .push(format!("MCP server {} did not start: {error}", config.name));
                        continue;
                    }
                    Err(_) => {
                        self.warnings.push(format!(
                            "MCP server {} is waiting for {}. {}, then try again.",
                            config.name,
                            crate::secrets::STORE,
                            crate::secrets::RECOVER
                        ));
                        continue;
                    }
                };
            // A project's server must be on the public internet, and the address checked here is
            // the one connected to.
            let pinned = match source {
                Source::Owner => None,
                Source::Project => {
                    match http::public_addresses(config.url.as_deref().unwrap_or_default()).await {
                        Ok(pinned) => pinned,
                        Err(error) => {
                            self.warnings.push(format!(
                                "{defined}MCP server {} did not start: {error}",
                                config.name
                            ));
                            continue;
                        }
                    }
                }
            };
            // The child gets an allow-list, not this process's environment.
            let env = crate::child_env::from_process(
                crate::child_env::Scope::Owner,
                project,
                &config.env,
            );
            // Names only: what a failing local server may have been missing.
            let not_passed = match config.url {
                None => env.dropped().to_vec(),
                Some(_) => vec![],
            };
            let hint = if not_passed.is_empty() {
                ""
            } else {
                " If it needs one of your environment variables, add its name to \"env\" in mcp.json."
            };
            // Kept outside `launch`, so a server that never answers can still be read.
            let tail = StdMutex::new(None);
            let secrets: Vec<String> = environment.iter().map(|(_, value)| value.clone()).collect();
            let launched = tokio::time::timeout(
                startup_timeout,
                launch(
                    &config,
                    directory,
                    project,
                    &env,
                    environment,
                    pinned,
                    &tail,
                ),
            )
            .await;
            let failure = match launched {
                Ok(Ok(connection)) => {
                    self.add(config, connection, names).await;
                    continue;
                }
                Ok(Err(error)) => format!("did not start: {error}"),
                Err(_) => timeout_failure(startup_timeout),
            };
            self.warnings
                .push(format!("MCP server {} {failure}{hint}", config.name));
            let tail = tail.into_inner().unwrap_or_else(|p| p.into_inner());
            let stderr = match tail {
                Some(tail) => {
                    tail.settle().await;
                    tail.text(&secrets)
                }
                None => String::new(),
            };
            if !stderr.is_empty() || !not_passed.is_empty() {
                self.diagnostics.push(ServerDiagnostic {
                    server: config.name,
                    stderr,
                    not_passed,
                });
            }
        }
    }

    /// Start only the server just added, allowing package runners time for their first download.
    pub async fn first_start(directory: &Path, name: &str) -> Result<usize, String> {
        let config = read_config(directory)?
            .into_iter()
            .find(|config| config.name == name)
            .ok_or_else(|| format!("No MCP server is named {name:?}."))?;
        let mut started = Self::default();
        let mut names = vec![];
        started
            .add_servers(
                vec![config],
                Source::Owner,
                directory,
                Path::new(""),
                &mut names,
                FIRST_START_TIMEOUT,
            )
            .await;
        if let Some(warning) = started.warnings.first() {
            Err(warning.clone())
        } else {
            Ok(started.definitions().len())
        }
    }

    async fn add(
        &mut self,
        config: ServerConfig,
        mut connection: Connection,
        names: &mut Vec<String>,
    ) {
        let listed = tokio::time::timeout(STARTUP_TIMEOUT, connection.list_tools()).await;
        let listed = match listed {
            Ok(Ok(listed)) => listed,
            _ => {
                self.warnings.push(format!(
                    "MCP server {} did not list its tools.",
                    config.name
                ));
                return;
            }
        };
        let mut tools = vec![];
        for tool in listed {
            let Some(name) = tool["name"].as_str() else {
                continue;
            };
            // A name carrying a secret would reach the model and telemetry, so it is never
            // offered, and the warning does not repeat it.
            if redact(name.to_owned(), &connection.secrets) != name {
                self.warnings.push(format!(
                    "A tool from MCP server {} was skipped because its name contains one of the server's secrets.",
                    config.name
                ));
                continue;
            }
            let allowed = config
                .allow_tools
                .as_ref()
                .is_none_or(|allow| allow.iter().any(|item| item == name))
                && !config.deny_tools.iter().any(|item| item == name);
            if !allowed {
                continue;
            }
            let exposed = format!("{}__{name}", config.name);
            if !valid_tool_name(&exposed) || names.contains(&exposed) {
                self.warnings.push(format!(
                    "MCP tool {name} from {} was skipped because its name is invalid or already used.",
                    config.name
                ));
                continue;
            }
            names.push(exposed.clone());
            tools.push(Tool {
                exposed,
                name: name.to_owned(),
                description: redact(
                    tool["description"]
                        .as_str()
                        .unwrap_or_default()
                        .chars()
                        .take(1024)
                        .collect(),
                    &connection.secrets,
                ),
                // A schema is text the model reads too, so a secret echoed into it is removed.
                input_schema: tool
                    .get("inputSchema")
                    .filter(|schema| schema.is_object())
                    .and_then(|schema| {
                        serde_json::from_str(&redact(schema.to_string(), &connection.secrets)).ok()
                    })
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({"type":"object","properties":{}})),
            });
            if tools.len() == MAX_TOOLS {
                break;
            }
        }
        self.servers.push(Server {
            // Owner servers never have this prefix; those that do are skipped at start.
            project: config.name.starts_with("p-"),
            host: config
                .url
                .as_deref()
                .and_then(|url| reqwest::Url::parse(url).ok())
                .and_then(|url| url.host_str().map(str::to_owned)),
            name: config.name,
            connection: Mutex::new(connection),
            tools,
        });
    }

    pub fn definitions(&self) -> Vec<Value> {
        self.servers
            .iter()
            .flat_map(|server| &server.tools)
            .map(|tool| {
                json!({"type":"function","function":{"name":tool.exposed,"description":tool.description,"parameters":tool.input_schema}})
            })
            .collect()
    }

    pub fn handles(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    fn find(&self, name: &str) -> Option<(&Server, &Tool)> {
        self.servers.iter().find_map(|server| {
            server
                .tools
                .iter()
                .find(|tool| tool.exposed == name)
                .map(|tool| (server, tool))
        })
    }

    /// Server descriptions and annotations never grant permission: every call asks or is decided
    /// by owner rules, which match the namespaced tool name.
    pub async fn call(
        &self,
        name: &str,
        arguments: &str,
        request_id: &str,
        approvals: &Approvals,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
        timeout: Duration,
    ) -> Result<String, ToolError> {
        let (server, tool) = self.find(name).ok_or(ToolError::Arguments)?;
        if arguments.len() > MAX_ARGUMENT_BYTES {
            return Err(ToolError::Arguments);
        }
        let arguments: Value = serde_json::from_str(arguments).map_err(|_| ToolError::Arguments)?;
        if !arguments.is_object() {
            return Err(ToolError::Arguments);
        }
        let detail = match &server.host {
            None => format!(
                "MCP server: {}\nTool: {}\nArguments: {arguments}\n\nThe server runs with your account permissions and may change data in the services it connects to.",
                server.name, tool.name
            ),
            Some(host) => format!(
                "MCP server: {} ({host}){}\nTool: {}\nArguments: {arguments}\n\nThe arguments are sent to {host}, which may change data in the services it connects to.",
                server.name,
                if server.project { ", defined by this project" } else { "" },
                tool.name
            ),
        };
        let approval = ApprovalTool {
            server: server.name.clone(),
            tool: tool.exposed.clone(),
        };
        if !approvals
            .request_tool(request_id, approval, detail, emit)
            .await
            .map_err(ToolError::Execution)?
        {
            return Err(ToolError::Denied);
        }
        let mut connection = server.connection.lock().await;
        let result = tokio::time::timeout(
            timeout,
            connection.request(
                "tools/call",
                json!({"name":tool.name,"arguments":arguments}),
            ),
        )
        .await;
        let result = match result {
            Ok(result) => result.map_err(|error| connection.redact(error))?,
            Err(_) => {
                // A late reply would arrive out of order, so the server is stopped and not reused.
                connection.broken = true;
                connection.transport.abandon();
                return Err(ToolError::Execution(
                    "The MCP tool exceeded its time limit and was stopped.".into(),
                ));
            }
        };
        let text = result["content"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|part| match part["type"].as_str() {
                Some("text") => part["text"].as_str().unwrap_or_default().to_owned(),
                Some(kind) => format!("[{kind} content omitted]"),
                None => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = redact(text, &connection.secrets);
        if text.len() > MAX_OUTPUT_BYTES {
            return Err(ToolError::Execution(
                "The MCP tool result exceeded 64 KiB.".into(),
            ));
        }
        if result["isError"] == true {
            Err(ToolError::Execution(text))
        } else {
            Ok(text)
        }
    }
}

struct Connection {
    transport: Box<dyn Transport>,
    next_id: u64,
    broken: bool,
    /// Values given to the server, removed from anything it sends back.
    secrets: Vec<String>,
}

impl Connection {
    fn new(transport: Box<dyn Transport>, secrets: Vec<String>) -> Self {
        Self {
            transport,
            next_id: 0,
            broken: false,
            secrets,
        }
    }

    fn redact(&self, error: ToolError) -> ToolError {
        match error {
            ToolError::Execution(text) => ToolError::Execution(redact(text, &self.secrets)),
            other => other,
        }
    }

    async fn initialize(&mut self) -> Result<(), ToolError> {
        let result = self
            .request_once(
            "initialize",
            json!({"protocolVersion":PROTOCOL_VERSION,"capabilities":{},"clientInfo":{"name":"Blackwall","version":env!("CARGO_PKG_VERSION")}}),
        )
        .await?;
        // Tool discovery and calls are unchanged across these versions.
        let version = result["protocolVersion"].as_str().unwrap_or_default();
        if !["2025-06-18", "2025-03-26", "2024-11-05"].contains(&version) {
            return Err(ToolError::Execution(
                "The MCP server uses an unsupported protocol version.".into(),
            ));
        }
        self.transport.negotiated(version);
        self.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await
    }

    async fn list_tools(&mut self) -> Result<Vec<Value>, ToolError> {
        let mut tools = vec![];
        let mut cursor: Option<String> = None;
        for _ in 0..8 {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
            let page = self.request("tools/list", params).await?;
            tools.extend(page["tools"].as_array().cloned().unwrap_or_default());
            cursor = page["nextCursor"].as_str().map(str::to_owned);
            if cursor.is_none() || tools.len() >= MAX_TOOLS {
                break;
            }
        }
        Ok(tools)
    }

    async fn send(&mut self, message: &Value) -> Result<(), ToolError> {
        let sent = self.transport.send(message).await;
        if sent.is_err() {
            self.broken = true;
        }
        sent
    }

    /// Send one request. A server that forgot its session is initialized again, once, and the
    /// request is retried.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, ToolError> {
        match self.request_once(method, params.clone()).await {
            Err(_) if self.transport.expired() => {
                self.broken = false;
                self.initialize().await?;
                self.request_once(method, params).await
            }
            result => result,
        }
    }

    async fn request_once(&mut self, method: &str, params: Value) -> Result<Value, ToolError> {
        if self.broken {
            return Err(ToolError::Execution(
                "The MCP server stopped responding.".into(),
            ));
        }
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        let mut asked = 0;
        loop {
            let message = match self.transport.recv().await {
                Ok(message) => message,
                Err(error) => {
                    self.broken = true;
                    return Err(error);
                }
            };
            if message.get("method").is_some() {
                // Server requests are not supported; answer so the server does not wait.
                if let Some(request) = message.get("id") {
                    asked += 1;
                    if asked > MAX_SERVER_REQUESTS {
                        self.broken = true;
                        return Err(ToolError::Execution(
                            "The MCP server sent too many requests of its own.".into(),
                        ));
                    }
                    let reply = if message["method"] == "ping" {
                        json!({"jsonrpc":"2.0","id":request,"result":{}})
                    } else {
                        json!({"jsonrpc":"2.0","id":request,"error":{"code":-32601,"message":"Method not found"}})
                    };
                    self.send(&reply).await?;
                }
                continue;
            }
            if message["id"] != id {
                continue;
            }
            if let Some(error) = message.get("error") {
                let text: String = error["message"]
                    .as_str()
                    .unwrap_or("The MCP server returned an error.")
                    .chars()
                    .take(500)
                    .collect();
                return Err(ToolError::Execution(text));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

/// For each header of a URL server: the header, the secret's name, and its credential store
/// account. A secret saved for another origin, or for a local command, is never used.
fn header_accounts(config: &ServerConfig) -> Result<Vec<(String, String, String)>, ToolError> {
    let origin = config.endpoint().unwrap_or_default();
    let mut accounts = vec![];
    for (header, reference) in &config.headers {
        let key = http::secret_reference(reference).unwrap_or_default();
        let again = format!("Save it again with bw mcp secret {} {key}.", config.name);
        if !config.secrets.iter().any(|saved| saved == key) {
            return Err(ToolError::Execution(format!(
                "{key} is not saved. Add it with bw mcp secret {} {key}.",
                config.name
            )));
        }
        match config.secret_origins.get(key) {
            Some(saved) if *saved == origin => {}
            Some(saved) => {
                return Err(ToolError::Execution(format!(
                    "{key} was saved for {saved}, not {origin}. {again}"
                )))
            }
            None => {
                return Err(ToolError::Execution(format!(
                    "{key} was saved for a local command, not {origin}. {again}"
                )))
            }
        }
        accounts.push((header.clone(), key.to_owned(), config.account(key)));
    }
    Ok(accounts)
}

/// Read a server's secrets from the credential store, which may first ask the owner for access.
/// A local server gets environment variables; a URL server gets headers. Each pair is the
/// variable or header name and its value.
async fn read_secrets(config: &ServerConfig) -> Result<Vec<(String, String)>, ToolError> {
    let wanted = if config.url.is_some() {
        header_accounts(config)?
    } else {
        config
            .secrets
            .iter()
            .map(|key| (key.clone(), key.clone(), config.account(key)))
            .collect()
    };
    let mut values = vec![];
    for (target, name, account) in wanted {
        let value =
            tokio::task::spawn_blocking(move || crate::secrets::get(SECRET_SERVICE, &account))
                .await
                .map_err(|_| ToolError::Execution("The credential store did not respond.".into()))?
                .map_err(|error| {
                    ToolError::Execution(error.explain(format!(
                        "{name} could not be read from {}.",
                        crate::secrets::STORE
                    )))
                })?
                .ok_or_else(|| {
                    ToolError::Execution(format!(
                        "{name} is not saved. Add it with bw mcp secret {} {name}.",
                        config.name
                    ))
                })?;
        values.push((target, value));
    }
    Ok(values)
}

/// Servers start in the data folder so a project cannot shadow what a package runner loads.
/// The trusted project's URL servers, named `p-<name>`. An untrusted, changed or unusable file
/// adds none; its warnings are reported by the run's policy.
async fn project_servers(directory: &Path, workspace: &Path) -> Vec<ServerConfig> {
    let (data, folder) = (directory.to_path_buf(), workspace.to_path_buf());
    let loaded = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || crate::project::try_load(&folder, &data)),
    )
    .await;
    let Ok(Ok(Ok(loaded))) = loaded else {
        return vec![];
    };
    if loaded.trust != crate::project::Trust::Trusted {
        return vec![];
    }
    let servers = loaded.config.mcp.map(|mcp| mcp.servers).unwrap_or_default();
    servers
        .into_iter()
        .map(|server| ServerConfig {
            name: format!("p-{}", server.name),
            command: vec![],
            url: Some(server.url),
            headers: BTreeMap::new(),
            enabled: server.enabled,
            allow_tools: server.allow_tools,
            deny_tools: server.deny_tools,
            secrets: vec![],
            secret_origins: BTreeMap::new(),
            env: vec![],
        })
        .collect()
}

async fn launch(
    config: &ServerConfig,
    directory: &Path,
    project: &Path,
    env: &crate::child_env::ChildEnv,
    environment: Vec<(String, String)>,
    pinned: Option<http::Pinned>,
    tail: &StdMutex<Option<StderrTail>>,
) -> Result<Connection, ToolError> {
    let transport: Box<dyn Transport> = match &config.url {
        Some(url) => Box::new(http::HttpTransport::new(
            url,
            http::headers(&environment)?,
            pinned,
        )?),
        None => start_process(config, directory, project, env, &environment, tail)?,
    };
    let secrets = environment.into_iter().map(|(_, value)| value).collect();
    let mut connection = Connection::new(transport, secrets);
    let initialized = connection.initialize().await;
    initialized.map_err(|error| connection.redact(error))?;
    Ok(connection)
}

fn start_process(
    config: &ServerConfig,
    directory: &Path,
    project: &Path,
    env: &crate::child_env::ChildEnv,
    secrets: &[(String, String)],
    tail: &StdMutex<Option<StderrTail>>,
) -> Result<Box<dyn Transport>, ToolError> {
    #[cfg(any(unix, windows))]
    {
        let program = crate::command_tools::resolve_program(&config.command[0], project)?;
        let (process, stdin, stdout, stderr) =
            crate::tools::spawn_server(directory, &program, &config.command[1..], env, secrets)?;
        // Draining the logs keeps a chatty server from blocking. Their end is kept for the owner
        // when the server does not start.
        *tail.lock().unwrap_or_else(|p| p.into_inner()) = Some(StderrTail::spawn_reader(stderr));
        Ok(Box::new(StdioTransport {
            reader: Box::new(BufReader::new(stdout)),
            writer: Box::new(stdin),
            process: Some(process),
        }))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (config, directory, project, env, secrets, tail);
        Err(ToolError::Execution(
            "MCP servers are not supported on this platform.".into(),
        ))
    }
}

fn redact(text: String, secrets: &[String]) -> String {
    secrets.iter().fold(text, |text, secret| {
        text.replace(secret.as_str(), "[redacted]")
    })
}

/// Secret names are environment variable names: uppercase letters, digits, and `_`.
pub fn valid_secret_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// A policy `tool` value: a server name, or `server__tool` for one of its tools.
pub(crate) fn valid_tool_rule(rule: &str) -> bool {
    match rule.split_once("__") {
        Some((server, _)) => valid_server_name(server) && valid_tool_name(rule),
        None => valid_server_name(rule),
    }
}

pub(crate) fn valid_server_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Model tool names must stay within the common function-name alphabet and length.
pub(crate) fn valid_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Where server entries come from. Project servers, from a trusted project file, are URL-only,
/// carry no secrets or headers, and use the reserved `p-` name prefix.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Owner,
    Project,
}

fn validate(servers: Vec<ServerConfig>) -> Result<Vec<ServerConfig>, &'static str> {
    validate_for(servers, Source::Owner)
}

fn validate_for(
    servers: Vec<ServerConfig>,
    source: Source,
) -> Result<Vec<ServerConfig>, &'static str> {
    if servers.len() > MAX_SERVERS {
        return Err("The MCP file has too many servers.");
    }
    for (index, server) in servers.iter().enumerate() {
        if !valid_server_name(&server.name) {
            return Err(
                "MCP server names use lowercase letters, numbers, and hyphens, up to 32 characters.",
            );
        }
        if servers[..index]
            .iter()
            .any(|other| other.name == server.name)
        {
            return Err("Two MCP servers have the same name.");
        }
        match source {
            Source::Project if !server.name.starts_with("p-") => {
                return Err("Project MCP server names start with p-.");
            }
            Source::Project if server.url.is_none() || !server.command.is_empty() => {
                return Err("A project can only add MCP servers by URL.");
            }
            Source::Project if !server.secrets.is_empty() || !server.headers.is_empty() => {
                return Err("Project MCP servers cannot use secrets or headers.");
            }
            Source::Project if !server.env.is_empty() => {
                return Err("Project MCP servers cannot use env.");
            }
            _ => {}
        }
        match &server.url {
            Some(_) if !server.command.is_empty() => {
                return Err("Give each MCP server either a command or a URL, not both.");
            }
            Some(_) if !server.env.is_empty() => {
                return Err("Only MCP servers with a command take env.");
            }
            Some(url) => {
                if !http::valid_url(url) {
                    return Err("An MCP server URL must use https, or http on 127.0.0.1 or ::1, and must not include a user name or password.");
                }
                if server.headers.len() > 16
                    || !server
                        .headers
                        .iter()
                        .all(|(name, value)| http::valid_header(name, value))
                {
                    return Err("MCP server headers must be {secret:NAME}, naming a secret saved for that server, and cannot replace the protocol's own headers.");
                }
            }
            None => {
                if server.command.is_empty() || server.command.len() > 64 {
                    return Err("Give each MCP server a command with at most 63 arguments.");
                }
                if !crate::command_tools::valid_program(&server.command[0]) {
                    return Err(if cfg!(windows) {
                        "An MCP server program must be an .exe given by name or absolute path; batch files are not supported."
                    } else {
                        "An MCP server program must be a program name or an absolute path, and not a .bat or .cmd file."
                    });
                }
                if !server.headers.is_empty() {
                    return Err("Only MCP servers with a URL take headers.");
                }
            }
        }
        crate::child_env::valid_pass_through_list(&server.env)?;
        if server.secrets.len() > 16
            || server.secrets.iter().enumerate().any(|(index, name)| {
                !valid_secret_name(name) || server.secrets[..index].contains(name)
            })
        {
            return Err("An MCP server has an invalid secret name.");
        }
        if server.secret_origins.iter().any(|(key, origin)| {
            !server.secrets.contains(key) || http::origin(origin).as_ref() != Some(origin)
        }) {
            return Err("An MCP server lists a secret origin that is not valid.");
        }
    }
    Ok(servers)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::protocol::{ApprovalDecision, ResolveApprovalRequest};
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use transport::read_message;

    fn stdio(
        reader: impl tokio::io::AsyncBufRead + Send + Unpin + 'static,
        writer: impl tokio::io::AsyncWrite + Send + Unpin + 'static,
    ) -> Connection {
        let transport = StdioTransport {
            reader: Box::new(reader),
            writer: Box::new(writer),
            #[cfg(any(unix, windows))]
            process: None,
        };
        Connection::new(Box::new(transport), vec![])
    }

    /// An in-memory MCP server: lists fixed tools and answers calls by name.
    fn fake_server(calls: Arc<StdMutex<Vec<String>>>) -> Connection {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client);
        tokio::spawn(async move {
            let (server_read, mut server_write) = tokio::io::split(server);
            let mut lines = BufReader::new(server_read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let message: Value = serde_json::from_str(&line).unwrap();
                // Notifications and the client's replies to server requests need no answer.
                let (Some(id), Some(method)) =
                    (message.get("id").cloned(), message["method"].as_str())
                else {
                    continue;
                };
                let result = match method {
                    "initialize" => json!({"protocolVersion":PROTOCOL_VERSION,"capabilities":{}}),
                    "tools/list" => json!({"tools":[
                        {"name":"search","description":"Search issues","inputSchema":{"type":"object","properties":{"q":{"type":"string"}}},"annotations":{"readOnlyHint":true}},
                        {"name":"delete_issue","description":"Delete"},
                        {"name":"fail"},
                        {"name":"slow"},
                        {"name":"bad name!"},
                        {"name":"read_file"}
                    ]}),
                    "tools/call" => {
                        let name = message["params"]["name"].as_str().unwrap().to_owned();
                        calls.lock().unwrap().push(name.clone());
                        match name.as_str() {
                            "slow" => continue,
                            "fail" => {
                                json!({"isError":true,"content":[{"type":"text","text":"boom"}]})
                            }
                            _ => {
                                // A server request before the reply must not confuse the client.
                                let ping = json!({"jsonrpc":"2.0","id":"server-1","method":"ping"});
                                server_write
                                    .write_all(format!("{ping}\n").as_bytes())
                                    .await
                                    .unwrap();
                                json!({"content":[{"type":"text","text":format!("found {}", message["params"]["arguments"])}]})
                            }
                        }
                    }
                    _ => continue,
                };
                let reply = json!({"jsonrpc":"2.0","id":id,"result":result});
                server_write
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        stdio(BufReader::new(client_read), client_write)
    }

    async fn servers(calls: Arc<StdMutex<Vec<String>>>, reserved: &[&str]) -> McpServers {
        let mut connection = fake_server(calls);
        connection.initialize().await.unwrap();
        let config = ServerConfig {
            name: "jira".into(),
            command: vec!["jira-mcp".into()],
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
            allow_tools: None,
            deny_tools: vec!["delete_issue".into()],
            secrets: vec![],
            secret_origins: BTreeMap::new(),
            env: vec![],
        };
        let mut names: Vec<String> = reserved.iter().map(|name| (*name).to_owned()).collect();
        let mut servers = McpServers::default();
        servers.add(config, connection, &mut names).await;
        servers
    }

    fn decide(approvals: &Approvals, decision: ApprovalDecision) -> impl Fn(AgentEvent) {
        let approvals = approvals.clone();
        move |event| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } = event
            {
                approvals
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision,
                    })
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn discovery_namespaces_filters_and_skips_colliding_tools() {
        let servers = servers(Arc::default(), &["read_file", "jira__fail"]).await;
        let names: Vec<_> = servers
            .definitions()
            .iter()
            .map(|definition| definition["function"]["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["jira__search", "jira__slow", "jira__read_file"]);
        assert!(servers.warnings.iter().any(|w| w.contains("bad name!")));
        assert!(servers.warnings.iter().any(|w| w.contains("tool fail ")));
        assert!(servers.handles("jira__search") && !servers.handles("search"));
    }

    #[tokio::test]
    async fn tools_named_with_a_secret_are_skipped_without_naming_them() {
        let mut connection = fake_server(Arc::default());
        connection.initialize().await.unwrap();
        connection.secrets = vec!["slow".into()];
        let config = ServerConfig {
            name: "jira".into(),
            command: vec!["jira-mcp".into()],
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
            allow_tools: None,
            deny_tools: vec![],
            secrets: vec!["JIRA_API_TOKEN".into()],
            secret_origins: BTreeMap::new(),
            env: vec![],
        };
        let mut servers = McpServers::default();
        servers.add(config, connection, &mut vec![]).await;
        let names = servers.definitions();
        assert!(names
            .iter()
            .all(|definition| !definition.to_string().contains("slow")));
        assert!(servers.handles("jira__search") && !servers.handles("jira__slow"));
        assert!(servers.warnings.iter().all(|w| !w.contains("slow")));
        assert!(servers
            .warnings
            .iter()
            .any(|w| w.contains("one of the server's secrets")));
    }

    #[tokio::test]
    async fn secret_values_are_removed_from_server_output() {
        let mut connection = fake_server(Arc::default());
        connection.initialize().await.unwrap();
        connection.secrets = vec!["boom".into(), "found".into()];
        let config = ServerConfig {
            name: "jira".into(),
            command: vec!["jira-mcp".into()],
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
            allow_tools: Some(vec!["search".into(), "fail".into()]),
            deny_tools: vec![],
            secrets: vec!["JIRA_API_TOKEN".into()],
            secret_origins: BTreeMap::new(),
            env: vec![],
        };
        let mut servers = McpServers::default();
        servers.add(config, connection, &mut vec![]).await;
        let approvals = Approvals::default();
        let allow = decide(&approvals, ApprovalDecision::Allow);
        let timeout = Duration::from_secs(5);
        let found = servers
            .call(
                "jira__search",
                r#"{"q":"x"}"#,
                "run",
                &approvals,
                &allow,
                timeout,
            )
            .await;
        assert_eq!(found.unwrap(), r#"[redacted] {"q":"x"}"#);
        assert!(matches!(
            servers.call("jira__fail", "{}", "run", &approvals, &allow, timeout).await,
            Err(ToolError::Execution(message)) if message == "[redacted]"
        ));
    }

    #[tokio::test]
    async fn calls_need_approval_and_report_errors_and_timeouts() {
        let calls = Arc::new(StdMutex::new(vec![]));
        let servers = servers(calls.clone(), &[]).await;
        let approvals = Approvals::default();
        let deny = decide(&approvals, ApprovalDecision::Deny);
        let allow = decide(&approvals, ApprovalDecision::Allow);
        assert!(matches!(
            servers
                .call(
                    "jira__search",
                    r#"{"q":"x"}"#,
                    "run",
                    &approvals,
                    &deny,
                    Duration::from_secs(5)
                )
                .await,
            Err(ToolError::Denied)
        ));
        assert!(calls.lock().unwrap().is_empty());
        let found = servers
            .call(
                "jira__search",
                r#"{"q":"x"}"#,
                "run",
                &approvals,
                &allow,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(found, r#"found {"q":"x"}"#);
        assert!(matches!(
            servers
                .call(
                    "jira__search",
                    "[]",
                    "run",
                    &approvals,
                    &allow,
                    Duration::from_secs(5)
                )
                .await,
            Err(ToolError::Arguments)
        ));
        assert!(matches!(
            servers
                .call("jira__fail", "{}", "run", &approvals, &allow, Duration::from_secs(5))
                .await,
            Err(ToolError::Execution(message)) if message == "boom"
        ));
        assert!(matches!(
            servers
                .call("jira__slow", "{}", "run", &approvals, &allow, Duration::from_millis(50))
                .await,
            Err(ToolError::Execution(message)) if message.contains("time limit")
        ));
        assert!(servers
            .call(
                "jira__search",
                "{}",
                "run",
                &approvals,
                &allow,
                Duration::from_secs(5)
            )
            .await
            .is_err());
    }

    #[test]
    fn mcp_and_tools_files_reject_injection_names_in_env() {
        assert!(
            parse(br#"{"servers":[{"name":"a","command":["x"],"env":["LD_PRELOAD"]}]}"#).is_err()
        );
        assert!(
            parse(br#"{"servers":[{"name":"a","command":["x"],"env":["GITHUB_TOKEN"]}]}"#).is_ok()
        );
        assert!(parse(br#"{"servers":[{"name":"a","command":["x"],"env":["PATH"]}]}"#).is_err());
        let many: Vec<String> = (0..33).map(|i| format!("V{i}")).collect();
        let body = serde_json::json!({"servers":[{"name":"a","command":["x"],"env":many}]});
        assert!(parse(body.to_string().as_bytes()).is_err());
    }

    #[test]
    fn project_servers_cannot_carry_env() {
        let mut server = ServerConfig {
            name: "p-docs".into(),
            command: vec![],
            url: Some("https://docs.example/mcp".into()),
            headers: BTreeMap::new(),
            enabled: true,
            allow_tools: None,
            deny_tools: vec![],
            secrets: vec![],
            env: vec![],
            secret_origins: BTreeMap::new(),
        };
        assert!(validate_for(vec![server.clone()], Source::Project).is_ok());
        server.env = vec!["HOME".into()];
        assert!(validate_for(vec![server], Source::Project).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn python_server_starts_despite_fake_appimage_environment() {
        use crate::child_env::{build, Scope};
        let system_path = "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin";
        let Some(python) = std::env::split_paths(system_path)
            .map(|dir| dir.join("python3"))
            .find(|candidate| candidate.is_file())
        else {
            eprintln!("skipped: python3 is not on the system PATH");
            return;
        };
        let source = [
            ("APPDIR", "/nonexistent"),
            ("PYTHONHOME", "/nonexistent/usr"),
            ("PYTHONPATH", "/nonexistent"),
            ("PATH", system_path),
        ]
        .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v)));
        let env = build(source, Scope::Owner, None, Path::new(""), &[]);
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_echo.py");
        let config = ServerConfig {
            name: "echo".into(),
            command: vec![python.to_string_lossy().into_owned(), script.into()],
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
            allow_tools: None,
            deny_tools: vec![],
            secrets: vec![],
            env: vec![],
            secret_origins: BTreeMap::new(),
        };
        let transport = start_process(
            &config,
            &std::env::temp_dir(),
            Path::new(""),
            &env,
            &[],
            &Default::default(),
        )
        .unwrap();
        let mut connection = Connection::new(transport, vec![]);
        tokio::time::timeout(Duration::from_secs(10), connection.initialize())
            .await
            .unwrap()
            .unwrap();
        let tools = connection.list_tools().await.unwrap();
        let names: Vec<_> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, ["echo"]);
    }

    #[test]
    fn secret_names_must_be_unique_environment_names() {
        let server = |secrets: &[&str]| ServerConfig {
            name: "jira".into(),
            command: vec!["jira-mcp".into()],
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
            allow_tools: None,
            deny_tools: vec![],
            secrets: secrets.iter().map(|name| (*name).to_owned()).collect(),
            secret_origins: BTreeMap::new(),
            env: vec![],
        };
        assert!(validate(vec![server(&["JIRA_API_TOKEN", "JIRA_2"])]).is_ok());
        for invalid in [
            &["lower"][..],
            &["2FA"],
            &["A-B"],
            &[""],
            &["TOKEN", "TOKEN"],
        ] {
            assert!(validate(vec![server(invalid)]).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn simultaneous_server_adds_keep_every_change() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-mcp-race-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        std::thread::scope(|scope| {
            for index in 0..8 {
                let directory = &directory;
                scope.spawn(move || {
                    add_server(directory, &format!("server-{index}"), vec!["server".into()])
                        .unwrap();
                });
            }
        });
        assert_eq!(read_config(&directory).unwrap().len(), 8);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn changing_an_endpoint_keeps_the_rest_of_the_server() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-mcp-edit-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        add_server(&directory, "time", vec!["uvx".into()]).unwrap();
        add_http_server(&directory, "docs", "https://mcp.example.com/mcp", vec![]).unwrap();
        let mut servers = read_config(&directory).unwrap();
        servers[0].secrets = vec!["TOKEN".into()];
        servers[0].enabled = false;
        write_config(&directory, servers).unwrap();

        let command = vec!["uvx".into(), "mcp-server-time".into(), "my folder".into()];
        set_endpoint(&directory, "time", command.clone(), None).unwrap();
        set_endpoint(
            &directory,
            "docs",
            vec![],
            Some(" https://mcp.example.org/v2 ".into()),
        )
        .unwrap();
        let servers = read_config(&directory).unwrap();
        assert_eq!(servers[0].command, command);
        assert_eq!(servers[0].secrets, ["TOKEN"]);
        assert!(!servers[0].enabled);
        assert_eq!(
            servers[1].url.as_deref(),
            Some("https://mcp.example.org/v2")
        );

        // A server keeps its kind, and an invalid address leaves the file as it was.
        assert!(set_endpoint(
            &directory,
            "time",
            vec![],
            Some("https://a.example.com".into())
        )
        .is_err());
        assert!(set_endpoint(&directory, "docs", vec!["uvx".into()], None).is_err());
        assert!(set_endpoint(
            &directory,
            "docs",
            vec![],
            Some("http://mcp.example.com".into())
        )
        .is_err());
        assert!(set_endpoint(&directory, "missing", vec!["uvx".into()], None).is_err());
        assert_eq!(read_config(&directory).unwrap()[1].url, servers[1].url);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn changing_a_command_ends_its_approvals_but_other_edits_keep_them() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-mcp-rules-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        add_server(
            &directory,
            "time",
            vec!["uvx".into(), "mcp-server-time".into()],
        )
        .unwrap();
        crate::policy::allow_tool(&directory, "time", None).unwrap();

        set_endpoint(
            &directory,
            "time",
            vec!["uvx".into(), "mcp-server-time".into()],
            None,
        )
        .unwrap();
        set_enabled(&directory, "time", false).unwrap();
        assert_eq!(crate::policy::tool_rules(&directory).unwrap().len(), 1);

        set_endpoint(&directory, "time", vec!["other-server".into()], None).unwrap();
        assert!(crate::policy::tool_rules(&directory).unwrap().is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn failed_server_removal_keeps_rules_for_the_configured_server() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-mcp-remove-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        add_server(&directory, "docs", vec!["server".into()]).unwrap();
        crate::policy::allow_tool(&directory, "docs", None).unwrap();
        let config = std::fs::read(directory.join(MCP_FILE)).unwrap();
        let source = directory.join("source.json");
        std::fs::write(&source, config).unwrap();
        std::fs::remove_file(directory.join(MCP_FILE)).unwrap();
        std::os::unix::fs::symlink(&source, directory.join(MCP_FILE)).unwrap();

        assert!(remove_server(&directory, "docs").is_err());
        assert_eq!(read_config(&directory).unwrap().len(), 1);
        assert_eq!(
            crate::policy::tool_rules(&directory).unwrap()[0].tool,
            "docs"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn invalid_files_warn_and_disabled_servers_never_launch() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-mcp-{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        for body in [
            "not json",
            r#"{"servers":[{"name":"Bad","command":["x"]}]}"#,
            r#"{"servers":[{"name":"a","command":["./local"]}]}"#,
            r#"{"servers":[{"name":"a","command":["x"],"env":{"TOKEN":"secret"}}]}"#,
        ] {
            tokio::fs::write(directory.join(MCP_FILE), body)
                .await
                .unwrap();
            let servers = McpServers::start(&directory, None, &[]).await;
            assert_eq!(servers.warnings.len(), 1, "{body}");
            assert!(servers.definitions().is_empty());
        }
        tokio::fs::write(
            directory.join(MCP_FILE),
            r#"{"servers":[{"name":"off","command":["blackwall-missing-mcp-server"],"enabled":false}]}"#,
        )
        .await
        .unwrap();
        let disabled = McpServers::start(&directory, None, &[]).await;
        assert!(disabled.warnings.is_empty() && disabled.definitions().is_empty());
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    /// Start the one server in `mcp.json` whose command is `/bin/sh -c <script>`.
    #[cfg(unix)]
    async fn start_failing_fixture(script: &str) -> McpServers {
        let directory =
            std::env::temp_dir().join(format!("blackwall-mcp-fail-{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        let body = json!({"servers":[{"name":"broken","command":["/bin/sh","-c",script]}]});
        tokio::fs::write(directory.join(MCP_FILE), body.to_string())
            .await
            .unwrap();
        let servers = McpServers::start(&directory, None, &[]).await;
        tokio::fs::remove_dir_all(&directory).await.unwrap();
        servers
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_server_reports_stderr_to_owner_but_not_in_warnings_or_model() {
        let servers =
            start_failing_fixture("printf 'Traceback: no module named x' >&2; exit 1").await;
        assert!(
            servers.warnings[0].contains("did not start")
                && !servers.warnings[0].contains("Traceback")
        );
        assert!(servers.diagnostics[0].stderr.contains("no module named x"));
        assert_eq!(servers.diagnostics[0].server, "broken");
        assert!(servers.definitions().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_never_answers_still_shows_its_stderr() {
        let servers = start_failing_fixture("echo 'bad token abc' >&2; sleep 30").await;
        assert!(servers.warnings[0].contains("within 15 seconds"));
        assert!(servers.warnings[0].contains("first start may still be downloading"));
        assert!(servers.diagnostics[0].stderr.contains("bad token abc"));
    }

    #[test]
    fn only_the_immediate_first_start_uses_the_long_timeout() {
        assert_eq!(STARTUP_TIMEOUT, Duration::from_secs(15));
        assert_eq!(FIRST_START_TIMEOUT, Duration::from_secs(120));
        assert!(timeout_failure(STARTUP_TIMEOUT).contains("try again"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropped_variables_are_named_for_the_owner_and_only_hinted_in_the_warning() {
        let servers = start_failing_fixture("exit 1").await;
        let dropped = &servers.diagnostics.first().map(|d| d.not_passed.clone());
        let warning = &servers.warnings[0];
        match dropped {
            // Whatever this machine's environment dropped is listed for the owner, not the model.
            Some(names) if !names.is_empty() => {
                assert!(warning.contains("add its name to \"env\" in mcp.json"));
                assert!(names.iter().all(|name| !warning.contains(name.as_str())));
            }
            _ => assert!(!warning.contains("add its name")),
        }
    }

    #[tokio::test]
    async fn oversized_messages_are_rejected() {
        let mut reader: &[u8] = &[b'x'; MAX_MESSAGE_BYTES + 2];
        assert!(read_message(&mut reader).await.is_err());
        let mut reader: &[u8] = b"noise\n{\"id\":1}\n";
        assert_eq!(read_message(&mut reader).await.unwrap().unwrap()["id"], 1);
    }

    #[tokio::test]
    async fn unsupported_protocol_versions_are_refused() {
        let reply: &[u8] =
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"1999-01-01\"}}\n";
        let mut connection = stdio(reply, tokio::io::sink());
        assert!(connection.initialize().await.is_err());
    }
}
