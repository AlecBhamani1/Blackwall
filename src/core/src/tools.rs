//! Workspace-capability file tools and explicitly approved shell execution.
use crate::{
    approvals::Approvals,
    child_env::Scope,
    model::ToolCall,
    protocol::{AgentEvent, ApprovalKind},
    sandbox::ExecutionProfile,
};
use cap_std::fs::{Dir, OpenOptions};
use rand::{rand_core::UnwrapErr, rngs::SysRng, Rng};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use tokio::io::AsyncReadExt;

const MAX_FILE: usize = 64 * 1024;
/// A command whose standard output or error passes this many bytes is stopped.
const MAX_STREAMED: u64 = 64 * 1024 * 1024;

/// One tool call's command output cap ([`crate::context::ContextBudget::max_output_bytes`]),
/// and whether that call's output was trimmed to fit it.
#[derive(Debug)]
pub struct OutputCap {
    bytes: usize,
    trimmed: std::sync::atomic::AtomicBool,
}
impl Default for OutputCap {
    fn default() -> Self {
        Self::new(crate::context::DEFAULT_OUTPUT_BYTES)
    }
}
impl OutputCap {
    /// A cap outside 4 KiB to 1 MiB is moved to the nearest end of that range.
    pub fn new(bytes: u32) -> Self {
        let range = crate::context::OUTPUT_BYTES_RANGE;
        Self {
            bytes: bytes.clamp(*range.start(), *range.end()) as usize,
            trimmed: Default::default(),
        }
    }
    /// Whether output was trimmed during this call.
    pub fn trimmed(&self) -> bool {
        self.trimmed.load(std::sync::atomic::Ordering::Relaxed)
    }
    /// `text`, already redacted, trimmed to the cap.
    pub(crate) fn trim_text(&self, text: String) -> String {
        if text.len() <= self.bytes {
            return text;
        }
        self.trim(&[Piece::Bytes(text.as_bytes())])
    }
    fn trim(&self, pieces: &[Piece<'_>]) -> String {
        let (text, trimmed) = trim_pieces(pieces, self.bytes);
        if trimmed {
            self.trimmed
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        text
    }
}

/// Part of a command's output: bytes that were kept, or a run of bytes that were not.
enum Piece<'a> {
    Bytes(&'a [u8]),
    Gap(u64),
}

/// `bytes` as text of at most about `cap` bytes: whole when it fits, otherwise its first 40% and
/// last 60% around a line that says how many bytes were left out. Cuts never split a character.
pub fn trim_output(bytes: &[u8], cap: usize) -> (String, bool) {
    trim_pieces(&[Piece::Bytes(bytes)], cap)
}

fn trim_pieces(pieces: &[Piece<'_>], cap: usize) -> (String, bool) {
    let total: u64 = pieces
        .iter()
        .map(|piece| match piece {
            Piece::Bytes(bytes) => bytes.len() as u64,
            Piece::Gap(length) => *length,
        })
        .sum();
    let whole = pieces.iter().all(|piece| matches!(piece, Piece::Bytes(_)));
    if whole && total <= cap as u64 {
        let bytes: Vec<u8> = pieces
            .iter()
            .flat_map(|piece| match piece {
                Piece::Bytes(bytes) => bytes.iter().copied(),
                Piece::Gap(_) => [].iter().copied(),
            })
            .collect();
        return (String::from_utf8_lossy(&bytes).into_owned(), false);
    }
    let head_length = cap * 2 / 5;
    let mut head = Vec::with_capacity(head_length);
    for piece in pieces {
        let Piece::Bytes(bytes) = piece else { break };
        let room = head_length - head.len();
        head.extend_from_slice(&bytes[..bytes.len().min(room)]);
        if head.len() == head_length {
            break;
        }
    }
    let tail_length = cap - head_length;
    let mut tail = std::collections::VecDeque::with_capacity(tail_length);
    for piece in pieces.iter().rev() {
        let Piece::Bytes(bytes) = piece else { break };
        let room = tail_length - tail.len();
        for byte in bytes[bytes.len().saturating_sub(room)..].iter().rev() {
            tail.push_front(*byte);
        }
        if tail.len() == tail_length {
            break;
        }
    }
    let mut tail = Vec::from(tail);
    head.truncate(char_end(&head));
    let skip = tail
        .iter()
        .take(3)
        .take_while(|byte| **byte & 0xC0 == 0x80)
        .count();
    tail.drain(..skip);
    let omitted = total.saturating_sub((head.len() + tail.len()) as u64);
    (
        format!(
            "{}\n[… {omitted} bytes omitted …]\n{}",
            String::from_utf8_lossy(&head),
            String::from_utf8_lossy(&tail)
        ),
        true,
    )
}

/// The length of `bytes` without a character cut off at its end.
fn char_end(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(4) {
        let index = bytes.len() - back;
        let byte = bytes[index];
        if byte & 0xC0 == 0x80 {
            continue;
        }
        let needed = match byte {
            0xF0..=0xF7 => 4,
            0xE0..=0xEF => 3,
            0xC0..=0xDF => 2,
            _ => 1,
        };
        return if back < needed { index } else { bytes.len() };
    }
    bytes.len()
}
/// The error a command that ran past its deadline returns; telemetry counts it as a timeout.
pub(crate) const TIME_LIMIT_MESSAGE: &str = "The command exceeded its time limit and was stopped.";

/// Project-relative labels use `/` everywhere; Windows file names cannot contain `\`.
pub(crate) fn label(path: &Path) -> Option<String> {
    let label = path.to_str()?;
    Some(if cfg!(windows) {
        label.replace('\\', "/")
    } else {
        label.to_owned()
    })
}
mod browser;
pub use browser::{DirectoryListing, FileEntry, FileSearch, FileSearchMatch, FileSearchMode};
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("Use a relative file path inside the selected project folder.")]
    Path,
    #[error("The file could not be accessed inside this project. It may be missing, protected, or outside the folder.")]
    File,
    #[error(
        "This is larger than the 64 KiB this tool accepts. Use a smaller file or a shorter value."
    )]
    Limit,
    #[error("The tool arguments are missing or invalid.")]
    Arguments,
    #[error("The operation was denied. Do not retry it without new user instructions.")]
    Denied,
    #[error("The file changed after the edit was proposed. Read it again before proposing another edit.")]
    Changed,
    #[error("{0}")]
    Execution(String),
}
impl ToolError {
    /// A stable name for traces, without any tool output.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::File => "file",
            Self::Limit => "limit",
            Self::Arguments => "arguments",
            Self::Denied => "denied",
            Self::Changed => "changed",
            Self::Execution(_) => "execution",
        }
    }
}
#[derive(Clone)]
pub struct Workspace {
    pub(crate) directory: Arc<Dir>,
    pub path: PathBuf,
    pub execution: crate::sandbox::ExecutionPolicy,
}
impl Workspace {
    pub fn open(path: &Path) -> Result<Self, ToolError> {
        let path = std::fs::canonicalize(path).map_err(|_| ToolError::Path)?;
        let directory = Dir::open_ambient_dir(&path, cap_std::ambient_authority())
            .map_err(|_| ToolError::Path)?;
        Ok(Self {
            directory: Arc::new(directory),
            path,
            execution: Default::default(),
        })
    }
    pub(crate) fn checked(path: &str) -> Result<&Path, ToolError> {
        let path = Path::new(path);
        if path.as_os_str().len() > 2048
            || path.is_absolute()
            || path
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
            || path.components().any(|c| c.as_os_str() == ".git")
        {
            return Err(ToolError::Path);
        }
        Ok(path)
    }
    /// Literal, bounded project search. Symlinks and generated dependency folders are skipped.
    pub fn search(&self, path: &str, query: &str) -> Result<String, ToolError> {
        let root = Self::checked(path)?;
        if query.is_empty() || query.len() > 256 {
            return Err(ToolError::Arguments);
        }
        let mut directories = vec![root.to_path_buf()];
        let mut visited = 0;
        let mut bytes = 0;
        let mut matches = Vec::new();
        while let Some(directory) = directories.pop() {
            let entries = self
                .directory
                .read_dir(&directory)
                .map_err(|_| ToolError::File)?;
            for entry in entries {
                visited += 1;
                if visited > 2000 || bytes >= 8 * 1024 * 1024 || matches.len() >= 100 {
                    matches.push(
                        "Search stopped at its scan/result limit. Narrow the directory or query."
                            .into(),
                    );
                    return Ok(matches.join("\n"));
                }
                let entry = entry.map_err(|_| ToolError::File)?;
                let name = entry.file_name();
                if [".git", "node_modules", "target"]
                    .iter()
                    .any(|skip| name == *skip)
                {
                    continue;
                }
                let kind = entry.file_type().map_err(|_| ToolError::File)?;
                let path = directory.join(name);
                if kind.is_dir() && path.components().count() < 20 {
                    directories.push(path);
                } else if kind.is_file() {
                    let Some(label) = label(&path) else {
                        continue;
                    };
                    let Ok(text) = self.read(&label) else {
                        continue;
                    };
                    bytes += text.len();
                    for (index, line) in text.lines().enumerate() {
                        if line.contains(query) {
                            matches.push(format!(
                                "{label}:{}: {}",
                                index + 1,
                                line.chars().take(240).collect::<String>()
                            ));
                            if matches.len() >= 100 {
                                break;
                            }
                        }
                    }
                }
            }
        }
        Ok(if matches.is_empty() {
            "No matches in supported text files.".into()
        } else {
            matches.join("\n")
        })
    }
    pub fn read(&self, path: &str) -> Result<String, ToolError> {
        let path = Self::checked(path)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
        }
        let file = self
            .directory
            .open_with(path, &options)
            .map_err(|_| ToolError::File)?;
        if !file.metadata().map_err(|_| ToolError::File)?.is_file() {
            return Err(ToolError::File);
        }
        let mut bytes = vec![];
        file.take(MAX_FILE as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ToolError::File)?;
        if bytes.len() > MAX_FILE {
            return Err(ToolError::Limit);
        }
        String::from_utf8(bytes).map_err(|_| ToolError::Limit)
    }
    pub(crate) fn original(&self, path: &str) -> Result<Option<String>, ToolError> {
        let checked = Self::checked(path)?;
        match self.directory.symlink_metadata(checked) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(ToolError::Path),
            Ok(_) => self.read(path).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(ToolError::File),
        }
    }
    pub fn write_reviewed(
        &self,
        path: &str,
        expected: Option<&str>,
        content: &str,
    ) -> Result<(), ToolError> {
        if content.len() > MAX_FILE {
            return Err(ToolError::Limit);
        }
        // The comparison and replacement are one cross-process transaction for this workspace.
        let _writing = workspace_write_lock(&self.path)?;
        if self.original(path)?.as_deref() != expected {
            return Err(ToolError::Changed);
        }
        let path = Self::checked(path)?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let directory = self
            .directory
            .open_dir(parent)
            .map_err(|_| ToolError::Path)?;
        let name = path.file_name().ok_or(ToolError::Path)?;
        let temporary = format!(".blackwall-edit-{:016x}", UnwrapErr(SysRng).next_u64());
        let result = (|| {
            let mut file = directory
                .open_with(&temporary, OpenOptions::new().write(true).create_new(true))
                .map_err(|_| ToolError::File)?;
            if let Ok(metadata) = directory.metadata(name) {
                file.set_permissions(metadata.permissions())
                    .map_err(|_| ToolError::File)?;
            }
            file.write_all(content.as_bytes())
                .map_err(|_| ToolError::File)?;
            file.sync_all().map_err(|_| ToolError::File)?;
            // Resolve from an already-open capability, so parent symlink changes cannot escape.
            directory
                .rename(&temporary, &directory, name)
                .map_err(|_| ToolError::File)
        })();
        if result.is_err() {
            let _ = directory.remove_file(&temporary);
        }
        result
    }
    pub fn list(&self, path: &str) -> Result<String, ToolError> {
        let mut names = vec![];
        for entry in self
            .directory
            .read_dir(Self::checked(path)?)
            .map_err(|_| ToolError::File)?
            .take(500)
        {
            let entry = entry.map_err(|_| ToolError::File)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            let suffix = if entry.file_type().map_err(|_| ToolError::File)?.is_dir() {
                "/"
            } else {
                ""
            };
            names.push(format!("{name}{suffix}"));
        }
        names.sort();
        Ok(names.join("\n"))
    }
}

fn workspace_write_lock(path: &Path) -> Result<std::fs::File, ToolError> {
    // Per user: a shared temporary folder would let another account block writes by owning it.
    let directory = std::env::home_dir()
        .map(|home| {
            home.join(".cache")
                .join("blackwall")
                .join("workspace-locks")
        })
        .unwrap_or_else(|| std::env::temp_dir().join("blackwall-workspace-locks"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .map_err(|_| ToolError::File)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(&directory).map_err(|_| ToolError::File)?;
    let digest: String = Sha256::digest(path.to_string_lossy().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    crate::policy::lock_owner_file(&directory, &format!("{digest}.lock"))
        .map_err(|_| ToolError::File)
}

const SHELL_DESCRIPTION: &str = "Propose a shell command in the project directory. Requires approval and runs under the owner's execution profile. Approval does not bypass filesystem or network restrictions.";
fn file_definitions() -> Vec<Value> {
    [("read_file","Read a UTF-8 text file inside the selected project.",json!({"path":{"type":"string"}}),vec!["path"]),
     ("list_files","List up to 500 entries in a project directory. Use '.' for the project root.",json!({"path":{"type":"string"}}),vec!["path"]),
     ("search_files","Search project text files for a literal string. Bounded to 2,000 entries, 8 MiB, and 100 matches; skips symlinks and dependency folders.",json!({"path":{"type":"string"},"query":{"type":"string"}}),vec!["path","query"]),
     ("write_file","Propose a complete text file replacement. The user reviews a diff before the write. Read existing files first.",json!({"path":{"type":"string"},"content":{"type":"string"}}),vec!["path","content"]),
     ("shell",SHELL_DESCRIPTION,json!({"command":{"type":"string"}}),vec!["command"])]
        .into_iter().map(|(name,description,properties,required)|json!({"type":"function","function":{"name":name,"description":description,"parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}})).collect()
}
pub fn definitions(web_enabled: bool, children: bool) -> Vec<Value> {
    let mut definitions = file_definitions();
    if web_enabled {
        definitions.extend(crate::web::definitions());
    }
    if children {
        definitions.push(json!({"type":"function","function":{"name":"spawn_agent","description":"Delegate a focused read-only project investigation. The child can read and list project files but cannot edit, run commands, browse, or spawn more agents.","parameters":{"type":"object","properties":{"goal":{"type":"string"},"context":{"type":"string"}},"required":["goal"],"additionalProperties":false}}}));
    }
    definitions
}

fn argument<'a>(value: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|v| v.len() <= MAX_FILE)
        .ok_or(ToolError::Arguments)
}

pub async fn execute(
    workspace: &Workspace,
    call: &ToolCall,
    request_id: &str,
    approvals: &Approvals,
    emit: &(dyn Fn(AgentEvent) + Send + Sync),
    command_timeout: Duration,
    output: &OutputCap,
) -> Result<String, ToolError> {
    let arguments: Value =
        serde_json::from_str(&call.function.arguments).map_err(|_| ToolError::Arguments)?;
    match call.function.name.as_str() {
        "read_file" => workspace.read(argument(&arguments, "path")?),
        "list_files" => workspace.list(argument(&arguments, "path")?),
        "search_files" => workspace.search(
            argument(&arguments, "path")?,
            argument(&arguments, "query")?,
        ),
        "write_file" => {
            let path = argument(&arguments, "path")?;
            let content = argument(&arguments, "content")?;
            let original = workspace.original(path)?;
            let diff = similar::TextDiff::from_lines(original.as_deref().unwrap_or(""), content)
                .unified_diff()
                .header(&format!("a/{path}"), &format!("b/{path}"))
                .to_string();
            if diff.len() > 128 * 1024 {
                return Err(ToolError::Limit);
            }
            let detail = format!(
                "Project: {}\nFile: {path}\n\n{diff}",
                workspace.path.display()
            );
            if !approvals
                .request(request_id, ApprovalKind::File, path, detail, emit)
                .await
                .map_err(ToolError::Execution)?
            {
                return Err(ToolError::Denied);
            }
            // The write may wait on another process's lock, so keep it off the async workers.
            let (writer, target, body) = (workspace.clone(), path.to_owned(), content.to_owned());
            tokio::task::spawn_blocking(move || {
                writer.write_reviewed(&target, original.as_deref(), &body)
            })
            .await
            .map_err(|_| ToolError::File)??;
            Ok(format!("Saved {path}"))
        }
        "shell" => {
            let command = argument(&arguments, "command")?;
            if command.trim().is_empty() || command.len() > 8000 {
                return Err(ToolError::Arguments);
            }
            // Never ask to approve a command that can't run under the owner's profile.
            crate::sandbox::ready(workspace).await?;
            let detail = format!(
                "Project: {}\nCommand: {command}\n\n{}",
                workspace.path.display(),
                workspace.execution.description()
            );
            if !approvals
                .request(request_id, ApprovalKind::Exec, command, detail, emit)
                .await
                .map_err(ToolError::Execution)?
            {
                return Err(ToolError::Denied);
            }
            let (process, mut guard) =
                crate::sandbox::prepare(workspace, shell_command(command), Scope::Shell, &[])
                    .await?;
            let outcome = run_with_input(
                &workspace.path,
                process,
                None,
                command_timeout,
                output,
                Some(&mut guard),
            )
            .await;
            guard.finish(outcome).await
        }
        "web_fetch" | "web_search" => crate::web::execute(call, request_id, approvals, emit).await,
        _ => Err(ToolError::Arguments),
    }
}

#[cfg(unix)]
struct ProcessGroup(u32);
#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // Fixed executable and numeric process-group argument, never model-provided shell syntax.
        let _ = std::process::Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", self.0)])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
#[cfg(unix)]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut process = tokio::process::Command::new("/bin/sh");
    process.arg("-c").arg(command);
    process
}
#[cfg(windows)]
fn shell_command(command: &str) -> tokio::process::Command {
    use base64::Engine;
    // An encoded command avoids Windows argument re-quoting; UTF-8 output keeps capture lossless.
    let script = format!(
        "$ProgressPreference='SilentlyContinue'; [Console]::OutputEncoding=[Text.Encoding]::UTF8\n{command}"
    );
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut process = tokio::process::Command::new("powershell.exe");
    process
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
        ])
        .arg(base64::engine::general_purpose::STANDARD.encode(bytes));
    process
}
#[cfg(test)]
async fn shell(directory: &Path, command: &str) -> Result<String, ToolError> {
    shell_with_timeout(directory, command, Duration::from_secs(120)).await
}
#[cfg(test)]
async fn shell_with_timeout(
    directory: &Path,
    command: &str,
    timeout: Duration,
) -> Result<String, ToolError> {
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (directory, command, timeout);
        Err(ToolError::Execution(
            "Shell tools are not supported on this platform.".into(),
        ))
    }
    #[cfg(any(unix, windows))]
    run_with_timeout(
        directory,
        shell_command(command),
        timeout,
        &OutputCap::default(),
    )
    .await
}
/// Run a resolved program with separate arguments, never parsed by a shell. `input`, when
/// given, is written to its standard input, which is otherwise empty. `pass_through` names the
/// owner's variables the program also receives, unless it runs sandboxed. It runs outside the
/// sandbox unless the owner chose `Isolation::Sandboxed`.
pub(crate) async fn program_with_timeout(
    workspace: &Workspace,
    program: &Path,
    args: &[String],
    pass_through: &[String],
    input: Option<Vec<u8>>,
    timeout: Duration,
    output: &OutputCap,
) -> Result<String, ToolError> {
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (
            workspace,
            program,
            args,
            pass_through,
            input,
            timeout,
            output,
        );
        Err(ToolError::Execution(
            "Owner tools are not supported on this platform.".into(),
        ))
    }
    #[cfg(any(unix, windows))]
    {
        let mut process = tokio::process::Command::new(program);
        process.args(args);
        if workspace.execution.command_tools == crate::command_tools::Isolation::Outside {
            // The owner chose to run command tools as themselves, whatever the shell profile.
            crate::child_env::from_process(Scope::Owner, &workspace.path, pass_through)
                .apply(&mut process);
            return run_with_input(&workspace.path, process, input, timeout, output, None).await;
        }
        let (process, mut guard) =
            crate::sandbox::prepare(workspace, process, Scope::Owner, pass_through).await?;
        let outcome = run_with_input(
            &workspace.path,
            process,
            input,
            timeout,
            output,
            Some(&mut guard),
        )
        .await;
        match guard.finish(outcome).await {
            Err(ToolError::Execution(text)) => Err(ToolError::Execution(with_sandbox_hint(
                text,
                workspace.execution.profile,
            ))),
            result => result,
        }
    }
}
/// Added to a command tool failure that looks like a sandbox denial; `{profile}` is the
/// profile's display name.
pub(crate) const SANDBOXED_TOOL_HINT: &str = "This may have been blocked by the {profile} sandbox. Command tools that need their own files or network can run outside it: Memory → Advanced · Subagents → Command tools, or `bw config set command-tools-isolation outside`.";
/// What a sandbox denial prints, in lower case. A program that merely failed prints none.
const SANDBOX_DENIAL_MARKERS: [&str; 10] = [
    "operation not permitted",
    "permission denied",
    "read-only file system",
    "sandbox",
    "deny",
    "network is unreachable",
    "could not resolve host",
    "connection refused",
    "not allowed",
    // bubblewrap's own errors, such as a program that isn't in the sandbox.
    "bwrap: ",
];
/// `text` with the sandbox hint added when a sandboxed profile is in force and the failure looks
/// like a denial. Never on the time limit, and never for a program that just exited non-zero.
fn with_sandbox_hint(text: String, profile: ExecutionProfile) -> String {
    let name = match profile {
        ExecutionProfile::ReadOnly => "Read-only",
        ExecutionProfile::WorkspaceWrite => "Workspace-write",
        ExecutionProfile::Unsandboxed => return text,
    };
    let lower = text.to_lowercase();
    if text == TIME_LIMIT_MESSAGE || !SANDBOX_DENIAL_MARKERS.iter().any(|m| lower.contains(m)) {
        return text;
    }
    format!("{text}\n{}", SANDBOXED_TOOL_HINT.replace("{profile}", name))
}
static SEARCH_PATH: std::sync::OnceLock<std::ffi::OsString> = std::sync::OnceLock::new();
/// Use the owner's login-shell `PATH` to find and run programs. Apps opened from the macOS Dock
/// start with only the system directories, so tools installed with Homebrew or npm are missing.
pub fn set_search_path(path: std::ffi::OsString) {
    let _ = SEARCH_PATH.set(path);
}
/// The `PATH` used to find programs: the login-shell one when the desktop app set it.
pub(crate) fn search_path() -> Option<std::ffi::OsString> {
    SEARCH_PATH
        .get()
        .cloned()
        .or_else(|| std::env::var_os("PATH"))
}
/// A long-running program with piped standard streams, such as an MCP server. Dropping it stops
/// the program and its descendants, like a finished or cancelled shell command.
#[cfg(any(unix, windows))]
pub(crate) struct ServerProcess {
    #[cfg(unix)]
    _child: tokio::process::Child,
    #[cfg(unix)]
    _group: ProcessGroup,
    #[cfg(windows)]
    _child: Box<dyn process_wrap::tokio::ChildWrapper>,
}
#[cfg(any(unix, windows))]
pub(crate) type ServerStreams = (
    ServerProcess,
    tokio::process::ChildStdin,
    tokio::process::ChildStdout,
    tokio::process::ChildStderr,
);
/// Start a resolved program without a shell, in `directory`, with only `env` and then the
/// server's own `secrets`.
#[cfg(any(unix, windows))]
pub(crate) fn spawn_server(
    directory: &Path,
    program: &Path,
    args: &[String],
    env: &crate::child_env::ChildEnv,
    secrets: &[(String, String)],
) -> Result<ServerStreams, ToolError> {
    let start = || ToolError::Execution("The program could not start.".into());
    let mut process = tokio::process::Command::new(program);
    process
        .args(args)
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    env.apply(&mut process);
    process.envs(secrets.iter().map(|(name, value)| (name, value)));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.as_std_mut().process_group(0);
        let mut child = process.spawn().map_err(|_| start())?;
        let group = ProcessGroup(child.id().ok_or_else(start)?);
        let stdin = child.stdin.take().ok_or_else(start)?;
        let stdout = child.stdout.take().ok_or_else(start)?;
        let stderr = child.stderr.take().ok_or_else(start)?;
        Ok((
            ServerProcess {
                _child: child,
                _group: group,
            },
            stdin,
            stdout,
            stderr,
        ))
    }
    #[cfg(windows)]
    {
        use process_wrap::tokio::{CommandWrap, CreationFlags, JobObject, KillOnDrop};
        use windows::Win32::System::Threading::CREATE_NO_WINDOW;
        let mut process = CommandWrap::from(process);
        process
            .wrap(CreationFlags(CREATE_NO_WINDOW))
            .wrap(KillOnDrop)
            .wrap(JobObject);
        let mut child = process.spawn().map_err(|_| start())?;
        let stdin = child.stdin().take().ok_or_else(start)?;
        let stdout = child.stdout().take().ok_or_else(start)?;
        let stderr = child.stderr().take().ok_or_else(start)?;
        Ok((ServerProcess { _child: child }, stdin, stdout, stderr))
    }
}
/// [`run_with_input`] with no input, for a command that isn't from `sandbox::prepare`.
#[cfg(all(test, any(unix, windows)))]
pub(crate) async fn run_with_timeout(
    directory: &Path,
    process: tokio::process::Command,
    timeout: Duration,
    output: &OutputCap,
) -> Result<String, ToolError> {
    run_with_input(directory, process, None, timeout, output, None).await
}
/// The start and end of one output stream, and how many bytes it had in all.
#[cfg(any(unix, windows))]
#[derive(Default)]
struct Stream {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    total: u64,
}
#[cfg(any(unix, windows))]
impl Stream {
    /// Keep up to `keep` bytes from each end, which covers any cut a cap of `keep` makes. Reading
    /// stops once the stream passes [`MAX_STREAMED`].
    async fn read(
        mut reader: impl tokio::io::AsyncRead + Unpin,
        keep: usize,
    ) -> std::io::Result<Self> {
        let mut stream = Self::default();
        let mut buffer = vec![0; 16 * 1024];
        while stream.total <= MAX_STREAMED {
            let read = reader.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            stream.total += read as u64;
            let chunk = &buffer[..read];
            let room = keep.saturating_sub(stream.head.len()).min(chunk.len());
            stream.head.extend_from_slice(&chunk[..room]);
            stream.tail.extend(&chunk[room..]);
            let excess = stream.tail.len().saturating_sub(keep);
            stream.tail.drain(..excess);
        }
        Ok(stream)
    }
    fn pieces<'a>(&'a self, pieces: &mut Vec<Piece<'a>>) {
        let (first, second) = self.tail.as_slices();
        let kept = (self.head.len() + first.len() + second.len()) as u64;
        pieces.push(Piece::Bytes(&self.head));
        if self.total > kept {
            pieces.push(Piece::Gap(self.total - kept));
        }
        pieces.push(Piece::Bytes(first));
        pieces.push(Piece::Bytes(second));
    }
}

/// Return a Win32-compatible spelling when a verbatim drive path can be represented without
/// changing its meaning. `cmd.exe` treats a `\\?\C:\...` current directory as a UNC path and
/// falls back to `C:\Windows`.
#[cfg(windows)]
fn child_current_dir(path: &Path) -> std::borrow::Cow<'_, Path> {
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::Prefix,
    };

    let mut components = path.components();
    let drive = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(drive) => drive,
            _ => return std::borrow::Cow::Borrowed(path),
        },
        _ => return std::borrow::Cow::Borrowed(path),
    };
    if !matches!(components.next(), Some(Component::RootDir)) {
        return std::borrow::Cow::Borrowed(path);
    }

    let mut simplified = vec![u16::from(drive), u16::from(b':'), u16::from(b'\\')];
    for component in components {
        let Component::Normal(component) = component else {
            return std::borrow::Cow::Borrowed(path);
        };
        let name: Vec<u16> = component.encode_wide().collect();
        if name.last().is_some_and(|last| matches!(*last, 0x20 | 0x2e))
            || name
                .iter()
                .any(|unit| matches!(*unit, 0..=31 | 0x22 | 0x2a | 0x2f | 0x3a | 0x3c..=0x3e | 0x3f | 0x5c | 0x7c))
            || is_reserved_windows_name(&name)
        {
            return std::borrow::Cow::Borrowed(path);
        }
        if simplified.len() > 3 {
            simplified.push(u16::from(b'\\'));
        }
        simplified.extend(name);
    }

    // MAX_PATH includes the terminating NUL.
    if simplified.len() >= 260 {
        return std::borrow::Cow::Borrowed(path);
    }
    std::borrow::Cow::Owned(PathBuf::from(OsString::from_wide(&simplified)))
}

#[cfg(windows)]
fn is_reserved_windows_name(name: &[u16]) -> bool {
    let stem = &name[..name
        .iter()
        .position(|unit| *unit == u16::from(b'.'))
        .unwrap_or(name.len())];
    let Ok(stem) = stem
        .iter()
        .map(|unit| u8::try_from(*unit))
        .collect::<Result<Vec<_>, _>>()
    else {
        return false;
    };
    let Ok(stem) = std::str::from_utf8(&stem) else {
        return false;
    };
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$"
    ) || stem.len() == 4
        && matches!(stem[..3].to_ascii_uppercase().as_str(), "COM" | "LPT")
        && matches!(stem.as_bytes()[3], b'1'..=b'9')
}

/// Bounded, time-limited capture with the same descendant cleanup for every process, writing
/// `input` to standard input while output is captured. Output over the cap is trimmed to its start
/// and end. The command brings its own environment; this never adds to or inherits one on its behalf. It runs in
/// `directory` unless it already has a working directory (the Linux sandbox launcher runs from
/// `/` and moves the command into the project itself). A command from `sandbox::prepare` is
/// started by its `guard`.
#[cfg(any(unix, windows))]
pub(crate) async fn run_with_input(
    directory: &Path,
    mut process: tokio::process::Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    output: &OutputCap,
    guard: Option<&mut crate::sandbox::RunGuard>,
) -> Result<String, ToolError> {
    if process.as_std().get_current_dir().is_none() {
        #[cfg(windows)]
        process.current_dir(child_current_dir(directory).as_ref());
        #[cfg(not(windows))]
        process.current_dir(directory);
    }
    process
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.as_std_mut().process_group(0);
    }
    #[cfg(unix)]
    let mut child = match guard {
        Some(guard) => guard.spawn(&mut process),
        None => process.spawn(),
    }
    .map_err(|_| ToolError::Execution("The command could not start.".into()))?;
    #[cfg(unix)]
    let _group = ProcessGroup(
        child
            .id()
            .ok_or(ToolError::Execution("The command did not start.".into()))?,
    );
    #[cfg(windows)]
    let mut child = {
        use process_wrap::tokio::{CommandWrap, CreationFlags, JobObject, KillOnDrop};
        use windows::Win32::System::Threading::CREATE_NO_WINDOW;
        // Windows has no sandbox to hand anything to.
        let _ = guard;

        let mut process = CommandWrap::from(process);
        // The wrapper spawns suspended, assigns the process to a kill-on-close job,
        // then resumes it. Descendants cannot escape before assignment or by parent exit.
        process
            .wrap(CreationFlags(CREATE_NO_WINDOW))
            .wrap(KillOnDrop)
            .wrap(JobObject);
        process
            .spawn()
            .map_err(|_| ToolError::Execution("The command could not start.".into()))?
    };
    #[cfg(unix)]
    let stdout = child.stdout.take().ok_or(ToolError::File)?;
    #[cfg(windows)]
    let stdout = child.stdout().take().ok_or(ToolError::File)?;
    #[cfg(unix)]
    let stderr = child.stderr.take().ok_or(ToolError::File)?;
    #[cfg(windows)]
    let stderr = child.stderr().take().ok_or(ToolError::File)?;
    #[cfg(unix)]
    let stdin = child.stdin.take();
    #[cfg(windows)]
    let stdin = child.stdin().take();
    // A program that exits without reading its input is not an error, so write failures are
    // ignored; dropping the pipe after the write ends the program's input.
    let mut in_task = Box::pin(async move {
        if let (Some(mut stdin), Some(input)) = (stdin, input) {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(&input).await;
        }
    });
    let mut out_task = Box::pin(Stream::read(stdout, output.bytes));
    let mut err_task = Box::pin(Stream::read(stderr, output.bytes));
    let mut wait_task = Box::pin(async {
        #[cfg(unix)]
        {
            child.wait().await
        }
        #[cfg(windows)]
        {
            // Wait on the shell itself, retaining the owning job. Stop descendants as
            // soon as the shell exits so inherited output pipes cannot hold capture open.
            // Dropping the job also stops them if this wait is cancelled before then.
            let status = child.inner_mut().wait().await?;
            child.start_kill()?;
            Ok::<_, std::io::Error>(status)
        }
    });
    let capture = async {
        // A stream past its hard limit stops the command at once, rather than waiting for a
        // child blocked on a full pipe. The process group/job kills descendants on return.
        let mut out: Option<Stream> = None;
        let mut err: Option<Stream> = None;
        let mut status = None;
        let mut written = false;
        let mut stopped = false;
        while !stopped && (out.is_none() || err.is_none() || status.is_none()) {
            tokio::select! {
                () = &mut in_task, if !written => written = true,
                result = &mut out_task, if out.is_none() => {
                    let stream = result.map_err(|_| ToolError::File)?;
                    stopped |= stream.total > MAX_STREAMED;
                    out = Some(stream);
                }
                result = &mut err_task, if err.is_none() => {
                    let stream = result.map_err(|_| ToolError::File)?;
                    stopped |= stream.total > MAX_STREAMED;
                    err = Some(stream);
                }
                result = &mut wait_task, if status.is_none() => {
                    status = Some(result.map_err(|_| ToolError::File)?)
                }
            }
        }
        let successful = !stopped && status.is_some_and(|status| status.success());
        let exit = format!(
            "Exit: {}\n",
            match status.and_then(|s| s.code()) {
                _ if stopped => "stopped".to_owned(),
                Some(code) => code.to_string(),
                None => "signal".to_owned(),
            }
        );
        let (out, err) = (out.unwrap_or_default(), err.unwrap_or_default());
        let mut pieces = vec![Piece::Bytes(exit.as_bytes())];
        out.pieces(&mut pieces);
        err.pieces(&mut pieces);
        let mut text = output.trim(&pieces);
        if stopped {
            text.push_str("\nThe command was stopped after 64 MiB of output.");
        }
        if successful {
            Ok(text)
        } else {
            Err(ToolError::Execution(text))
        }
    };
    tokio::time::timeout(timeout, capture)
        .await
        .map_err(|_| ToolError::Execution(TIME_LIMIT_MESSAGE.into()))?
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn child_current_dir_only_simplifies_safe_verbatim_drive_paths() {
        assert_eq!(
            child_current_dir(Path::new(r"\\?\C:\work\project")),
            Path::new(r"C:\work\project")
        );
        assert_eq!(
            child_current_dir(Path::new(r"\\?\UNC\server\share\project")),
            Path::new(r"\\?\UNC\server\share\project")
        );
        for unsafe_path in [
            r"\\?\C:\work\CON",
            r"\\?\C:\work\com1.txt",
            r"\\?\C:\work\trailing.",
            r"\\?\C:\work\trailing ",
            r"\\?\C:\work\question?mark",
        ] {
            assert_eq!(
                child_current_dir(Path::new(unsafe_path)),
                Path::new(unsafe_path)
            );
        }

        let long = format!(r"\\?\C:\{}\{}", "a".repeat(200), "b".repeat(100));
        assert_eq!(child_current_dir(Path::new(&long)), Path::new(&long));
    }

    #[test]
    fn trim_keeps_whole_characters_and_counts_what_it_leaves_out() {
        let (text, trimmed) = trim_output(b"short", 4096);
        assert_eq!((text.as_str(), trimmed), ("short", false));
        // 'é' is two bytes, so 3000 of them cannot be cut evenly at 40% and 60% of 4096.
        let input = "é".repeat(3000);
        let (text, trimmed) = trim_output(input.as_bytes(), 4096);
        assert!(trimmed && !text.contains('\u{fffd}'));
        let (head, rest) = text.split_once("\n[… ").unwrap();
        let (marker, tail) = rest.split_once(" bytes omitted …]\n").unwrap();
        assert!(head.chars().all(|c| c == 'é') && tail.chars().all(|c| c == 'é'));
        assert!(head.len() <= 1638 && tail.len() <= 2458);
        assert_eq!(
            marker.parse::<usize>().unwrap(),
            input.len() - head.len() - tail.len()
        );
    }
    #[test]
    fn sandbox_hint_only_follows_a_failure_that_looks_like_a_denial() {
        let hinted = |text: &str, profile| with_sandbox_hint(text.to_owned(), profile);
        let denied = "Exit: 1\ntouch: x: Operation not permitted\n";
        let read_only = hinted(denied, ExecutionProfile::ReadOnly);
        assert!(read_only.starts_with(denied));
        assert!(read_only.contains("This may have been blocked by the Read-only sandbox."));
        assert!(read_only.contains("Memory → Advanced · Subagents → Command tools"));
        assert!(hinted(
            "curl: (6) Could not resolve host: x",
            ExecutionProfile::WorkspaceWrite
        )
        .contains("the Workspace-write sandbox."));
        // bubblewrap's own failure, such as a program missing from the sandbox.
        assert!(hinted(
            "Exit: 1\nbwrap: execvp /x: No such file or directory\n",
            ExecutionProfile::ReadOnly
        )
        .contains("the Read-only sandbox."));
        // A plain failure, the time limit, and an unsandboxed profile get no hint.
        for text in [
            "Exit: 3\n",
            "Exit: 1\nFAILED tests::a\n",
            TIME_LIMIT_MESSAGE,
        ] {
            assert_eq!(hinted(text, ExecutionProfile::ReadOnly), text);
        }
        assert_eq!(hinted(denied, ExecutionProfile::Unsandboxed), denied);
    }
    #[cfg(unix)]
    async fn run_env_probe(
        directory: &Path,
        env: &crate::child_env::ChildEnv,
        script: &str,
    ) -> Result<String, ToolError> {
        let mut process = shell_command(script);
        env.apply(&mut process);
        run_with_timeout(
            directory,
            process,
            Duration::from_secs(30),
            &OutputCap::default(),
        )
        .await
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn unsandboxed_shell_inherits_the_owner_environment_minus_blackwall_and_loader_variables()
    {
        use crate::child_env::build;
        let directory = std::env::temp_dir();
        let source = [
            ("HOME", "/h"),
            ("SSH_AUTH_SOCK", "/s"),
            ("KUBECONFIG", "/k"),
            ("BLACKWALL_MODEL_API_KEY", "key"),
            ("PYTHONHOME", "/py"),
            ("LD_PRELOAD", "/x.so"),
            ("APPDIR", "/tmp/.mount_x"),
            ("APPIMAGE", "/a.AppImage"),
        ]
        .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v)));
        let env = build(
            source,
            Scope::Shell,
            Some(std::ffi::OsStr::new("/usr/bin:/bin")),
            &directory,
            &[],
        );
        let out = run_env_probe(&directory, &env, "env").await.unwrap();
        for kept in ["HOME=/h", "SSH_AUTH_SOCK=/s", "KUBECONFIG=/k"] {
            assert!(out.contains(kept), "{out}");
        }
        for gone in [
            "BLACKWALL_MODEL_API_KEY",
            "PYTHONHOME",
            "LD_PRELOAD",
            "APPDIR",
            "APPIMAGE",
        ] {
            assert!(!out.contains(gone), "{out}");
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn the_shell_tool_is_wired_to_the_shell_scope_and_programs_to_the_owner_scope() {
        let directory = std::env::temp_dir().join(format!(
            "blackwall-shell-env-{}",
            UnwrapErr(SysRng).next_u64()
        ));
        std::fs::create_dir(&directory).unwrap();
        let mut workspace = Workspace::open(&directory).unwrap();
        workspace.execution.profile = crate::sandbox::ExecutionProfile::Unsandboxed;
        // Cargo sets this for the test process: the shell keeps it, programs do not get it.
        let process = crate::sandbox::prepare(&workspace, shell_command("env"), Scope::Shell, &[])
            .await
            .unwrap()
            .0;
        let out = run_with_timeout(
            &workspace.path,
            process,
            Duration::from_secs(30),
            &OutputCap::default(),
        )
        .await
        .unwrap();
        assert!(out.contains("CARGO_MANIFEST_DIR="), "{out}");
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn owner_programs_get_only_allow_listed_and_passed_through_variables() {
        let directory = std::env::temp_dir().join(format!(
            "blackwall-owner-env-{}",
            UnwrapErr(SysRng).next_u64()
        ));
        std::fs::create_dir(&directory).unwrap();
        let mut workspace = Workspace::open(&directory).unwrap();
        workspace.execution.profile = crate::sandbox::ExecutionProfile::Unsandboxed;
        let run = |pass: Vec<String>| {
            let workspace = workspace.clone();
            async move {
                program_with_timeout(
                    &workspace,
                    Path::new("/usr/bin/env"),
                    &[],
                    &pass,
                    None,
                    Duration::from_secs(30),
                    &OutputCap::default(),
                )
                .await
                .unwrap()
            }
        };
        // Cargo sets this for the test process, so a leak would show.
        assert!(!run(vec![]).await.contains("CARGO_MANIFEST_DIR"));
        assert!(run(vec!["CARGO_MANIFEST_DIR".into()])
            .await
            .contains("CARGO_MANIFEST_DIR="));
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn output_over_cap_is_trimmed_head_and_tail() {
        let cap = OutputCap::new(4096);
        // 300,000 bytes: a start line, filler, and an end line.
        let output = run_with_timeout(
            &std::env::temp_dir(),
            shell_command("printf START; head -c 299992 /dev/zero | tr '\\0' x; printf END"),
            Duration::from_secs(30),
            &cap,
        )
        .await
        .unwrap();
        assert!(cap.trimmed());
        assert!(output.starts_with("Exit: 0\nSTARTxxx") && output.ends_with("xxxEND"));
        let head = output.split("\n[… ").next().unwrap();
        assert_eq!(head.len(), 4096 * 2 / 5);
        assert!(output.contains(&format!(
            "[… {} bytes omitted …]",
            "Exit: 0\n".len() + 300_000 - 4096
        )));
        assert!(output.len() < 4096 + 64);
        // Within the cap nothing changes, and the default cap is the old 64 KiB limit.
        let cap = OutputCap::default();
        let output = run_with_timeout(
            &std::env::temp_dir(),
            shell_command("head -c 60000 /dev/zero | tr '\\0' y"),
            Duration::from_secs(30),
            &cap,
        )
        .await
        .unwrap();
        assert!(!cap.trimmed() && output.len() == "Exit: 0\n".len() + 60000);
        // A failing command's output is trimmed too, and a stream past 64 MiB stops it.
        let cap = OutputCap::new(4096);
        let error = run_with_timeout(
            &std::env::temp_dir(),
            shell_command("yes overflow"),
            Duration::from_secs(60),
            &cap,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(cap.trimmed() && error.starts_with("Exit: stopped\noverflow"));
        assert!(error.ends_with("The command was stopped after 64 MiB of output."));
    }
    #[test]
    fn file_read_cap_unchanged() {
        let path = std::env::temp_dir().join(format!(
            "blackwall-read-cap-{:016x}",
            UnwrapErr(SysRng).next_u64()
        ));
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("fits.txt"), "a".repeat(MAX_FILE)).unwrap();
        std::fs::write(path.join("large.txt"), "a".repeat(MAX_FILE + 1)).unwrap();
        let workspace = Workspace::open(&path).unwrap();
        assert_eq!(workspace.read("fits.txt").unwrap().len(), MAX_FILE);
        // Reads are never trimmed: a larger file is refused, and the message says why.
        let error = workspace.read("large.txt").unwrap_err();
        assert!(matches!(error, ToolError::Limit));
        assert!(error.to_string().contains("64 KiB") && !error.to_string().contains("text files"));
        drop(workspace);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn file_tools_reject_escape_and_stale_edits() {
        let path = std::env::temp_dir().join(format!(
            "blackwall-tools-{:016x}",
            UnwrapErr(SysRng).next_u64()
        ));
        std::fs::create_dir(&path).unwrap();
        let workspace = Workspace::open(&path).unwrap();
        assert!(workspace.read("../outside").is_err());
        std::fs::write(path.join("search.txt"), "first line\nneedle here").unwrap();
        assert!(workspace
            .search(".", "needle")
            .unwrap()
            .contains("search.txt:2: needle here"));
        assert!(workspace.search("../", "needle").is_err());
        assert!(workspace.read("/etc/passwd").is_err());
        workspace
            .write_reviewed("file.txt", None, "before")
            .unwrap();
        assert!(matches!(
            workspace.write_reviewed("file.txt", Some("other"), "after"),
            Err(ToolError::Changed)
        ));
        workspace
            .write_reviewed("file.txt", Some("before"), "after")
            .unwrap();
        assert_eq!(workspace.read("file.txt").unwrap(), "after");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", path.join("escape")).unwrap();
            assert!(workspace.read("escape/passwd").is_err());
            assert!(workspace
                .write_reviewed("escape/new.txt", None, "no")
                .is_err());
        }
        // Close the directory handle first; Windows cannot delete an open directory.
        drop(workspace);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn reviewed_write_rechecks_after_the_workspace_lock() {
        let path = std::env::temp_dir().join(format!(
            "blackwall-tools-lock-{:016x}",
            UnwrapErr(SysRng).next_u64()
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("file.txt"), "before").unwrap();
        let workspace = Workspace::open(&path).unwrap();
        let lock = workspace_write_lock(&workspace.path).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let writer = workspace.clone();
        let handle = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            result_tx
                .send(writer.write_reviewed("file.txt", Some("before"), "agent"))
                .unwrap();
        });
        started_rx.recv().unwrap();
        assert!(result_rx.recv_timeout(Duration::from_millis(100)).is_err());
        std::fs::write(path.join("file.txt"), "concurrent").unwrap();
        drop(lock);
        assert!(matches!(
            result_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Err(ToolError::Changed)
        ));
        handle.join().unwrap();
        assert_eq!(workspace.read("file.txt").unwrap(), "concurrent");
        drop(workspace);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn shell_captures_exit_status_and_caps_output() {
        let output = shell(&std::env::temp_dir(), "printf hello; exit 7")
            .await
            .unwrap_err()
            .to_string();
        assert!(output.contains("Exit: 7"));
        assert!(output.contains("hello"));
        assert!(shell(&std::env::temp_dir(), "yes overflow").await.is_err());
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_shell_preserves_quoting_unicode_and_exit_status() {
        let output = shell(
            &std::env::temp_dir(),
            "Write-Output 'h\u{e9}llo \"quoted\" & | $literal'; exit 7",
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(output.contains("Exit: 7"));
        assert!(output.contains("h\u{e9}llo \"quoted\" & | $literal"));
        assert!(shell(&std::env::temp_dir(), "while ($true) { 'overflow' }")
            .await
            .is_err());
    }
    #[cfg(windows)]
    fn windows_shell_test_directory() -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "blackwall-shell-cancel-{}",
            UnwrapErr(SysRng).next_u64()
        ));
        std::fs::create_dir(&directory).unwrap();
        directory
    }
    #[cfg(windows)]
    async fn wait_for_windows_shell_start(
        run: &mut std::pin::Pin<Box<impl std::future::Future<Output = Result<String, ToolError>>>>,
        directory: &Path,
    ) {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                tokio::select! {
                    result = run.as_mut() => panic!("Shell finished before readiness: {result:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {
                        if std::fs::read_to_string(directory.join("ready"))
                            .is_ok_and(|pid| pid.trim().parse::<u32>().is_ok())
                        {
                            break;
                        }
                    }
                }
            }
        })
        .await
        .unwrap();
    }
    #[cfg(windows)]
    async fn assert_windows_shell_stopped(directory: &Path) {
        let pid: u32 = std::fs::read_to_string(directory.join("ready"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut pids = vec![pid.to_string()];
        if let Ok(parent) = std::fs::read_to_string(directory.join("parent")) {
            pids.push(parent.trim().parse::<u32>().unwrap().to_string());
        }
        let pids = pids.join(",");
        // Release any surviving process so a broken cleanup produces a visible side effect.
        // Wait for termination rather than assuming it finishes within a fixed sleep.
        // Kill a survivor on failure so the regression itself cannot leave an orphan.
        let probe = format!(
            "Set-Content release go; \
             foreach ($processId in @({pids})) {{ \
                 $process = Get-Process -Id $processId -ErrorAction SilentlyContinue; \
                 if ($process -and -not $process.WaitForExit(5000)) {{ \
                     Stop-Process -Id $processId -Force; exit 1 \
                 }} \
             }}; \
             if (Test-Path late-write) {{ exit 2 }}; exit 0"
        );
        let result = shell_with_timeout(directory, &probe, Duration::from_secs(15)).await;
        let wrote = directory.join("late-write").exists();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(result.is_ok(), "Process survived cleanup: {result:?}");
        assert!(!wrote);
    }
    #[cfg(windows)]
    const WINDOWS_WAIT_FOR_RELEASE: &str = r#"
$deadline = [DateTime]::UtcNow.AddSeconds(30)
while (-not (Test-Path release)) {
    if ([DateTime]::UtcNow -gt $deadline) { exit 4 }
    Start-Sleep -Milliseconds 20
}
Set-Content late-write done
"#;
    #[cfg(windows)]
    fn windows_descendant_command() -> String {
        // Both processes wait on test-controlled gates. Once readiness is observed, tests
        // can let the parent exit without polling the shell future or stopping its child.
        format!(
            r#"
$childScript = @'
Set-Content ready.tmp $PID
Move-Item ready.tmp ready
{WINDOWS_WAIT_FOR_RELEASE}
'@
$encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($childScript))
Set-Content parent.tmp $PID
Move-Item parent.tmp parent
Start-Process powershell.exe -NoNewWindow -ArgumentList "-NoLogo -NoProfile -NonInteractive -EncodedCommand $encoded"
$deadline = [DateTime]::UtcNow.AddSeconds(30)
while (-not (Test-Path parent-release)) {{
    if ([DateTime]::UtcNow -gt $deadline) {{ exit 3 }}
    Start-Sleep -Milliseconds 20
}}
exit 0
"#
        )
    }
    #[cfg(windows)]
    async fn exit_windows_parent(directory: &Path) {
        let parent: u32 = std::fs::read_to_string(directory.join("parent"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::fs::write(directory.join("parent-release"), "go").unwrap();
        let probe = format!(
            "$parent = Get-Process -Id {parent} -ErrorAction SilentlyContinue; \
             if ($parent -and -not $parent.WaitForExit(5000)) {{ exit 1 }}; exit 0"
        );
        assert!(
            shell_with_timeout(directory, &probe, Duration::from_secs(15))
                .await
                .is_ok()
        );
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn cancelling_windows_shell_stops_side_effects() {
        let directory = windows_shell_test_directory();
        let command = format!(
            "Set-Content ready.tmp $PID\nMove-Item ready.tmp ready\n{WINDOWS_WAIT_FOR_RELEASE}"
        );
        let mut run = Box::pin(shell(&directory, &command));
        wait_for_windows_shell_start(&mut run, &directory).await;
        drop(run);
        assert_windows_shell_stopped(&directory).await;
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn cancelling_windows_shell_stops_children_after_parent_exit() {
        let directory = windows_shell_test_directory();
        let command = windows_descendant_command();
        let mut run = Box::pin(shell(&directory, &command));
        wait_for_windows_shell_start(&mut run, &directory).await;
        // Leave `run` unpolled while the parent exits. Cleanup must work without observing
        // its exit through child.wait(), and cannot rely on taskkill finding a live parent.
        exit_windows_parent(&directory).await;
        drop(run);
        assert_windows_shell_stopped(&directory).await;
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_shell_timeout_stops_started_descendants() {
        let directory = windows_shell_test_directory();
        let command = windows_descendant_command();
        let mut run = Box::pin(shell_with_timeout(
            &directory,
            &command,
            Duration::from_secs(10),
        ));
        wait_for_windows_shell_start(&mut run, &directory).await;
        let result = run.await;
        assert_windows_shell_stopped(&directory).await;
        assert!(result.is_err_and(|error| error.to_string().contains("limit")));
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_shell_completion_stops_remaining_children() {
        let directory = windows_shell_test_directory();
        let command = windows_descendant_command();
        let mut run = Box::pin(shell_with_timeout(
            &directory,
            &command,
            Duration::from_secs(20),
        ));
        wait_for_windows_shell_start(&mut run, &directory).await;
        exit_windows_parent(&directory).await;
        // The child still holds the output pipes. Parent completion must stop it before
        // capture waits for EOF, rather than waiting for the command's timeout to fire.
        let result = run.await;
        assert_windows_shell_stopped(&directory).await;
        assert!(result.is_ok(), "Shell did not complete: {result:?}");
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_shell_stops_descendant_side_effects() {
        let directory = std::env::temp_dir().join(format!(
            "blackwall-shell-cancel-{}",
            UnwrapErr(SysRng).next_u64()
        ));
        std::fs::create_dir(&directory).unwrap();
        let result = tokio::time::timeout(
            Duration::from_millis(30),
            shell(&directory, "sleep 0.3; touch late-write"),
        )
        .await;
        assert!(result.is_err());
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert!(!directory.join("late-write").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
