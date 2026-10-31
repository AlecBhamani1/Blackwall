//! Owner-defined tools that run a fixed program with model-supplied arguments, without a shell,
//! or send one HTTP request to a fixed origin.
use crate::{
    approvals::Approvals,
    protocol::{AgentEvent, ApprovalKind},
    tools::ToolError,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

mod editor;
mod http;
pub use editor::{detect_clis, remove_tool, save_tool, summaries, CliStatus, ToolSummary};
pub use http::{delete_secret, save_secret, save_secret_with, SECRET_SERVICE};

pub const COMMAND_TOOLS_FILE: &str = "tools.json";
const MAX_FILE_BYTES: usize = 64 * 1024;
const MAX_TOOLS: usize = 20;
const MAX_ARGUMENTS: usize = 64;
const MAX_VALUE_BYTES: usize = 8000;
/// The JSON written to a `stdin: "json"` tool, so a long prompt is not limited by argv.
const MAX_INPUT_BYTES: usize = 256 * 1024;
const MAX_PROPERTIES: usize = 64;
const MAX_CHOICES: usize = 64;
/// How much of standard input or a request body the approval prompt shows.
const MAX_SHOWN_INPUT: usize = 2000;
const BUILT_IN: [&str; 13] = [
    "read_file",
    "list_files",
    "search_files",
    "write_file",
    "shell",
    "web_fetch",
    "web_search",
    "spawn_agent",
    "memory_list",
    "memory_propose",
    "session_recall",
    "read_skill",
    "skill_propose",
];

/// What an approval for an owner command tool that runs outside the sandbox says first.
pub const OUTSIDE_NOTE: &str =
    "Runs outside the sandbox with your account permissions. It can run code from this project and use your files and network.";

/// Where the owner's command tools run: outside the sandbox, always asking (the default), or
/// inside the Commands profile. Only the owner chooses; a project file cannot.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    #[default]
    Outside,
    Sandboxed,
}

impl Isolation {
    pub fn name(self) -> &'static str {
        match self {
            Self::Outside => "outside",
            Self::Sandboxed => "sandboxed",
        }
    }
}

impl std::str::FromStr for Isolation {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "outside" => Ok(Self::Outside),
            "sandboxed" => Ok(Self::Sandboxed),
            _ => Err("Choose outside or sandboxed."),
        }
    }
}

/// The desktop's saved `commandToolsIsolation`; a missing one is the default.
pub fn isolation_from_preferences(
    preferences: &Map<String, Value>,
) -> Result<Isolation, &'static str> {
    preferences
        .get("commandToolsIsolation")
        .map(|value| {
            value
                .as_str()
                .ok_or("Choose outside or sandboxed.")
                .and_then(str::parse)
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

/// What a tool does when called: run a program (the default) or send an HTTP request.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    #[default]
    Command,
    Http,
}

/// What a command tool's standard input holds: nothing (the default) or `{"arguments":{…}}`.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StdinMode {
    #[default]
    None,
    Json,
}

/// An HTTP tool's request body: none (the default) or the arguments as a JSON object.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BodyMode {
    #[default]
    None,
    Json,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    #[default]
    Get,
    Post,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandTool {
    name: String,
    description: String,
    #[serde(default)]
    kind: ToolKind,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    stdin: StdinMode,
    /// The owner's argument schema. Without one, every placeholder is a required string.
    #[serde(default)]
    parameters: Option<Schema>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    method: Option<Method>,
    /// Header names and `{secret:NAME}` references; values are in the credential store.
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: BodyMode,
    /// Names of the owner's environment variables the program also receives, when it runs
    /// unsandboxed. A sandboxed program never gets them.
    #[serde(default)]
    env: Vec<String>,
}

/// The JSON Schema subset owners can declare: an object of typed, optionally enumerated values.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Schema {
    #[serde(rename = "type")]
    kind: ObjectType,
    #[serde(default)]
    properties: BTreeMap<String, Property>,
    #[serde(default)]
    required: Vec<String>,
    /// Only `false` is accepted; arguments the schema does not name are always refused.
    #[serde(default)]
    additional_properties: Option<bool>,
}

#[derive(Debug, Deserialize)]
enum ObjectType {
    #[serde(rename = "object")]
    Object,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Property {
    #[serde(rename = "type")]
    kind: ValueType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(rename = "enum", default, skip_serializing_if = "Option::is_none")]
    choices: Option<Vec<Value>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ValueType {
    String,
    Number,
    Integer,
    Boolean,
}

impl ValueType {
    fn accepts(self, value: &Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Number => value.is_number(),
            // JSON has one number type, so a whole-number float such as `3.0` is an integer.
            Self::Integer => {
                value.is_i64()
                    || value.is_u64()
                    || value
                        .as_f64()
                        .is_some_and(|n| n.is_finite() && n.fract() == 0.0)
            }
            Self::Boolean => value.is_boolean(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandToolFile {
    tools: Vec<CommandTool>,
}

fn parse_file(body: &[u8]) -> Result<Vec<CommandTool>, &'static str> {
    if body.len() > MAX_FILE_BYTES {
        return Err("The tools file is too large.");
    }
    serde_json::from_slice::<CommandToolFile>(body)
        .map_err(|_| "The tools file is not valid.")
        .and_then(|file| validate(file.tools))
}

/// Tools loaded for one run. A missing file has no tools; an invalid file has no tools and a
/// warning, so a mistake never exposes a partial configuration.
#[derive(Debug, Default)]
pub struct CommandTools {
    tools: Vec<CommandTool>,
    pub error: Option<String>,
}

impl CommandTools {
    pub async fn load(directory: &Path) -> Self {
        let path = directory.join(COMMAND_TOOLS_FILE);
        let read = crate::policy::read_bounded(&path, MAX_FILE_BYTES);
        let parsed = match tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into()))
        {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(_) => Err("The tools file could not be read."),
            Ok(body) => parse_file(&body),
        };
        match parsed {
            Ok(tools) => Self { tools, error: None },
            Err(error) => Self {
                tools: vec![],
                error: Some(format!("{error} Owner tools are unavailable for this run.")),
            },
        }
    }

    pub fn definitions(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|tool| {
                json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.parameters_json()}})
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&CommandTool> {
        self.tools.iter().find(|tool| tool.name == name)
    }

    /// The names of the tools a subagent can run on, with `stdin: "json"` and a `prompt`.
    pub fn delegates(&self) -> Vec<&str> {
        self.tools
            .iter()
            .filter(|tool| tool.delegate_problem().is_none())
            .map(|tool| tool.name.as_str())
            .collect()
    }
}

/// Check that the owner's `tools.json` in `directory` has a tool named `name` that a subagent
/// can run on. Used when the owner chooses it, so a mistake shows before a run needs it.
pub fn check_delegate(directory: &Path, name: &str) -> Result<(), String> {
    let mut body = vec![];
    let read = std::fs::File::open(directory.join(COMMAND_TOOLS_FILE)).and_then(|file| {
        use std::io::Read;
        file.take(MAX_FILE_BYTES as u64 + 1).read_to_end(&mut body)
    });
    let tools = match read {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(_) => return Err("The tools file could not be read.".into()),
        Ok(_) => parse_file(&body).map_err(str::to_owned)?,
    };
    let tool = tools
        .iter()
        .find(|tool| tool.name == name)
        .ok_or_else(|| format!("There is no command tool named {name} in {COMMAND_TOOLS_FILE}."))?;
    match tool.delegate_problem() {
        Some(problem) => Err(problem.into()),
        None => Ok(()),
    }
}

/// Check `args` against an owner schema: an object with only declared names, every required
/// name present, and each value of its declared type and, when listed, one of its choices.
pub fn validate_arguments(schema: &Schema, args: &Value) -> Result<(), ToolError> {
    let object = args.as_object().ok_or(ToolError::Arguments)?;
    if schema
        .required
        .iter()
        .any(|name| !object.contains_key(name))
    {
        return Err(ToolError::Arguments);
    }
    for (name, value) in object {
        let property = schema.properties.get(name).ok_or(ToolError::Arguments)?;
        if !property.kind.accepts(value)
            || property
                .choices
                .as_ref()
                .is_some_and(|choices| !choices.contains(value))
        {
            return Err(ToolError::Arguments);
        }
    }
    Ok(())
}

impl CommandTool {
    /// The approval kind a call asks with: Network for HTTP tools, Exec for programs.
    pub fn approval_kind(&self) -> ApprovalKind {
        match self.kind {
            ToolKind::Command => ApprovalKind::Exec,
            ToolKind::Http => ApprovalKind::Network,
        }
    }

    /// Placeholder names in first-use order, each once: argv placeholders, or URL placeholders.
    fn placeholders(&self) -> Vec<&str> {
        let names: Vec<&str> = match self.kind {
            ToolKind::Command => self
                .command
                .iter()
                .filter_map(|arg| placeholder(arg))
                .collect(),
            ToolKind::Http => self
                .url
                .as_deref()
                .and_then(|url| http::Template::parse(url).ok())
                .map(|template| template.names)
                .unwrap_or_default(),
        };
        let mut seen = std::collections::HashSet::new();
        names
            .into_iter()
            .filter(|name| seen.insert(*name))
            .collect()
    }

    fn parameters_json(&self) -> Value {
        match &self.parameters {
            Some(schema) => {
                json!({"type":"object","properties":schema.properties,"required":schema.required,"additionalProperties":false})
            }
            None => {
                // A placeholder may repeat; the schema lists each name once.
                let parameters = self.placeholders();
                let properties: Map<_, _> = parameters
                    .iter()
                    .map(|name| ((*name).to_owned(), json!({"type":"string"})))
                    .collect();
                json!({"type":"object","properties":properties,"required":parameters,"additionalProperties":false})
            }
        }
    }

    /// The text a placeholder becomes. Without an owner schema only strings are accepted, as
    /// before schemas existed; with one, numbers and booleans are written as JSON writes them.
    fn value_text(&self, values: &Value, name: &str) -> Result<String, ToolError> {
        let integer = self.parameters.as_ref().is_some_and(|schema| {
            schema
                .properties
                .get(name)
                .is_some_and(|property| property.kind == ValueType::Integer)
        });
        let text = match values.get(name) {
            Some(Value::String(text)) => text.clone(),
            // `3.0` for an integer is written `3`, as the owner's program or service expects.
            Some(Value::Number(number)) if integer && !number.is_i64() && !number.is_u64() => {
                format!("{:.0}", number.as_f64().ok_or(ToolError::Arguments)?)
            }
            Some(value @ (Value::Number(_) | Value::Bool(_))) if self.parameters.is_some() => {
                value.to_string()
            }
            _ => return Err(ToolError::Arguments),
        };
        if text.len() > MAX_VALUE_BYTES {
            return Err(ToolError::Arguments);
        }
        Ok(text)
    }

    /// Command rules match the configured command template, so, for a sandboxed tool,
    /// `claude -p` can allow every prompt. A tool outside the sandbox always asks; only deny
    /// rules apply. HTTP tools ask with kind Network, and rules match the request's host.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &self,
        workspace: &crate::tools::Workspace,
        arguments: &str,
        request_id: &str,
        approvals: &Approvals,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
        timeout: Duration,
        output: &crate::tools::OutputCap,
    ) -> Result<String, ToolError> {
        let keychain: http::Lookup =
            std::sync::Arc::new(|account: &str| crate::secrets::get(SECRET_SERVICE, account));
        self.run_with(
            workspace, arguments, request_id, approvals, emit, timeout, output, keychain,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_with(
        &self,
        workspace: &crate::tools::Workspace,
        arguments: &str,
        request_id: &str,
        approvals: &Approvals,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
        timeout: Duration,
        output: &crate::tools::OutputCap,
        secrets: http::Lookup,
    ) -> Result<String, ToolError> {
        let values: Value = serde_json::from_str(arguments).map_err(|_| ToolError::Arguments)?;
        match &self.parameters {
            Some(schema) => validate_arguments(schema, &values)?,
            // Arguments sent whole, on stdin or as a body, follow the placeholder schema exactly;
            // argv-only tools still ignore names they do not use, as before schemas existed.
            None if self.stdin == StdinMode::Json || self.body == BodyMode::Json => {
                let names = self.placeholders();
                let object = values.as_object().ok_or(ToolError::Arguments)?;
                if names.iter().any(|name| !object.contains_key(*name))
                    || object
                        .iter()
                        .any(|(name, value)| !value.is_string() || !names.contains(&name.as_str()))
                {
                    return Err(ToolError::Arguments);
                }
            }
            None => {}
        }
        if self.kind == ToolKind::Http {
            // Trimmed after the response is redacted, so a cut never splits a secret.
            return http::run(self, &values, request_id, approvals, emit, timeout, secrets)
                .await
                .map(|text| output.trim_text(text));
        }
        let (program, identity, args, input) = self.prepared(&values, &workspace.path)?;
        let outside = workspace.execution.command_tools == Isolation::Outside;
        let env = self.passed_env(outside);
        // JSON keeps distinct argument lists distinct, so a remembered approval stays exact.
        let mut detail = format!(
            "{}Project: {}\nTool: {}\nProgram: {}\n{}Arguments: {}",
            outside_prefix(outside),
            workspace.path.display(),
            self.name,
            program.display(),
            passed_line(&env),
            serde_json::to_string(&args).map_err(|_| ToolError::Arguments)?,
        );
        if let Some(input) = &input {
            detail.push_str(&format!("\nStandard input: JSON, {}", shown_bytes(input)));
        }
        if !outside {
            // Never ask to approve a command that can't run in the sandbox.
            crate::sandbox::ready(workspace).await?;
            detail.push_str(&format!("\n\n{}", workspace.execution.description()));
        }
        if !approvals
            .request_owner_tool(
                request_id,
                &self.command.join(" "),
                &self.call(workspace, &program, &args, &env, input.as_deref()),
                detail,
                false,
                outside,
                emit,
            )
            .await
            .map_err(ToolError::Execution)?
        {
            return Err(ToolError::Denied);
        }
        self.unchanged(&program, &identity, &workspace.path)?;
        crate::tools::program_with_timeout(
            workspace, &program, &args, &self.env, input, timeout, output,
        )
        .await
    }

    /// Why this tool cannot take a subagent's prompt, or `None` when it can: it must run a
    /// program, read `{"arguments":{"prompt":…}}` from standard input, and accept a `prompt`
    /// string as its only required argument.
    pub fn delegate_problem(&self) -> Option<&'static str> {
        if self.kind != ToolKind::Command {
            return Some("A subagent can only run on a command tool, not an HTTP tool.");
        }
        if self.stdin != StdinMode::Json {
            return Some("A subagent tool must set \"stdin\": \"json\", so the prompt is sent on standard input.");
        }
        // The prompt never goes on the command line, where its size is limited and other
        // programs on this computer can read it.
        let accepts = self.placeholders().is_empty()
            && self.parameters.as_ref().is_some_and(|schema| {
                schema
                    .properties
                    .get("prompt")
                    .is_some_and(|property| property.kind == ValueType::String)
                    && schema.required.iter().all(|name| name == "prompt")
            });
        (!accepts).then_some(
            "A subagent tool must declare a string \"prompt\" in \"parameters\", need no other argument, and use no {placeholders} in its command.",
        )
    }

    /// Run this tool as a subagent: `prompt` goes to it as `{"arguments":{"prompt":…}}` on
    /// standard input. This always asks, the answer is never remembered, the owner sees the
    /// whole prompt, and an allow rule never skips the prompt. Deny rules still apply.
    #[allow(clippy::too_many_arguments)]
    pub async fn delegate(
        &self,
        workspace: &crate::tools::Workspace,
        prompt: &str,
        request_id: &str,
        approvals: &Approvals,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
        timeout: Duration,
        output: &crate::tools::OutputCap,
    ) -> Result<String, ToolError> {
        if let Some(problem) = self.delegate_problem() {
            return Err(ToolError::Execution(problem.into()));
        }
        let values = json!({ "prompt": prompt });
        if let Some(schema) = &self.parameters {
            validate_arguments(schema, &values)?;
        }
        let (program, identity, args, input) = self.prepared(&values, &workspace.path)?;
        let outside = workspace.execution.command_tools == Isolation::Outside;
        let env = self.passed_env(outside);
        let mut detail = format!(
            "{}A subagent wants to send its task to another program. It may send this prompt off this device.\n\nProject: {}\nTool: {}\nProgram: {}\n{}Arguments: {}\nPrompt ({} bytes, on standard input):\n{prompt}",
            outside_prefix(outside),
            workspace.path.display(),
            self.name,
            program.display(),
            passed_line(&env),
            serde_json::to_string(&args).map_err(|_| ToolError::Arguments)?,
            prompt.len(),
        );
        if !outside {
            // Never ask to approve a command that can't run in the sandbox.
            crate::sandbox::ready(workspace).await?;
            detail.push_str(&format!("\n\n{}", workspace.execution.description()));
        }
        if !approvals
            .request_owner_tool(
                request_id,
                &self.command.join(" "),
                &self.call(workspace, &program, &args, &env, input.as_deref()),
                detail,
                true,
                outside,
                emit,
            )
            .await
            .map_err(ToolError::Execution)?
        {
            return Err(ToolError::Denied);
        }
        self.unchanged(&program, &identity, &workspace.path)?;
        crate::tools::program_with_timeout(
            workspace, &program, &args, &self.env, input, timeout, output,
        )
        .await
    }

    /// What the audit records of a call to this tool.
    fn call<'a>(
        &'a self,
        workspace: &crate::tools::Workspace,
        program: &'a Path,
        args: &'a [String],
        env: &'a [String],
        input: Option<&[u8]>,
    ) -> crate::policy::ProgramCall<'a> {
        crate::policy::ProgramCall {
            program,
            tool: &self.name,
            isolation: workspace.execution.command_tools,
            args,
            env,
            stdin_sha256: input.map(sha256_hex),
            overridden: false,
        }
    }

    /// The `env` names the program gets: those set in Blackwall's environment, when it runs
    /// outside the sandbox. A sandboxed program gets none.
    fn passed_env(&self, outside: bool) -> Vec<String> {
        match outside {
            true => crate::child_env::passed_through(&self.env),
            false => Vec::new(),
        }
    }

    /// Refuses to run when `program` is no longer the file the prompt named: the configured
    /// name now resolves elsewhere, or the file was replaced or rewritten while the owner was
    /// asked. Checked just before the program starts, after any wait for an answer.
    fn unchanged(
        &self,
        program: &Path,
        identity: &ProgramIdentity,
        project: &Path,
    ) -> Result<(), ToolError> {
        let same = resolve_program(&self.command[0], project).is_ok_and(|again| again == program)
            && ProgramIdentity::of(program).as_ref() == Some(identity);
        match same {
            true => Ok(()),
            false => Err(ToolError::Execution(format!(
                "{} changed after you approved it. The command was not run.",
                program.display()
            ))),
        }
    }

    /// The resolved program, its identity, its arguments, and its standard input for checked
    /// `values`.
    fn prepared(&self, values: &Value, project: &Path) -> Result<Prepared, ToolError> {
        let args = self.command[1..]
            .iter()
            .map(|arg| match placeholder(arg) {
                Some(name) => {
                    self.value_text(values, name)
                        .and_then(|value| match is_option(&value) {
                            true => Err(ToolError::Arguments),
                            false => Ok(value),
                        })
                }
                None => Ok(arg.clone()),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let input = match self.stdin {
            StdinMode::None => None,
            StdinMode::Json => {
                let input = serde_json::to_vec(&json!({ "arguments": values }))
                    .map_err(|_| ToolError::Arguments)?;
                if input.len() > MAX_INPUT_BYTES {
                    return Err(ToolError::Arguments);
                }
                Some(input)
            }
        };
        let program = resolve_program(&self.command[0], project)?;
        let identity = ProgramIdentity::of(&program).ok_or_else(|| {
            ToolError::Execution(not_resolved_message(&self.command[0], NotResolved::Missing))
        })?;
        Ok((program, identity, args, input))
    }
}

/// What an owner tool's approval detail starts with: [`OUTSIDE_NOTE`] when it runs outside the
/// sandbox, where the sandbox description would be false.
fn outside_prefix(outside: bool) -> String {
    match outside {
        true => format!("{OUTSIDE_NOTE}\n\n"),
        false => String::new(),
    }
}

/// The approval line naming the variables a program gets from the owner's environment, never
/// their values; empty when it gets none.
fn passed_line(names: &[String]) -> String {
    match names.is_empty() {
        true => String::new(),
        false => format!(
            "Gets these variables from your environment: {}\n",
            names.join(", ")
        ),
    }
}

/// A command tool's resolved program, its identity, its arguments, and its standard input.
type Prepared = (PathBuf, ProgramIdentity, Vec<String>, Option<Vec<u8>>);

/// The file a resolved program names when its prompt is built: where it really is (through
/// symlinks), and its size and modification time, plus its device, inode and change time on Unix.
/// A file renamed over it, a retargeted link, or a rewrite in place changes it.
#[derive(Debug, PartialEq, Eq)]
struct ProgramIdentity {
    canonical: PathBuf,
    size: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    inode: (u64, u64, i64, i64),
}

impl ProgramIdentity {
    fn of(program: &Path) -> Option<Self> {
        let canonical = std::fs::canonicalize(program).ok()?;
        let meta = std::fs::metadata(&canonical).ok()?;
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec())
        };
        Some(Self {
            canonical,
            size: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            inode,
        })
    }
}

/// Size, digest, and the start of a payload for an approval prompt. The digest keeps a
/// remembered approval exact when the shown text is clipped.
fn shown_bytes(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut shown: String = text.chars().take(MAX_SHOWN_INPUT).collect();
    if shown.len() < text.len() {
        shown.push('…');
    }
    format!(
        "{} bytes, sha256 {}\n{shown}",
        bytes.len(),
        sha256_hex(bytes)
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A value that could be read as an option the owner did not configure.
fn is_option(value: &str) -> bool {
    value.starts_with('-') || (cfg!(windows) && value.starts_with('/'))
}

/// An absolute program outside `project`, or a bare name found in the child's validated `PATH`:
/// absolute entries outside the AppImage mount and `project`, whose directory also does not
/// resolve (through symlinks) into `project`. An absolute program that is, or resolves through
/// symlinks into, `project` is refused. A repository can therefore never plant the program that
/// an owner command tool or MCP server starts. On Windows the `.exe` is resolved here, so the
/// approved path is exactly the one that runs.
pub(crate) fn resolve_program(program: &str, project: &Path) -> Result<PathBuf, ToolError> {
    let project = std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
    let appdir = std::env::var_os("APPDIR").map(PathBuf::from);
    resolve_in(
        program,
        crate::tools::search_path().as_deref(),
        crate::child_env::valid_appdir(appdir.as_deref()),
        &project,
    )
    .map_err(|why| ToolError::Execution(not_resolved_message(program, why)))
}

fn not_resolved_message(program: &str, why: NotResolved) -> String {
    match why {
        NotResolved::Missing => format!("{program} was not found on this computer."),
        NotResolved::OnlyInProject => format!(
            "{program} was only found inside the project folder, which Blackwall never runs programs from. Install it outside the project."
        ),
        NotResolved::InsideProject => format!(
            "{program} is inside the project folder, which Blackwall never runs programs from."
        ),
    }
}

/// Why a program name did not resolve.
#[derive(Debug, PartialEq, Eq)]
enum NotResolved {
    Missing,
    /// Every match was in a `PATH` directory inside the project.
    OnlyInProject,
    /// An absolute program that is, or resolves through symlinks into, the project.
    InsideProject,
}

fn resolve_in(
    program: &str,
    search_path: Option<&std::ffi::OsStr>,
    appdir: Option<&Path>,
    project: &Path,
) -> Result<PathBuf, NotResolved> {
    let name = if cfg!(windows) && Path::new(program).extension().is_none() {
        format!("{program}.exe")
    } else {
        program.to_owned()
    };
    let path = Path::new(&name);
    if path.is_absolute() {
        return if !executable(path) {
            Err(NotResolved::Missing)
        } else if crate::child_env::inside_project(path, project) {
            Err(NotResolved::InsideProject)
        } else {
            Ok(path.to_path_buf())
        };
    }
    let search_path = search_path.ok_or(NotResolved::Missing)?;
    // `validated_path` already drops directories that are, or resolve into, the project.
    let found = crate::child_env::validated_path(search_path, appdir, project)
        .into_iter()
        .flat_map(|directories| std::env::split_paths(&directories).collect::<Vec<_>>())
        .map(|directory| directory.join(&name))
        .find(|candidate| executable(candidate));
    found.ok_or_else(|| {
        let planted = std::env::split_paths(search_path).any(|directory| {
            directory.is_absolute()
                && executable(&directory.join(&name))
                && crate::child_env::inside_project(&directory, project)
        });
        if planted {
            NotResolved::OnlyInProject
        } else {
            NotResolved::Missing
        }
    })
}

fn executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

pub(crate) fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Only whole arguments such as `{prompt}` are placeholders; text is never spliced into one.
fn placeholder(arg: &str) -> Option<&str> {
    arg.strip_prefix('{')?
        .strip_suffix('}')
        .filter(|name| valid_name(name))
}

/// A bare name or absolute path. On Windows only `.exe` programs are accepted: batch files run
/// through `cmd.exe`, which expands their arguments in ways separate arguments cannot make safe.
pub(crate) fn valid_program(program: &str) -> bool {
    let path = Path::new(program);
    let extension = path.extension().and_then(|extension| extension.to_str());
    let batch = extension.is_some_and(|extension| {
        extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd")
    });
    let windows_exe = extension.is_none_or(|extension| extension.eq_ignore_ascii_case("exe"));
    !program.trim().is_empty()
        && !program.ends_with(['.', ' '])
        && placeholder(program).is_none()
        && !batch
        && (!cfg!(windows) || windows_exe)
        && (path.is_absolute() || !program.contains(['/', '\\', ':']))
}

fn validate(tools: Vec<CommandTool>) -> Result<Vec<CommandTool>, &'static str> {
    if tools.len() > MAX_TOOLS {
        return Err("The tools file has too many tools.");
    }
    for (index, tool) in tools.iter().enumerate() {
        if !valid_name(&tool.name)
            || BUILT_IN.contains(&tool.name.as_str())
            || tools[..index].iter().any(|other| other.name == tool.name)
            || tool.description.trim().is_empty()
            || tool.description.len() > 1024
        {
            return Err("A tool has an invalid name, description, or command.");
        }
        crate::child_env::valid_pass_through_list(&tool.env)?;
        match tool.kind {
            ToolKind::Command => {
                if tool.command.is_empty()
                    || tool.command.len() > MAX_ARGUMENTS
                    || !valid_program(&tool.command[0])
                {
                    return Err("A tool has an invalid name, description, or command.");
                }
                if tool.url.is_some()
                    || tool.method.is_some()
                    || !tool.headers.is_empty()
                    || tool.body != BodyMode::None
                {
                    return Err("Only HTTP tools take a url, method, headers, or body.");
                }
            }
            ToolKind::Http => {
                if !tool.env.is_empty() {
                    return Err("Only command tools take env.");
                }
                http::validate(tool)?
            }
        }
        if let Some(schema) = &tool.parameters {
            validate_schema(schema)?;
            // Every placeholder needs a value, so each must be a required parameter.
            if tool
                .placeholders()
                .iter()
                .any(|name| !schema.required.iter().any(|required| required == name))
            {
                return Err("Every placeholder must be a required parameter.");
            }
        }
    }
    Ok(tools)
}

fn validate_schema(schema: &Schema) -> Result<(), &'static str> {
    const INVALID: &str = "A tool has invalid parameters.";
    let ObjectType::Object = schema.kind;
    if schema.additional_properties == Some(true)
        || schema.properties.len() > MAX_PROPERTIES
        || schema.required.iter().enumerate().any(|(index, name)| {
            !schema.properties.contains_key(name) || schema.required[..index].contains(name)
        })
    {
        return Err(INVALID);
    }
    for (name, property) in &schema.properties {
        let choices_valid = property.choices.as_ref().is_none_or(|choices| {
            !choices.is_empty()
                && choices.len() <= MAX_CHOICES
                && choices.iter().all(|choice| property.kind.accepts(choice))
        });
        if !valid_name(name)
            || property
                .description
                .as_ref()
                .is_some_and(|description| description.len() > 1024)
            || !choices_valid
        {
            return Err(INVALID);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn isolation_defaults_outside_and_parses_strictly() {
        assert_eq!(Isolation::default(), Isolation::Outside);
        assert_eq!("sandboxed".parse::<Isolation>(), Ok(Isolation::Sandboxed));
        assert_eq!("outside".parse::<Isolation>(), Ok(Isolation::Outside));
        assert!("inside".parse::<Isolation>().is_err());
        assert_eq!(Isolation::Sandboxed.name(), "sandboxed");
        assert_eq!(
            isolation_from_preferences(&json!({}).as_object().unwrap().clone()),
            Ok(Isolation::Outside)
        );
        let saved = json!({"commandToolsIsolation":"sandboxed"});
        assert_eq!(
            isolation_from_preferences(saved.as_object().unwrap()),
            Ok(Isolation::Sandboxed)
        );
        for bad in [json!("inside"), json!(true), json!(null)] {
            let saved = json!({ "commandToolsIsolation": bad });
            assert!(isolation_from_preferences(saved.as_object().unwrap()).is_err());
        }
    }

    /// A fixture directory under the temp folder with a `bin` that holds an executable `tool`.
    #[cfg(unix)]
    fn plant(root: &Path, dir: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = root.join(dir);
        std::fs::create_dir_all(&bin).unwrap();
        let tool = bin.join("tool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[cfg(unix)]
    #[test]
    fn bare_names_skip_project_directories_on_path_even_through_symlinks() {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("blackwall-resolve-{}", rand::random::<u64>()));
        let project = root.join("project");
        let planted = plant(&project, "node_modules/.bin");
        let system = plant(&root, "system/bin");
        std::os::unix::fs::symlink(&planted, root.join("alias")).unwrap();
        let join = |dirs: &[&Path]| std::env::join_paths(dirs).unwrap();
        let find =
            |program: &str, dirs: &[&Path]| resolve_in(program, Some(&join(dirs)), None, &project);
        // The project directory is on PATH first, but the system one is chosen.
        assert_eq!(find("tool", &[&planted, &system]), Ok(system.join("tool")));
        // A symlink that leads back into the project does not count either.
        assert_eq!(
            find("tool", &[&root.join("alias"), &system]),
            Ok(system.join("tool"))
        );
        // With only project entries, nothing is found, and the error says why.
        assert_eq!(
            find("tool", &[&planted, &root.join("alias")]),
            Err(NotResolved::OnlyInProject)
        );
        assert_eq!(find("absent", &[&system]), Err(NotResolved::Missing));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn absolute_programs_inside_the_project_are_refused_even_through_symlinks() {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("blackwall-resolve-{}", rand::random::<u64>()));
        let project = root.join("project");
        let planted = plant(&project, "node_modules/.bin");
        let system = plant(&root, "system/bin");
        // A folder outside the project that links into it, and a file link to a planted program.
        std::os::unix::fs::symlink(&planted, root.join("alias")).unwrap();
        std::os::unix::fs::symlink(planted.join("tool"), system.join("linked")).unwrap();
        let find = |program: &Path| resolve_in(program.to_str().unwrap(), None, None, &project);
        assert_eq!(find(&planted.join("tool")), Err(NotResolved::InsideProject));
        assert_eq!(
            find(&root.join("alias").join("tool")),
            Err(NotResolved::InsideProject)
        );
        assert_eq!(
            find(&system.join("linked")),
            Err(NotResolved::InsideProject)
        );
        assert_eq!(find(&system.join("tool")), Ok(system.join("tool")));
        let program = planted.join("tool");
        let error = resolve_program(program.to_str().unwrap(), &project)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(
                "is inside the project folder, which Blackwall never runs programs from."
            ),
            "{error}"
        );
        // A bare name found only in the project points the owner outside it, never into it.
        let error = format!(
            "{}",
            ToolError::Execution(not_resolved_message("tool", NotResolved::OnlyInProject))
        );
        assert!(!error.contains("absolute path"), "{error}");
        assert!(error.contains("Install it outside the project"), "{error}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn absolute_programs_resolve_without_any_search_path() {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("blackwall-resolve-{}", rand::random::<u64>()));
        let system = plant(&root, "system/bin");
        let tool = system.join("tool");
        assert_eq!(
            resolve_in(tool.to_str().unwrap(), None, None, Path::new("/p")),
            Ok(tool)
        );
        assert_eq!(
            resolve_in("tool", None, None, Path::new("/p")),
            Err(NotResolved::Missing)
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    async fn load(body: &str) -> (CommandTools, std::path::PathBuf) {
        let directory =
            std::env::temp_dir().join(format!("blackwall-tools-{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        tokio::fs::write(directory.join(COMMAND_TOOLS_FILE), body)
            .await
            .unwrap();
        (CommandTools::load(&directory).await, directory)
    }

    #[test]
    fn mcp_and_tools_files_reject_injection_names_in_env() {
        let tool = |env: &str, extra: &str| {
            format!(
                r#"{{"tools":[{{"name":"t","description":"d","command":["x"],"env":{env}{extra}}}]}}"#
            )
        };
        assert!(parse_file(tool(r#"["GITHUB_TOKEN"]"#, "").as_bytes()).is_ok());
        for env in [
            r#"["NODE_OPTIONS"]"#,
            r#"["LD_PRELOAD"]"#,
            r#"["PATH"]"#,
            r#"["A=B"]"#,
        ] {
            assert!(parse_file(tool(env, "").as_bytes()).is_err(), "{env}");
        }
        let many: Vec<String> = (0..33).map(|i| format!("V{i}")).collect();
        assert!(parse_file(tool(&serde_json::to_string(&many).unwrap(), "").as_bytes()).is_err());
        let http = |env: &str| {
            format!(
                r#"{{"tools":[{{"name":"t","description":"d","kind":"http","url":"https://example.com/x"{env}}}]}}"#
            )
        };
        assert!(parse_file(http("").as_bytes()).is_ok());
        assert!(parse_file(http(r#","env":["HOME"]"#).as_bytes()).is_err());
    }

    #[tokio::test]
    async fn tools_load_from_owner_data_and_reject_invalid_files() {
        let (tools, directory) = load(
            r#"{"tools":[{"name":"ask_claude","description":"Ask Claude","command":["claude","-p","{prompt}"]}]}"#,
        )
        .await;
        assert!(tools.error.is_none());
        let definition = &tools.definitions()[0]["function"];
        assert_eq!(definition["name"], "ask_claude");
        assert_eq!(definition["parameters"]["required"], json!(["prompt"]));
        tokio::fs::remove_dir_all(&directory).await.unwrap();
        let (repeated, directory) = load(
            r#"{"tools":[{"name":"compare","description":"Compare","command":["cmp","{a}","{b}","{a}"]}]}"#,
        )
        .await;
        assert_eq!(
            repeated.definitions()[0]["function"]["parameters"]["required"],
            json!(["a", "b"])
        );
        tokio::fs::remove_dir_all(&directory).await.unwrap();
        for body in [
            "not json",
            r#"{"tools":[],"extra":1}"#,
            r#"{"tools":[{"name":"shell","description":"x","command":["sh"]}]}"#,
            r#"{"tools":[{"name":"run","description":"x","command":["{program}"]}]}"#,
            r#"{"tools":[{"name":"Bad Name","description":"x","command":["ls"]}]}"#,
            r#"{"tools":[{"name":"a","description":"x","command":["ls"]},{"name":"a","description":"y","command":["ls"]}]}"#,
            r#"{"tools":[{"name":"local","description":"x","command":["./scripts/run"]}]}"#,
            r#"{"tools":[{"name":"batch","description":"x","command":["claude.CMD","{prompt}"]}]}"#,
            r#"{"tools":[{"name":"drive","description":"x","command":["C:tool.exe"]}]}"#,
            r#"{"tools":[{"name":"dotted","description":"x","command":["tool.cmd."]}]}"#,
        ] {
            let (tools, directory) = load(body).await;
            assert!(tools.error.is_some(), "{body}");
            assert!(tools.definitions().is_empty());
            tokio::fs::remove_dir_all(&directory).await.unwrap();
        }
        let missing = CommandTools::load(&std::env::temp_dir().join("blackwall-no-tools")).await;
        assert!(missing.error.is_none() && missing.definitions().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn arguments_are_passed_without_a_shell_and_options_are_rejected() {
        use crate::protocol::{ApprovalDecision, ResolveApprovalRequest};
        let (tools, directory) = load(
            r#"{"tools":[{"name":"echo_back","description":"Echo","command":["echo","{text}"]}]}"#,
        )
        .await;
        let tool = tools.get("echo_back").unwrap();
        let mut workspace = crate::tools::Workspace::open(&directory).unwrap();
        workspace.execution.profile = crate::sandbox::ExecutionProfile::Unsandboxed;
        let approvals = Approvals::default();
        let decisions = approvals.clone();
        let details = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let captured = details.clone();
        let allow = move |event| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                detail,
                ..
            } = event
            {
                captured.lock().unwrap().push(detail);
                decisions
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision: ApprovalDecision::Allow,
                    })
                    .unwrap();
            }
        };
        let output = tool
            .run(
                &workspace,
                r#"{"text":"hi; touch injected"}"#,
                "run",
                &approvals,
                &allow,
                Duration::from_secs(10),
                &Default::default(),
            )
            .await
            .unwrap();
        assert!(output.contains("hi; touch injected"));
        assert!(!directory.join("injected").exists());
        assert!(details.lock().unwrap()[0].contains(r#"Arguments: ["hi; touch injected"]"#));
        assert!(matches!(
            tool.run(
                &workspace,
                r#"{"text":"--version"}"#,
                "run",
                &approvals,
                &allow,
                Duration::from_secs(10),
                &Default::default(),
            )
            .await,
            Err(ToolError::Arguments)
        ));
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    /// A tool config whose program creates `tool-write` in the project folder.
    #[cfg(unix)]
    const TOUCH_TOOL: &str = r#"{"tools":[{"name":"touch_probe","description":"Touch","command":["/usr/bin/touch","tool-write"]}]}"#;
    #[cfg(windows)]
    const TOUCH_TOOL: &str = r#"{"tools":[{"name":"touch_probe","description":"Touch","command":["cmd.exe","/D","/C","cd>tool-cwd && echo x>tool-write"]}]}"#;

    /// Run a tool that creates `tool-write` in the project, approving its prompt.
    #[cfg(any(unix, windows))]
    async fn touch_probe(
        isolation: Isolation,
        profile: crate::sandbox::ExecutionProfile,
    ) -> (Result<String, ToolError>, PathBuf) {
        use crate::protocol::{ApprovalDecision, ResolveApprovalRequest};
        let (tools, directory) = load(TOUCH_TOOL).await;
        let mut workspace = crate::tools::Workspace::open(&directory).unwrap();
        workspace.execution.profile = profile;
        workspace.execution.command_tools = isolation;
        let approvals = Approvals::default();
        let decisions = approvals.clone();
        let allow = move |event| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } = event
            {
                decisions
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision: ApprovalDecision::Allow,
                    })
                    .unwrap();
            }
        };
        let result = tools
            .get("touch_probe")
            .unwrap()
            .run(
                &workspace,
                r#"{}"#,
                "run",
                &approvals,
                &allow,
                // PowerShell can take far longer than 5 seconds to cold-start on a CI runner.
                Duration::from_secs(if cfg!(windows) { 60 } else { 5 }),
                &Default::default(),
            )
            .await;
        (result, workspace.path.clone())
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn outside_tool_runs_even_when_shell_profile_is_read_only() {
        let (result, path) = touch_probe(
            Isolation::Outside,
            crate::sandbox::ExecutionProfile::ReadOnly,
        )
        .await;
        result.unwrap();
        assert!(path.join("tool-write").exists());
        #[cfg(windows)]
        {
            let reported = tokio::fs::read_to_string(path.join("tool-cwd"))
                .await
                .unwrap();
            assert_eq!(std::fs::canonicalize(reported.trim()).unwrap(), path);
        }
        tokio::fs::remove_dir_all(&path).await.unwrap();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandboxed_tool_failure_names_the_setting() {
        let (result, path) = touch_probe(
            Isolation::Sandboxed,
            crate::sandbox::ExecutionProfile::ReadOnly,
        )
        .await;
        let error = result.unwrap_err().to_string();
        assert!(error.contains("Operation not permitted"), "{error}");
        assert!(
            error.contains("This may have been blocked by the Read-only sandbox."),
            "{error}"
        );
        assert!(
            error.contains("Memory → Advanced · Subagents → Command tools"),
            "{error}"
        );
        assert!(!path.join("tool-write").exists());
        tokio::fs::remove_dir_all(&path).await.unwrap();
    }

    fn schema(value: Value) -> Schema {
        serde_json::from_value(value).unwrap()
    }

    #[tokio::test]
    async fn legacy_tools_json_loads_unchanged() {
        let (tools, directory) = load(
            r#"{"tools":[{"name":"ask_claude","description":"Ask Claude","command":["claude","-p","{prompt}","--model","{model}","{prompt}"]},{"name":"status","description":"Git status","command":["git","status"]}]}"#,
        )
        .await;
        assert!(tools.error.is_none());
        assert_eq!(
            tools.definitions(),
            vec![
                json!({"type":"function","function":{"name":"ask_claude","description":"Ask Claude","parameters":{"type":"object","properties":{"prompt":{"type":"string"},"model":{"type":"string"}},"required":["prompt","model"],"additionalProperties":false}}}),
                json!({"type":"function","function":{"name":"status","description":"Git status","parameters":{"type":"object","properties":{},"required":[],"additionalProperties":false}}}),
            ]
        );
        let tool = tools.get("ask_claude").unwrap();
        assert_eq!(tool.kind, ToolKind::Command);
        assert_eq!(tool.stdin, StdinMode::None);
        assert!(tool.parameters.is_none());
        tokio::fs::remove_dir_all(&directory).await.unwrap();
        // HTTP-only fields are refused on a command tool.
        let (tools, directory) = load(
            r#"{"tools":[{"name":"x","description":"x","command":["ls"],"url":"https://example.com/"}]}"#,
        )
        .await;
        assert!(tools.error.is_some());
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    #[test]
    fn schema_types_enforced() {
        let schema = schema(json!({"type":"object","properties":{
            "name":{"type":"string","description":"Who"},
            "count":{"type":"integer"},
            "ratio":{"type":"number"},
            "dry_run":{"type":"boolean"}
        },"required":["name"],"additionalProperties":false}));
        validate_schema(&schema).unwrap();
        assert!(validate_arguments(
            &schema,
            &json!({"name":"a","count":3,"ratio":0.5,"dry_run":true})
        )
        .is_ok());
        for arguments in [
            json!({"name":1}),
            json!({"name":"a","count":"3"}),
            json!({"name":"a","count":1.5}),
            json!({"name":"a","ratio":"0.5"}),
            json!({"name":"a","dry_run":"true"}),
            json!({"name":"a","extra":1}),
            json!(["a"]),
            json!(null),
        ] {
            assert!(
                matches!(
                    validate_arguments(&schema, &arguments),
                    Err(ToolError::Arguments)
                ),
                "{arguments}"
            );
        }
        // Only the subset is accepted.
        for parameters in [
            json!({"type":"object","properties":{"list":{"type":"array"}}}),
            json!({"type":"object","properties":{"a":{"type":"string","minLength":1}}}),
            json!({"type":"string"}),
            json!({"type":"object","properties":{},"additionalProperties":true}),
            json!({"type":"object","properties":{},"required":["missing"]}),
            json!({"type":"object","properties":{"Bad Name":{"type":"string"}}}),
        ] {
            let parsed = serde_json::from_value::<Schema>(parameters.clone());
            assert!(
                parsed.is_err() || validate_schema(&parsed.unwrap()).is_err(),
                "{parameters}"
            );
        }
    }

    #[tokio::test]
    async fn schema_enum_enforced() {
        let schema = schema(json!({"type":"object","properties":{
            "level":{"type":"string","enum":["low","high"]},
            "size":{"type":"integer","enum":[1,2]}
        },"required":["level"]}));
        validate_schema(&schema).unwrap();
        assert!(validate_arguments(&schema, &json!({"level":"low","size":2})).is_ok());
        assert!(validate_arguments(&schema, &json!({"level":"mid"})).is_err());
        assert!(validate_arguments(&schema, &json!({"level":"low","size":3})).is_err());
        // Choices must match the declared type and the list cannot be empty.
        for property in [
            json!({"type":"string","enum":[1]}),
            json!({"type":"string","enum":[]}),
        ] {
            let bad = schema_from_property(property.clone());
            assert!(validate_schema(&bad).is_err(), "{property}");
        }
        // The definition the model sees carries the owner's schema.
        let (tools, directory) = load(
            r#"{"tools":[{"name":"deploy","description":"Deploy","command":["deploy","{level}"],"parameters":{"type":"object","properties":{"level":{"type":"string","enum":["low","high"],"description":"How far"}},"required":["level"]}}]}"#,
        )
        .await;
        assert!(tools.error.is_none(), "{:?}", tools.error);
        assert_eq!(
            tools.definitions()[0]["function"]["parameters"],
            json!({"type":"object","properties":{"level":{"type":"string","description":"How far","enum":["low","high"]}},"required":["level"],"additionalProperties":false})
        );
        tokio::fs::remove_dir_all(&directory).await.unwrap();
        // A placeholder that is not a required parameter is refused.
        let (tools, directory) = load(
            r#"{"tools":[{"name":"deploy","description":"Deploy","command":["deploy","{level}"],"parameters":{"type":"object","properties":{"level":{"type":"string"}}}}]}"#,
        )
        .await;
        assert!(tools.error.is_some());
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    fn schema_from_property(property: Value) -> Schema {
        schema(json!({"type":"object","properties":{"a":property}}))
    }

    #[tokio::test]
    async fn required_missing_is_arguments_error() {
        let (tools, directory) = load(
            r#"{"tools":[{"name":"ask","description":"Ask","command":["cat"],"stdin":"json","parameters":{"type":"object","properties":{"prompt":{"type":"string"},"model":{"type":"string"}},"required":["prompt"]}}]}"#,
        )
        .await;
        assert!(tools.error.is_none(), "{:?}", tools.error);
        let workspace = crate::tools::Workspace::open(&directory).unwrap();
        let approvals = Approvals::default();
        let never = |_: AgentEvent| panic!("invalid arguments never ask for approval");
        for arguments in [r#"{"model":"x"}"#, r#"{}"#, "not json"] {
            assert!(matches!(
                tools
                    .get("ask")
                    .unwrap()
                    .run(
                        &workspace,
                        arguments,
                        "run",
                        &approvals,
                        &never,
                        Duration::from_secs(10),
                        &Default::default(),
                    )
                    .await,
                Err(ToolError::Arguments)
            ));
        }
        drop(workspace);
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    /// Run `echo hi` twice as an owner tool with `rules` as the owner's policy, answering every
    /// prompt with "Allow identical action this run". Returns the prompts, both results, and the
    /// folder that holds the project, the policy, and its audit log.
    #[cfg(unix)]
    async fn echo_twice(
        isolation: Isolation,
        rules: &str,
    ) -> (Vec<AgentEvent>, Vec<Result<String, ToolError>>, PathBuf) {
        use crate::protocol::{ApprovalDecision, ResolveApprovalRequest};
        let (tools, directory) =
            load(r#"{"tools":[{"name":"echo_hi","description":"Echo","command":["echo","hi"]}]}"#)
                .await;
        tokio::fs::write(directory.join(crate::policy::POLICY_FILE), rules)
            .await
            .unwrap();
        let mut workspace = crate::tools::Workspace::open(&directory).unwrap();
        workspace.execution.profile = crate::sandbox::ExecutionProfile::Unsandboxed;
        workspace.execution.command_tools = isolation;
        let approvals = Approvals::default();
        approvals
            .load_policy("run", &directory, &workspace.path)
            .await
            .unwrap();
        let events = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let (captured, decisions) = (events.clone(), approvals.clone());
        let emit = move |event: AgentEvent| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } = &event
            {
                decisions
                    .resolve(ResolveApprovalRequest {
                        request_id: request_id.clone(),
                        approval_id: approval_id.clone(),
                        decision: ApprovalDecision::AlwaysAllow,
                    })
                    .unwrap();
                captured.lock().unwrap().push(event);
            }
        };
        let tool = tools.get("echo_hi").unwrap();
        let mut results = vec![];
        for _ in 0..2 {
            results.push(
                tool.run(
                    &workspace,
                    "{}",
                    "run",
                    &approvals,
                    &emit,
                    Duration::from_secs(10),
                    &Default::default(),
                )
                .await,
            );
        }
        let events = events.lock().unwrap().clone();
        (events, results, directory)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn outside_owner_tool_asks_every_time_despite_allow_rule_and_remember() {
        let (events, results, directory) = echo_twice(
            Isolation::Outside,
            r#"{"rules":[{"decision":"allow","kind":"exec","command":"echo hi"}]}"#,
        )
        .await;
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(events.iter().all(|event| matches!(
            event,
            AgentEvent::ApprovalRequest { remember: false, outside_sandbox: true, detail, .. }
                if detail.starts_with(OUTSIDE_NOTE)
                    && !detail.contains("Approval cannot expand this boundary")
        )));
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn audit_marks_an_allow_rule_an_outside_tool_overrode() {
        let last = |directory: &Path| -> Value {
            let audit = std::fs::read_to_string(directory.join("policy-audit.jsonl")).unwrap();
            serde_json::from_str(audit.lines().last().unwrap()).unwrap()
        };
        let (_, _, directory) = echo_twice(
            Isolation::Outside,
            r#"{"rules":[{"decision":"allow","kind":"exec","command":"echo hi"}]}"#,
        )
        .await;
        let line = last(&directory);
        assert_eq!(line["decision"], "ask", "{line}");
        assert_eq!(line["rule"]["index"], 0, "{line}");
        assert_eq!(line["overridden"], true, "{line}");
        tokio::fs::remove_dir_all(&directory).await.unwrap();
        // Without a matching allow rule there is nothing to override.
        let (_, _, directory) = echo_twice(Isolation::Outside, r#"{"rules":[]}"#).await;
        let line = last(&directory);
        assert!(line.get("overridden").is_none(), "{line}");
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn outside_owner_tool_deny_rule_still_denies_without_asking() {
        let (events, results, directory) = echo_twice(
            Isolation::Outside,
            r#"{"rules":[{"decision":"deny","kind":"exec","command":"echo hi"}]}"#,
        )
        .await;
        assert_eq!(events.len(), 0, "{events:?}");
        assert!(matches!(results[0], Err(ToolError::Denied)));
        assert!(matches!(results[1], Err(ToolError::Denied)));
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn outside_owner_tool_prompt_names_a_broken_policy_after_the_note() {
        let (events, _, directory) = echo_twice(Isolation::Outside, "{").await;
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(events.iter().all(|event| matches!(
            event,
            AgentEvent::ApprovalRequest { detail, .. }
                if detail.starts_with(&format!("{OUTSIDE_NOTE}\n\nThe policy file is not valid."))
                    && detail.contains("\n\nProject: ")
        )), "{events:?}");
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sandboxed_runs_on_tool_still_asks_every_time() {
        use crate::protocol::{ApprovalDecision, ResolveApprovalRequest};
        let (tools, directory) = load(
            r#"{"tools":[{"name":"review","description":"Review","command":["cat"],"stdin":"json","parameters":{"type":"object","properties":{"prompt":{"type":"string"}},"required":["prompt"]}}]}"#,
        )
        .await;
        tokio::fs::write(
            directory.join(crate::policy::POLICY_FILE),
            r#"{"rules":[{"decision":"allow","kind":"exec","command":"cat"}]}"#,
        )
        .await
        .unwrap();
        let mut workspace = crate::tools::Workspace::open(&directory).unwrap();
        workspace.execution.profile = crate::sandbox::ExecutionProfile::Unsandboxed;
        workspace.execution.command_tools = Isolation::Sandboxed;
        let approvals = Approvals::default();
        approvals
            .load_policy("run", &directory, &workspace.path)
            .await
            .unwrap();
        let events = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let (captured, decisions) = (events.clone(), approvals.clone());
        let emit = move |event: AgentEvent| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } = &event
            {
                decisions
                    .resolve(ResolveApprovalRequest {
                        request_id: request_id.clone(),
                        approval_id: approval_id.clone(),
                        decision: ApprovalDecision::AlwaysAllow,
                    })
                    .unwrap();
                captured.lock().unwrap().push(event);
            }
        };
        let tool = tools.get("review").unwrap();
        for _ in 0..2 {
            tool.delegate(
                &workspace,
                "Review the diff",
                "run",
                &approvals,
                &emit,
                Duration::from_secs(10),
                &Default::default(),
            )
            .await
            .unwrap();
        }
        let events = events.lock().unwrap().clone();
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(events.iter().all(|event| matches!(
            event,
            AgentEvent::ApprovalRequest {
                remember: false,
                off_device: true,
                outside_sandbox: false,
                ..
            }
        )));
        drop(workspace);
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn audit_records_the_resolved_absolute_program() {
        let (_, results, directory) = echo_twice(Isolation::Outside, r#"{"rules":[]}"#).await;
        assert!(results[0].is_ok(), "{results:?}");
        let audit = tokio::fs::read_to_string(directory.join("policy-audit.jsonl"))
            .await
            .unwrap();
        let line: Value = serde_json::from_str(audit.lines().last().unwrap()).unwrap();
        let program = line["program"].as_str().unwrap();
        // The configured `echo` is recorded as the absolute path that ran.
        assert!(
            program.starts_with('/') && program.ends_with("/echo"),
            "{line}"
        );
        assert_eq!(line["subject"], "echo hi");
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    /// The audit line for one approved call of a tool that sends `arguments` on standard
    /// input, passes `HOME` through, and puts `{label}` on its command line.
    #[cfg(unix)]
    async fn audited(isolation: Isolation, subagent: bool, arguments: &str) -> Value {
        use crate::protocol::{ApprovalDecision, ResolveApprovalRequest};
        let body = json!({"tools":[{"name":"review","description":"Review","command":["sh","-c","cat",
            if subagent { "fixed" } else { "{label}" }],"stdin":"json","env":["HOME","BLACKWALL_TEST_NEVER_SET"],
            "parameters":{"type":"object","properties":{"prompt":{"type":"string"},"label":{"type":"string"}},
            "required": if subagent { json!(["prompt"]) } else { json!(["prompt","label"]) }}}]});
        let (tools, directory) = load(&body.to_string()).await;
        let mut workspace = crate::tools::Workspace::open(&directory).unwrap();
        workspace.execution.profile = crate::sandbox::ExecutionProfile::Unsandboxed;
        workspace.execution.command_tools = isolation;
        let approvals = Approvals::default();
        approvals
            .load_policy("run", &directory, &workspace.path)
            .await
            .unwrap();
        let decisions = approvals.clone();
        let emit = move |event: AgentEvent| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } = event
            {
                decisions
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision: ApprovalDecision::Allow,
                    })
                    .unwrap();
            }
        };
        let tool = tools.get("review").unwrap();
        let (timeout, output) = (Duration::from_secs(10), Default::default());
        match subagent {
            true => tool
                .delegate(
                    &workspace, arguments, "run", &approvals, &emit, timeout, &output,
                )
                .await
                .unwrap(),
            false => tool
                .run(
                    &workspace, arguments, "run", &approvals, &emit, timeout, &output,
                )
                .await
                .unwrap(),
        };
        let audit = tokio::fs::read_to_string(directory.join("policy-audit.jsonl"))
            .await
            .unwrap();
        drop(workspace);
        tokio::fs::remove_dir_all(&directory).await.unwrap();
        serde_json::from_str(audit.lines().last().unwrap()).unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn audit_records_the_approved_call_without_secret_values() {
        let sha = |bytes: &[u8]| -> String {
            Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        };
        let arguments = r#"{"prompt":"Review","label":"token=abc123"}"#;
        let stdin = serde_json::to_vec(&json!({
            "arguments": serde_json::from_str::<Value>(arguments).unwrap()
        }))
        .unwrap();
        for (isolation, env) in [
            (Isolation::Outside, json!(["HOME"])),
            (Isolation::Sandboxed, json!([])),
        ] {
            let line = audited(isolation, false, arguments).await;
            assert_eq!(line["tool"], "review", "{line}");
            assert_eq!(line["isolation"], isolation.name(), "{line}");
            // The secret-looking argument is redacted, as the subject is.
            assert_eq!(
                line["args"],
                json!(["-c", "cat", "token=[redacted]"]),
                "{line}"
            );
            assert_eq!(line["env"], env, "{line}");
            assert_eq!(line["stdinSha256"], sha(&stdin), "{line}");
            assert!(!line.to_string().contains("abc123"), "{line}");
            // The fields there before are kept.
            assert_eq!(line["subject"], "sh -c cat {label}", "{line}");
            assert!(line["program"].as_str().unwrap().ends_with("/sh"), "{line}");
            assert_eq!(line["allowed"], true, "{line}");
            // A subagent's runs-on tool is recorded the same way; its prompt is on stdin.
            let line = audited(isolation, true, "Review the diff").await;
            let stdin = serde_json::to_vec(&json!({"arguments":{"prompt":"Review the diff"}}));
            assert_eq!(line["tool"], "review", "{line}");
            assert_eq!(line["args"], json!(["-c", "cat", "fixed"]), "{line}");
            assert_eq!(line["stdinSha256"], sha(&stdin.unwrap()), "{line}");
        }
    }

    /// Write an executable script at `path` that prints `text`, then creates `marker`.
    #[cfg(unix)]
    fn script(path: &Path, text: &str, marker: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let body = format!(
            "#!/bin/sh\ncat >/dev/null\necho {text}\ntouch '{}'\n",
            marker.display()
        );
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Run (or, with `subagent`, delegate to) a tool whose program, outside the project, `swap`
    /// replaces while its prompt is open, then approve it. Returns the result and whether the
    /// replacement ran.
    #[cfg(unix)]
    async fn swapped_while_asking(
        isolation: Isolation,
        subagent: bool,
        swap: fn(&Path, &Path),
    ) -> (Result<String, ToolError>, bool) {
        use crate::protocol::{ApprovalDecision, ResolveApprovalRequest};
        let root = std::env::temp_dir().join(format!("blackwall-swap-{}", rand::random::<u64>()));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let program = root.join("bin").join("review");
        let marker = root.join("replacement-ran");
        script(&program, "original", &root.join("original-ran"));
        let body = json!({"tools":[{"name":"review","description":"Review","command":[program],"stdin":"json",
            "parameters":{"type":"object","properties":{"prompt":{"type":"string"}},"required":["prompt"]}}]});
        let (tools, directory) = load(&body.to_string()).await;
        let mut workspace = crate::tools::Workspace::open(&directory).unwrap();
        workspace.execution.profile = crate::sandbox::ExecutionProfile::Unsandboxed;
        workspace.execution.command_tools = isolation;
        let approvals = Approvals::default();
        let decisions = approvals.clone();
        let (target, replacement) = (program.clone(), marker.clone());
        let emit = move |event: AgentEvent| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } = event
            {
                swap(&target, &replacement);
                decisions
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision: ApprovalDecision::Allow,
                    })
                    .unwrap();
            }
        };
        let tool = tools.get("review").unwrap();
        let (timeout, output) = (Duration::from_secs(10), Default::default());
        let result = match subagent {
            true => {
                tool.delegate(
                    &workspace, "Review", "run", &approvals, &emit, timeout, &output,
                )
                .await
            }
            false => {
                tool.run(
                    &workspace,
                    r#"{"prompt":"Review"}"#,
                    "run",
                    &approvals,
                    &emit,
                    timeout,
                    &output,
                )
                .await
            }
        };
        let ran = marker.exists();
        drop(workspace);
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::remove_dir_all(&root).unwrap();
        (result, ran)
    }

    /// The prompt a `cat` tool that passes `env` through asks with, run or delegated to.
    #[cfg(unix)]
    async fn prompt_detail(isolation: Isolation, subagent: bool, env: &[&str]) -> String {
        use crate::protocol::{ApprovalDecision, ResolveApprovalRequest};
        let body = json!({"tools":[{"name":"review","description":"Review","command":["cat"],"stdin":"json","env":env,
            "parameters":{"type":"object","properties":{"prompt":{"type":"string"}},"required":["prompt"]}}]});
        let (tools, directory) = load(&body.to_string()).await;
        let mut workspace = crate::tools::Workspace::open(&directory).unwrap();
        workspace.execution.profile = crate::sandbox::ExecutionProfile::Unsandboxed;
        workspace.execution.command_tools = isolation;
        let approvals = Approvals::default();
        let decisions = approvals.clone();
        let details = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let captured = details.clone();
        let emit = move |event: AgentEvent| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                detail,
                ..
            } = event
            {
                captured.lock().unwrap().push(detail);
                decisions
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision: ApprovalDecision::Allow,
                    })
                    .unwrap();
            }
        };
        let tool = tools.get("review").unwrap();
        let (timeout, output) = (Duration::from_secs(10), Default::default());
        match subagent {
            true => tool
                .delegate(
                    &workspace, "Review", "run", &approvals, &emit, timeout, &output,
                )
                .await
                .unwrap(),
            false => tool
                .run(
                    &workspace,
                    r#"{"prompt":"Review"}"#,
                    "run",
                    &approvals,
                    &emit,
                    timeout,
                    &output,
                )
                .await
                .unwrap(),
        };
        drop(workspace);
        tokio::fs::remove_dir_all(&directory).await.unwrap();
        let detail = details.lock().unwrap()[0].clone();
        detail
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn outside_prompt_names_the_variables_passed_through_never_their_values() {
        const LINE: &str = "Gets these variables from your environment:";
        for subagent in [false, true] {
            // Only names set here are listed: an unset one is not passed, so it is not shown.
            let env = ["HOME", "BLACKWALL_TEST_NEVER_SET", "USER"];
            let detail = prompt_detail(Isolation::Outside, subagent, &env).await;
            let lines: Vec<&str> = detail.lines().collect();
            let at = lines.iter().position(|line| line.starts_with("Program: "));
            let names = at.and_then(|at| lines.get(at + 1)).copied();
            let expected = match std::env::var_os("USER") {
                Some(_) => format!("{LINE} HOME, USER"),
                None => format!("{LINE} HOME"),
            };
            assert_eq!(names, Some(expected.as_str()), "{detail}");
            assert!(!detail.contains("BLACKWALL_TEST_NEVER_SET"), "{detail}");
            // Nothing to list, no line.
            let detail =
                prompt_detail(Isolation::Outside, subagent, &["BLACKWALL_TEST_NEVER_SET"]).await;
            assert!(!detail.contains(LINE), "{detail}");
            // A sandboxed program never gets them, so its prompt names none.
            let detail = prompt_detail(Isolation::Sandboxed, subagent, &env).await;
            assert!(!detail.contains(LINE), "{detail}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_program_replaced_after_its_prompt_is_not_run() {
        // Another file renamed over the program, as an installer or an attacker would.
        fn rename_over(program: &Path, marker: &Path) {
            let fresh = program.with_extension("new");
            script(&fresh, "replaced", marker);
            std::fs::rename(&fresh, program).unwrap();
        }
        // The same file rewritten in place, keeping its inode.
        fn rewrite(program: &Path, marker: &Path) {
            script(program, "rewritten", marker);
        }
        fn untouched(_: &Path, _: &Path) {}
        for isolation in [Isolation::Outside, Isolation::Sandboxed] {
            for subagent in [false, true] {
                for swap in [rename_over as fn(&Path, &Path), rewrite] {
                    let (result, ran) = swapped_while_asking(isolation, subagent, swap).await;
                    let error = result.unwrap_err().to_string();
                    assert!(
                        error.ends_with("changed after you approved it. The command was not run."),
                        "{isolation:?} {subagent} {error}"
                    );
                    assert!(!ran, "{isolation:?} {subagent}");
                }
                let (result, _) = swapped_while_asking(isolation, subagent, untouched).await;
                assert!(
                    result.unwrap().contains("original"),
                    "{isolation:?} {subagent}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sandboxed_owner_tool_keeps_todays_remembered_approval() {
        let (events, results, directory) =
            echo_twice(Isolation::Sandboxed, r#"{"rules":[]}"#).await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(
            &events[0],
            AgentEvent::ApprovalRequest { remember: true, outside_sandbox: false, detail, .. }
                if !detail.contains(OUTSIDE_NOTE)
                    && detail.ends_with("UNSANDBOXED: full account filesystem and network access.")
        ));
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        let audit = tokio::fs::read_to_string(directory.join("policy-audit.jsonl"))
            .await
            .unwrap();
        let line: Value = serde_json::from_str(audit.lines().last().unwrap()).unwrap();
        assert!(line["program"].as_str().unwrap().starts_with('/'), "{line}");
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_json_receives_arguments() {
        use crate::protocol::{ApprovalDecision, ResolveApprovalRequest};
        let (tools, directory) = load(
            r#"{"tools":[{"name":"ask","description":"Ask","command":["cat"],"stdin":"json","parameters":{"type":"object","properties":{"prompt":{"type":"string"},"max":{"type":"integer"}},"required":["prompt"]}}]}"#,
        )
        .await;
        assert!(tools.error.is_none(), "{:?}", tools.error);
        let mut workspace = crate::tools::Workspace::open(&directory).unwrap();
        workspace.execution.profile = crate::sandbox::ExecutionProfile::Unsandboxed;
        let approvals = Approvals::default();
        let decisions = approvals.clone();
        let details = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let captured = details.clone();
        let allow = move |event| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                detail,
                ..
            } = event
            {
                captured.lock().unwrap().push(detail);
                decisions
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision: ApprovalDecision::Allow,
                    })
                    .unwrap();
            }
        };
        // A value longer than the argv limit and starting with `-` is fine on stdin.
        let prompt = format!("-{}", "p".repeat(MAX_VALUE_BYTES + 10));
        let arguments = json!({"prompt":prompt,"max":3}).to_string();
        let output = tools
            .get("ask")
            .unwrap()
            .run(
                &workspace,
                &arguments,
                "run",
                &approvals,
                &allow,
                Duration::from_secs(10),
                &Default::default(),
            )
            .await
            .unwrap();
        let echoed: Value =
            serde_json::from_str(output.strip_prefix("Exit: 0\n").unwrap()).unwrap();
        assert_eq!(echoed, json!({"arguments":{"prompt":prompt,"max":3}}));
        let detail = details.lock().unwrap()[0].clone();
        assert!(detail.contains("Standard input: JSON,"), "{detail}");
        assert!(detail.contains("sha256 "), "{detail}");
        drop(workspace);
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    #[tokio::test]
    async fn stdin_json_without_parameters_takes_only_placeholders() {
        let (tools, directory) = load(
            r#"{"tools":[{"name":"ask","description":"Ask","command":["ask","{prompt}"],"stdin":"json"}]}"#,
        )
        .await;
        assert!(tools.error.is_none(), "{:?}", tools.error);
        let workspace = crate::tools::Workspace::open(&directory).unwrap();
        let approvals = Approvals::default();
        let never = |_: AgentEvent| panic!("invalid arguments never ask for approval");
        for arguments in [
            r#"{"prompt":"a","extra":"b"}"#,
            r#"{"prompt":{"nested":"a"}}"#,
            r#"{"prompt":"a","more":{"nested":1}}"#,
            r#"{}"#,
        ] {
            assert!(
                matches!(
                    tools
                        .get("ask")
                        .unwrap()
                        .run(
                            &workspace,
                            arguments,
                            "run",
                            &approvals,
                            &never,
                            Duration::from_secs(10),
                            &Default::default(),
                        )
                        .await,
                    Err(ToolError::Arguments)
                ),
                "{arguments}"
            );
        }
        drop(workspace);
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    #[tokio::test]
    async fn integer_accepts_whole_number_floats() {
        let (tools, directory) = load(
            r#"{"tools":[{"name":"issue","description":"Issue","command":["issue","{n}"],"parameters":{"type":"object","properties":{"n":{"type":"integer"}},"required":["n"]}}]}"#,
        )
        .await;
        let tool = tools.get("issue").unwrap();
        let schema = tool.parameters.as_ref().unwrap();
        assert!(validate_arguments(schema, &json!({"n":3.0})).is_ok());
        assert!(validate_arguments(schema, &json!({"n":3.5})).is_err());
        assert_eq!(tool.value_text(&json!({"n":3.0}), "n").unwrap(), "3");
        assert_eq!(tool.value_text(&json!({"n":-4}), "n").unwrap(), "-4");
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }
}
