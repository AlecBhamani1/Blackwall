//! Owner-configured local MCP servers whose tools join Agent runs behind Blackwall approvals.
use crate::{
    approvals::Approvals,
    protocol::{AgentEvent, ApprovalTool},
    tools::ToolError,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};
use tokio::{io::BufReader, sync::Mutex};
use transport::{StdioTransport, Transport};

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
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const SECRET_TIMEOUT: Duration = Duration::from_secs(30);
const PROTOCOL_VERSION: &str = "2025-06-18";
const EDIT_LOCK: &str = ".mcp.lock";

/// One server entry in `mcp.json`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ServerConfig {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_tools: Vec<String>,
    /// Names of environment variables the server receives from the platform credential store.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
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
    crate::policy::replace_owner_file(directory, &path, &body).map_err(|_| failed)
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
    let name = saved_name(name);
    let name = name.as_str();
    let _editing = crate::policy::lock_owner_file(directory, EDIT_LOCK)
        .map_err(|_| "The MCP file could not be saved.")?;
    let mut servers = read_config(directory)?;
    if servers.iter().any(|server| server.name == name) {
        return Err(format!("An MCP server named {name:?} already exists."));
    }
    servers.push(ServerConfig {
        name: name.into(),
        command,
        enabled: true,
        allow_tools: None,
        deny_tools: vec![],
        secrets: vec![],
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
        crate::secrets::delete(SECRET_SERVICE, &format!("{name}/{key}")).map_err(store_error)?;
    }
    crate::policy::remove_server_rules(directory, name)
}

/// The value is saved first, so the file never names a secret that failed to save. Blocking.
pub fn save_secret(directory: &Path, name: &str, key: &str, value: &str) -> Result<(), String> {
    let name = &saved_name(name);
    if !valid_secret_name(key) {
        return Err("Use an environment variable name, such as JIRA_API_TOKEN.".into());
    }
    if value.is_empty() || value.len() > 16 * 1024 {
        return Err("Provide a value of up to 16 KiB.".into());
    }
    edit(directory, name, |servers, index| {
        crate::secrets::set(SECRET_SERVICE, &format!("{name}/{key}"), value)
            .map_err(store_error)?;
        if !servers[index]
            .secrets
            .iter()
            .any(|existing| existing == key)
        {
            servers[index].secrets.push(key.into());
        }
        Ok(())
    })
}

/// Blocking: the credential store may ask the owner for access.
pub fn delete_secret(directory: &Path, name: &str, key: &str) -> Result<(), String> {
    let name = &saved_name(name);
    edit(directory, name, |servers, index| {
        crate::secrets::delete(SECRET_SERVICE, &format!("{name}/{key}")).map_err(store_error)?;
        servers[index].secrets.retain(|existing| existing != key);
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
    connection: Mutex<Connection>,
    tools: Vec<Tool>,
}

/// MCP servers started for one run. Dropping them stops every server process.
#[derive(Default)]
pub struct McpServers {
    servers: Vec<Server>,
    pub warnings: Vec<String>,
}

impl McpServers {
    /// Start enabled servers from owner data. Failures become warnings and never stop the run.
    pub async fn start(directory: &Path, reserved: &[Value]) -> Self {
        let mut started = Self::default();
        let path = directory.join(MCP_FILE);
        let read = crate::policy::read_bounded(&path, MAX_FILE_BYTES);
        let configs = match tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into()))
        {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return started,
            Err(_) => Err("The MCP file could not be read."),
            Ok(body) => parse(&body),
        };
        let configs = match configs {
            Ok(configs) => configs,
            Err(error) => {
                started
                    .warnings
                    .push(format!("{error} MCP servers are unavailable for this run."));
                return started;
            }
        };
        let mut names: Vec<String> = reserved
            .iter()
            .filter_map(|definition| definition["function"]["name"].as_str())
            .map(str::to_owned)
            .collect();
        for config in configs.into_iter().filter(|config| config.enabled) {
            // A credential store prompt gets its own, longer wait than the server's startup.
            let environment =
                match tokio::time::timeout(SECRET_TIMEOUT, read_secrets(&config)).await {
                    Ok(Ok(environment)) => environment,
                    Ok(Err(error)) => {
                        started
                            .warnings
                            .push(format!("MCP server {} did not start: {error}", config.name));
                        continue;
                    }
                    Err(_) => {
                        started.warnings.push(format!(
                            "MCP server {} is waiting for {}. {}, then try again.",
                            config.name,
                            crate::secrets::STORE,
                            crate::secrets::RECOVER
                        ));
                        continue;
                    }
                };
            match tokio::time::timeout(STARTUP_TIMEOUT, launch(&config, directory, environment))
                .await
            {
                Ok(Ok(connection)) => {
                    started.add(config, connection, &mut names).await;
                }
                Ok(Err(error)) => started
                    .warnings
                    .push(format!("MCP server {} did not start: {error}", config.name)),
                Err(_) => started.warnings.push(format!(
                    "MCP server {} did not start within 15 seconds.",
                    config.name
                )),
            }
        }
        started
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
                description: tool["description"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(1024)
                    .collect(),
                input_schema: tool
                    .get("inputSchema")
                    .filter(|schema| schema.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object","properties":{}})),
            });
            if tools.len() == MAX_TOOLS {
                break;
            }
        }
        self.servers.push(Server {
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
        let detail = format!(
            "MCP server: {}\nTool: {}\nArguments: {arguments}\n\nThe server runs with your account permissions and may change data in the services it connects to.",
            server.name, tool.name
        );
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
            .request(
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

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, ToolError> {
        if self.broken {
            return Err(ToolError::Execution(
                "The MCP server stopped responding.".into(),
            ));
        }
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
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

/// Servers start in the data folder so a project cannot shadow what a package runner loads.
/// Read a server's secrets from the credential store, which may first ask the owner for access.
async fn read_secrets(config: &ServerConfig) -> Result<Vec<(String, String)>, ToolError> {
    let mut environment = vec![];
    for name in &config.secrets {
        let account = format!("{}/{name}", config.name);
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
        environment.push((name.clone(), value));
    }
    Ok(environment)
}

async fn launch(
    config: &ServerConfig,
    directory: &Path,
    environment: Vec<(String, String)>,
) -> Result<Connection, ToolError> {
    #[cfg(any(unix, windows))]
    {
        let program =
            crate::command_tools::resolve_program(&config.command[0]).ok_or_else(|| {
                ToolError::Execution(format!(
                    "{} was not found on this computer.",
                    config.command[0]
                ))
            })?;
        let (process, stdin, stdout, stderr) =
            crate::tools::spawn_server(directory, &program, &config.command[1..], &environment)?;
        // Server logs are not shown; draining them keeps a chatty server from blocking.
        tokio::spawn(async move {
            let mut stderr = stderr;
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
        });
        let transport = StdioTransport {
            reader: Box::new(BufReader::new(stdout)),
            writer: Box::new(stdin),
            process: Some(process),
        };
        let secrets = environment.into_iter().map(|(_, value)| value).collect();
        let mut connection = Connection::new(Box::new(transport), secrets);
        let initialized = connection.initialize().await;
        initialized.map_err(|error| connection.redact(error))?;
        Ok(connection)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (config, directory, environment);
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

fn validate(servers: Vec<ServerConfig>) -> Result<Vec<ServerConfig>, &'static str> {
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
        if server.secrets.len() > 16
            || server.secrets.iter().enumerate().any(|(index, name)| {
                !valid_secret_name(name) || server.secrets[..index].contains(name)
            })
        {
            return Err("An MCP server has an invalid secret name.");
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
            enabled: true,
            allow_tools: None,
            deny_tools: vec!["delete_issue".into()],
            secrets: vec![],
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
    async fn secret_values_are_removed_from_server_output() {
        let mut connection = fake_server(Arc::default());
        connection.initialize().await.unwrap();
        connection.secrets = vec!["boom".into(), "found".into()];
        let config = ServerConfig {
            name: "jira".into(),
            command: vec!["jira-mcp".into()],
            enabled: true,
            allow_tools: Some(vec!["search".into(), "fail".into()]),
            deny_tools: vec![],
            secrets: vec!["JIRA_API_TOKEN".into()],
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
    fn secret_names_must_be_unique_environment_names() {
        let server = |secrets: &[&str]| ServerConfig {
            name: "jira".into(),
            command: vec!["jira-mcp".into()],
            enabled: true,
            allow_tools: None,
            deny_tools: vec![],
            secrets: secrets.iter().map(|name| (*name).to_owned()).collect(),
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
            let servers = McpServers::start(&directory, &[]).await;
            assert_eq!(servers.warnings.len(), 1, "{body}");
            assert!(servers.definitions().is_empty());
        }
        tokio::fs::write(
            directory.join(MCP_FILE),
            r#"{"servers":[{"name":"off","command":["blackwall-missing-mcp-server"],"enabled":false}]}"#,
        )
        .await
        .unwrap();
        let disabled = McpServers::start(&directory, &[]).await;
        assert!(disabled.warnings.is_empty() && disabled.definitions().is_empty());
        tokio::fs::remove_dir_all(&directory).await.unwrap();
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
