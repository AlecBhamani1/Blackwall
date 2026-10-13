//! Owner-defined tools that run a fixed program with model-supplied arguments, without a shell.
use crate::{
    approvals::Approvals,
    protocol::{AgentEvent, ApprovalKind},
    tools::ToolError,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

pub const COMMAND_TOOLS_FILE: &str = "tools.json";
const MAX_FILE_BYTES: usize = 64 * 1024;
const MAX_TOOLS: usize = 20;
const MAX_ARGUMENTS: usize = 64;
const MAX_VALUE_BYTES: usize = 8000;
const BUILT_IN: [&str; 10] = [
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
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandTool {
    name: String,
    description: String,
    command: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandToolFile {
    tools: Vec<CommandTool>,
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
            Ok(body) if body.len() > MAX_FILE_BYTES => Err("The tools file is too large."),
            Ok(body) => serde_json::from_slice::<CommandToolFile>(&body)
                .map_err(|_| "The tools file is not valid.")
                .and_then(|file| validate(file.tools)),
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
                // A placeholder may repeat; the schema lists each name once.
                let mut seen = std::collections::HashSet::new();
                let parameters: Vec<_> = tool
                    .command
                    .iter()
                    .filter_map(|arg| placeholder(arg))
                    .filter(|name| seen.insert(*name))
                    .collect();
                let properties: serde_json::Map<_, _> = parameters
                    .iter()
                    .map(|name| ((*name).to_owned(), json!({"type":"string"})))
                    .collect();
                json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":{"type":"object","properties":properties,"required":parameters,"additionalProperties":false}}})
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&CommandTool> {
        self.tools.iter().find(|tool| tool.name == name)
    }
}

impl CommandTool {
    /// Rules match the configured command template, so `claude -p` can allow every prompt.
    pub async fn run(
        &self,
        workspace: &Path,
        arguments: &str,
        request_id: &str,
        approvals: &Approvals,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
        timeout: Duration,
    ) -> Result<String, ToolError> {
        let values: Value = serde_json::from_str(arguments).map_err(|_| ToolError::Arguments)?;
        let args = self.command[1..]
            .iter()
            .map(|arg| match placeholder(arg) {
                Some(name) => values
                    .get(name)
                    .and_then(Value::as_str)
                    .filter(|value| value.len() <= MAX_VALUE_BYTES && !is_option(value))
                    .map(str::to_owned)
                    .ok_or(ToolError::Arguments),
                None => Ok(arg.clone()),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let program = resolve_program(&self.command[0]).ok_or_else(|| {
            ToolError::Execution(format!(
                "{} was not found on this computer.",
                self.command[0]
            ))
        })?;
        // JSON keeps distinct argument lists distinct, so a remembered approval stays exact.
        let detail = format!(
            "Project: {}\nTool: {}\nProgram: {}\nArguments: {}\n\nThis program runs with your account permissions. It can access files outside the project and use the network.",
            workspace.display(),
            self.name,
            program.display(),
            serde_json::to_string(&args).map_err(|_| ToolError::Arguments)?,
        );
        if !approvals
            .request(
                request_id,
                ApprovalKind::Exec,
                &self.command.join(" "),
                detail,
                emit,
            )
            .await
            .map_err(ToolError::Execution)?
        {
            return Err(ToolError::Denied);
        }
        crate::tools::program_with_timeout(workspace, &program, &args, timeout).await
    }
}

/// A value that could be read as an option the owner did not configure.
fn is_option(value: &str) -> bool {
    value.starts_with('-') || (cfg!(windows) && value.starts_with('/'))
}

/// An absolute program, or a bare name found in an absolute `PATH` directory, so a relative
/// `PATH` entry can never select a program from the project folder. On Windows the `.exe` is
/// resolved here, so the approved path is exactly the one that runs.
fn resolve_program(program: &str) -> Option<PathBuf> {
    let name = if cfg!(windows) && Path::new(program).extension().is_none() {
        format!("{program}.exe")
    } else {
        program.to_owned()
    };
    let path = Path::new(&name);
    if path.is_absolute() {
        return executable(path).then(|| path.to_path_buf());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(&name))
        .find(|candidate| candidate.is_absolute() && executable(candidate))
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

fn valid_name(name: &str) -> bool {
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
fn valid_program(program: &str) -> bool {
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
            || tool.command.is_empty()
            || tool.command.len() > MAX_ARGUMENTS
            || !valid_program(&tool.command[0])
        {
            return Err("A tool has an invalid name, description, or command.");
        }
    }
    Ok(tools)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::protocol::{ApprovalDecision, ResolveApprovalRequest};

    async fn load(body: &str) -> (CommandTools, std::path::PathBuf) {
        let directory =
            std::env::temp_dir().join(format!("blackwall-tools-{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        tokio::fs::write(directory.join(COMMAND_TOOLS_FILE), body)
            .await
            .unwrap();
        (CommandTools::load(&directory).await, directory)
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
        let (tools, directory) = load(
            r#"{"tools":[{"name":"echo_back","description":"Echo","command":["echo","{text}"]}]}"#,
        )
        .await;
        let tool = tools.get("echo_back").unwrap();
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
                &directory,
                r#"{"text":"hi; touch injected"}"#,
                "run",
                &approvals,
                &allow,
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert!(output.contains("hi; touch injected"));
        assert!(!directory.join("injected").exists());
        assert!(details.lock().unwrap()[0].contains(r#"Arguments: ["hi; touch injected"]"#));
        assert!(matches!(
            tool.run(
                &directory,
                r#"{"text":"--version"}"#,
                "run",
                &approvals,
                &allow,
                Duration::from_secs(10),
            )
            .await,
            Err(ToolError::Arguments)
        ));
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }
}
