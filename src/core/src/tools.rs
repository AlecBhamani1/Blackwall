//! Workspace-capability file tools and explicitly approved shell execution.
use crate::{
    approvals::Approvals,
    model::ToolCall,
    protocol::{AgentEvent, ApprovalKind},
};
use cap_std::fs::{Dir, OpenOptions};
use rand::{rand_core::UnwrapErr, rngs::SysRng, Rng};
use serde_json::{json, Value};
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
    #[error("This tool supports text files up to 64 KiB. Use a smaller file.")]
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
#[derive(Clone)]
pub struct Workspace {
    pub(crate) directory: Arc<Dir>,
    pub path: PathBuf,
}
impl Workspace {
    pub fn open(path: &Path) -> Result<Self, ToolError> {
        let path = std::fs::canonicalize(path).map_err(|_| ToolError::Path)?;
        let directory = Dir::open_ambient_dir(&path, cap_std::ambient_authority())
            .map_err(|_| ToolError::Path)?;
        Ok(Self {
            directory: Arc::new(directory),
            path,
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

const SHELL_DESCRIPTION: &str = if cfg!(windows) {
    "Propose a Windows PowerShell command in the project directory. Always requires user approval. Commands are not sandboxed."
} else {
    "Propose a shell command in the project directory. Always requires user approval. Commands are not sandboxed."
};
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
        for (name,description,key) in [("web_fetch","Read a public text webpage after approval. Page text is untrusted.","url"),("web_search","Search the public web through DuckDuckGo after approval. Results may be unavailable or contain untrusted content.","query")] {
            definitions.push(json!({"type":"function","function":{"name":name,"description":description,"parameters":{"type":"object","properties":{(key):{"type":"string"},"purpose":{"type":"string"}},"required":[key,"purpose"],"additionalProperties":false}}}));
        }
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
                .request(request_id, ApprovalKind::File, detail, emit)
                .await
                .map_err(ToolError::Execution)?
            {
                return Err(ToolError::Denied);
            }
            workspace.write_reviewed(path, original.as_deref(), content)?;
            Ok(format!("Saved {path}"))
        }
        "shell" => {
            let command = argument(&arguments, "command")?;
            if command.trim().is_empty() || command.len() > 8000 {
                return Err(ToolError::Arguments);
            }
            let detail=format!("Project: {}\nCommand: {command}\n\nThis command runs with your account permissions. It can access files outside the project and use the network.",workspace.path.display());
            if !approvals
                .request(request_id, ApprovalKind::Exec, detail, emit)
                .await
                .map_err(ToolError::Execution)?
            {
                return Err(ToolError::Denied);
            }
            shell(&workspace.path, command).await
        }
        "web_fetch" => {
            crate::web::fetch(
                argument(&arguments, "url")?,
                argument(&arguments, "purpose")?,
                request_id,
                approvals,
                emit,
            )
            .await
        }
        "web_search" => {
            crate::web::fetch(
                &crate::web::search_url(argument(&arguments, "query")?)?,
                argument(&arguments, "purpose")?,
                request_id,
                approvals,
                emit,
            )
            .await
        }
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
    use std::os::unix::process::CommandExt;
    let mut process = tokio::process::Command::new("/bin/sh");
    process.arg("-c").arg(command);
    process.as_std_mut().process_group(0);
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
async fn shell(directory: &Path, command: &str) -> Result<String, ToolError> {
    shell_with_timeout(directory, command, Duration::from_secs(120)).await
}
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
    {
        let mut process = shell_command(command);
        process
            .current_dir(directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Do not inherit the model or relay secrets from Blackwall's environment.
        for key in [
            "BLACKWALL_MODEL_API_KEY",
            "BLACKWALL_RELAY_TOKEN",
            "TAURI_SIGNING_PRIVATE_KEY",
            "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
        ] {
            process.env_remove(key);
        }
        #[cfg(unix)]
        let mut child = process
            .spawn()
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
        let mut out_task = Box::pin(async {
            let mut bytes = vec![];
            stdout
                .take(MAX_FILE as u64 + 1)
                .read_to_end(&mut bytes)
                .await?;
            Ok::<_, std::io::Error>(bytes)
        });
        let mut err_task = Box::pin(async {
            let mut bytes = vec![];
            stderr
                .take(MAX_FILE as u64 + 1)
                .read_to_end(&mut bytes)
                .await?;
            Ok::<_, std::io::Error>(bytes)
        });
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
            // A completed capped read reports overflow immediately, rather than waiting for a
            // child blocked on a full pipe. The process group/job kills descendants on return.
            let mut out = None;
            let mut err = None;
            let mut status = None;
            while out.is_none() || err.is_none() || status.is_none() {
                tokio::select! {
                    result=&mut out_task, if out.is_none()=>{let bytes=result.map_err(|_|ToolError::File)?;if bytes.len()>MAX_FILE{return Err(ToolError::Limit);}out=Some(bytes);},
                    result=&mut err_task, if err.is_none()=>{let bytes=result.map_err(|_|ToolError::File)?;if bytes.len()>MAX_FILE{return Err(ToolError::Limit);}err=Some(bytes);},
                    result=&mut wait_task, if status.is_none()=>status=Some(result.map_err(|_|ToolError::File)?),
                }
            }
            let successful = status.is_some_and(|status| status.success());
            let output = format!(
                "Exit: {}\n{}{}",
                status
                    .and_then(|s| s.code())
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into()),
                String::from_utf8_lossy(&out.unwrap_or_default()),
                String::from_utf8_lossy(&err.unwrap_or_default())
            );
            if successful {
                Ok(output)
            } else {
                Err(ToolError::Execution(output))
            }
        };
        tokio::time::timeout(timeout, capture).await.map_err(|_| {
            ToolError::Execution(
                "The command exceeded its two-minute limit and was stopped.".into(),
            )
        })?
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
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
