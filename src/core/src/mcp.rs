//! Owner-configured local MCP servers whose tools join Agent runs behind Blackwall approvals.
use crate::{
    approvals::Approvals,
    protocol::{AgentEvent, ApprovalKind},
    tools::ToolError,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    sync::Mutex,
};

pub const MCP_FILE: &str = "mcp.json";
const MAX_FILE_BYTES: usize = 64 * 1024;
const MAX_SERVERS: usize = 16;
const MAX_TOOLS: usize = 128;
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const PROTOCOL_VERSION: &str = "2025-06-18";

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
    use std::io::Write;
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
    let temporary = directory.join(format!(".mcp-{:016x}", rand::random::<u64>()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let result = options
        .open(&temporary)
        .and_then(|mut file| file.write_all(&body).and_then(|()| file.sync_all()))
        .and_then(|()| std::fs::rename(&temporary, &path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|_| failed)
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
            match tokio::time::timeout(STARTUP_TIMEOUT, launch(&config, directory)).await {
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
        if !approvals
            .request(request_id, ApprovalKind::Exec, &tool.exposed, detail, emit)
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
            Ok(result) => result?,
            Err(_) => {
                // A late reply would arrive out of order, so the server is stopped and not reused.
                connection.broken = true;
                #[cfg(any(unix, windows))]
                connection.process.take();
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
    reader: Box<dyn AsyncBufRead + Send + Unpin>,
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    next_id: u64,
    broken: bool,
    #[cfg(any(unix, windows))]
    process: Option<crate::tools::ServerProcess>,
}

impl Connection {
    async fn initialize(&mut self) -> Result<(), ToolError> {
        let result = self
            .request(
            "initialize",
            json!({"protocolVersion":PROTOCOL_VERSION,"capabilities":{},"clientInfo":{"name":"Blackwall","version":env!("CARGO_PKG_VERSION")}}),
        )
        .await?;
        // Tool discovery and calls are unchanged across these versions.
        if !["2025-06-18", "2025-03-26", "2024-11-05"]
            .contains(&result["protocolVersion"].as_str().unwrap_or_default())
        {
            return Err(ToolError::Execution(
                "The MCP server uses an unsupported protocol version.".into(),
            ));
        }
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
        let unavailable = || ToolError::Execution("The MCP server stopped responding.".into());
        let mut line = serde_json::to_vec(message).map_err(|_| ToolError::Arguments)?;
        line.push(b'\n');
        self.writer
            .write_all(&line)
            .await
            .map_err(|_| unavailable())?;
        self.writer.flush().await.map_err(|_| unavailable())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, ToolError> {
        let unavailable = || ToolError::Execution("The MCP server stopped responding.".into());
        if self.broken {
            return Err(unavailable());
        }
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        loop {
            let message = match read_message(&mut self.reader).await {
                Ok(Some(message)) => message,
                _ => {
                    self.broken = true;
                    return Err(unavailable());
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

/// One bounded line of JSON. Blank and non-JSON lines are skipped.
async fn read_message(
    reader: &mut (dyn AsyncBufRead + Send + Unpin),
) -> std::io::Result<Option<Value>> {
    loop {
        let mut line = vec![];
        loop {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return Ok(None);
            }
            let (chunk, done) = match available.iter().position(|byte| *byte == b'\n') {
                Some(end) => (&available[..end], true),
                None => (available, false),
            };
            if line.len() + chunk.len() > MAX_MESSAGE_BYTES {
                return Err(std::io::ErrorKind::InvalidData.into());
            }
            line.extend_from_slice(chunk);
            let used = chunk.len() + usize::from(done);
            reader.consume(used);
            if done {
                break;
            }
        }
        if let Ok(message) = serde_json::from_slice::<Value>(&line) {
            if message.is_object() {
                return Ok(Some(message));
            }
        }
    }
}

/// Servers start in the data folder so a project cannot shadow what a package runner loads.
async fn launch(config: &ServerConfig, directory: &Path) -> Result<Connection, ToolError> {
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
            crate::tools::spawn_server(directory, &program, &config.command[1..])?;
        // Server logs are not shown; draining them keeps a chatty server from blocking.
        tokio::spawn(async move {
            let mut stderr = stderr;
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
        });
        let mut connection = Connection {
            reader: Box::new(BufReader::new(stdout)),
            writer: Box::new(stdin),
            next_id: 0,
            broken: false,
            process: Some(process),
        };
        connection.initialize().await?;
        Ok(connection)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (config, directory);
        Err(ToolError::Execution(
            "MCP servers are not supported on this platform.".into(),
        ))
    }
}

fn valid_server_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Model tool names must stay within the common function-name alphabet and length.
fn valid_tool_name(name: &str) -> bool {
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
        if !valid_server_name(&server.name)
            || servers[..index]
                .iter()
                .any(|other| other.name == server.name)
            || server.command.is_empty()
            || server.command.len() > 64
            || !crate::command_tools::valid_program(&server.command[0])
        {
            return Err("An MCP server has an invalid name or command.");
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
        Connection {
            reader: Box::new(BufReader::new(client_read)),
            writer: Box::new(client_write),
            next_id: 0,
            broken: false,
            #[cfg(any(unix, windows))]
            process: None,
        }
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
        let mut connection = Connection {
            reader: Box::new(reply),
            writer: Box::new(tokio::io::sink()),
            next_id: 0,
            broken: false,
            #[cfg(any(unix, windows))]
            process: None,
        };
        assert!(connection.initialize().await.is_err());
    }
}
