//! Owner-selected execution boundaries. Approval never disables the selected sandbox.
use crate::tools::{ToolError, Workspace};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
mod bwrap;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
mod seccomp;
#[cfg(target_os = "linux")]
mod walk;

#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
#[cfg(target_os = "macos")]
const MACOS_UNAVAILABLE: &str = "The macOS sandbox backend is unavailable. The command was not run. Choose unsandboxed explicitly only if full account access is intended.";
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const UNSUPPORTED: &str = "Sandboxed commands are not supported on this platform. Choose unsandboxed explicitly to run with full account access.";
/// Until the next update, Linux runs only the Read-only profile in a sandbox.
pub const LINUX_WORKSPACE_WRITE_LATER: &str =
    "Workspace-write on Linux arrives in the next update. Choose Read-only or Unsandboxed.";

/// Whether the owner's sandboxed profiles can run on this machine, for the profile selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxStatus {
    pub available: bool,
    /// Why sandboxed commands can't run here, when they can't.
    pub note: Option<&'static str>,
    /// Whether Workspace-write can run here too. Not yet on Linux.
    pub workspace_write: bool,
}

/// Whether the owner's sandboxed profiles can run on this machine, where the owner's choice of
/// profile depends on it: Linux only, since there whether bubblewrap works varies by computer.
/// Elsewhere it is `None` and nothing is gated. On Linux the first call looks for a working
/// bubblewrap, so it can block for a few seconds: call it off the async workers (`spawn_blocking`).
pub fn selector_status(data_directory: &Path) -> Option<SandboxStatus> {
    #[cfg(target_os = "linux")]
    {
        Some(linux_status(linux::availability(data_directory)))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = data_directory;
        None
    }
}
#[cfg(target_os = "linux")]
fn linux_status(availability: &linux::Availability) -> SandboxStatus {
    SandboxStatus {
        available: matches!(availability, linux::Availability::Available { .. }),
        note: availability.note(),
        workspace_write: false,
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionProfile {
    #[default]
    ReadOnly,
    WorkspaceWrite,
    Unsandboxed,
}
impl ExecutionProfile {
    pub fn name(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::Unsandboxed => "unsandboxed",
        }
    }
}
impl std::str::FromStr for ExecutionProfile {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "read-only" => Ok(Self::ReadOnly),
            "workspace-write" => Ok(Self::WorkspaceWrite),
            "unsandboxed" => Ok(Self::Unsandboxed),
            _ => Err("Choose read-only, workspace-write, or unsandboxed."),
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct ExecutionPolicy {
    pub profile: ExecutionProfile,
    pub network: bool,
    /// The actual data directory, including installations using a custom data location.
    pub data_directory: Option<PathBuf>,
    /// Where owner command tools run. Only the owner's saved setting sets it.
    pub command_tools: crate::command_tools::Isolation,
}
impl ExecutionPolicy {
    pub fn description(&self) -> String {
        if self.profile == ExecutionProfile::Unsandboxed {
            "UNSANDBOXED: full account filesystem and network access.".into()
        } else {
            format!(
                "{}; command network {}. Approval cannot expand this boundary.",
                self.profile.name(),
                if self.network { "enabled" } else { "disabled" }
            )
        }
    }
}

/// What a sandboxed command leaves behind until it ends: its run folder
/// (`<data>/staging/<run-id>`, holding the seccomp filter and the bubblewrap arguments) on Linux,
/// nothing elsewhere. [`RunGuard::finish`] removes it, and so does dropping the guard, so a
/// cancelled command cleans up too. On Linux it also holds the checked project's descriptor
/// until [`RunGuard::spawn`] hands it to bubblewrap.
pub(crate) struct RunGuard {
    run: Option<PathBuf>,
    #[cfg(target_os = "linux")]
    project: Option<std::os::fd::OwnedFd>,
}
impl RunGuard {
    fn empty() -> Self {
        Self {
            run: None,
            #[cfg(target_os = "linux")]
            project: None,
        }
    }
    /// Start the command [`prepare`] returned. Unix only: Windows starts commands its own way. A Linux sandbox gets the project descriptor its
    /// arguments name, which is closed here once bubblewrap has started.
    #[cfg(unix)]
    pub(crate) fn spawn(
        &mut self,
        process: &mut tokio::process::Command,
    ) -> std::io::Result<tokio::process::Child> {
        #[cfg(target_os = "linux")]
        if let Some(project) = self.project.take() {
            return linux::spawn_with(project, || process.spawn());
        }
        process.spawn()
    }
    /// Remove the run folder and hand back the command's outcome.
    pub(crate) async fn finish(
        mut self,
        outcome: Result<String, ToolError>,
    ) -> Result<String, ToolError> {
        self.remove();
        outcome
    }
    fn remove(&mut self) {
        if let Some(run) = self.run.take() {
            let _ = std::fs::remove_dir_all(run);
        }
    }
}
impl Drop for RunGuard {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Prepare `process` for the owner's execution profile and give it its environment: the minimal
/// sandbox one, or, when unsandboxed, the one for `unsandboxed` (the shell's own, or the owner
/// one plus the `pass_through` variables). Spawn the result from the async runtime with the guard's
/// [`RunGuard::spawn`], and pass its outcome through the guard's `finish`.
pub(crate) async fn prepare(
    workspace: &Workspace,
    process: tokio::process::Command,
    unsandboxed: crate::child_env::Scope,
    pass_through: &[String],
) -> Result<(tokio::process::Command, RunGuard), ToolError> {
    #[cfg(target_os = "linux")]
    {
        let availability = if workspace.execution.profile == ExecutionProfile::Unsandboxed {
            // Unsandboxed needs no bubblewrap.
            &linux::Availability::Unavailable
        } else {
            find_availability(workspace).await?
        };
        prepare_with(workspace, process, unsandboxed, pass_through, availability).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        #[cfg(target_os = "macos")]
        let backend = Path::new(SANDBOX_EXEC);
        #[cfg(not(target_os = "macos"))]
        let backend = Path::new("");
        wrap_with_backend(workspace, process, backend, unsandboxed, pass_through)
            .map(|process| (process, RunGuard::empty()))
    }
}

/// Whether a command under the owner's profile could start now, so the owner is never asked to
/// approve one that can't. Linux only: elsewhere the backend checks when the command starts, as
/// before. [`prepare`] checks again.
pub(crate) async fn ready(workspace: &Workspace) -> Result<(), ToolError> {
    #[cfg(target_os = "linux")]
    {
        if workspace.execution.profile == ExecutionProfile::Unsandboxed {
            return Ok(());
        }
        ready_for(workspace, find_availability(workspace).await?).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = workspace;
        Ok(())
    }
}
/// [`ready_with`], then the project's location (not its contents, so it is quick): a project that
/// is the home folder, or holds Blackwall's data, fails before the prompt rather than after it.
#[cfg(target_os = "linux")]
async fn ready_for(
    workspace: &Workspace,
    availability: &linux::Availability,
) -> Result<(), ToolError> {
    ready_with(workspace, availability)?;
    let project = workspace.path.clone();
    let data = workspace.execution.data_directory.clone();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    tokio::task::spawn_blocking(move || {
        walk::check_location(&project, home.as_deref(), data.as_deref())
    })
    .await
    .map_err(|_| ToolError::Execution(PREPARE_FAILED.into()))?
}
/// Bubblewrap is there and the profile is one Linux can sandbox.
#[cfg(target_os = "linux")]
fn ready_with(workspace: &Workspace, availability: &linux::Availability) -> Result<(), ToolError> {
    if !matches!(availability, linux::Availability::Available { .. }) {
        let note = availability.note().unwrap_or(linux::LINUX_UNAVAILABLE);
        return Err(ToolError::Execution(note.into()));
    }
    if workspace.execution.profile == ExecutionProfile::WorkspaceWrite {
        return Err(ToolError::Execution(LINUX_WORKSPACE_WRITE_LATER.into()));
    }
    Ok(())
}
/// [`linux::availability`] for the workspace's data folder, off the async workers.
#[cfg(target_os = "linux")]
async fn find_availability(
    workspace: &Workspace,
) -> Result<&'static linux::Availability, ToolError> {
    let data = workspace
        .execution
        .data_directory
        .clone()
        .ok_or_else(|| ToolError::Execution(NO_DATA_FOLDER.into()))?;
    tokio::task::spawn_blocking(move || linux::availability(&data))
        .await
        .map_err(|_| ToolError::Execution(PREPARE_FAILED.into()))
}

#[cfg(target_os = "linux")]
const PREPARE_FAILED: &str =
    "Blackwall couldn't prepare the sandbox for this command. The command was not run.";
#[cfg(target_os = "linux")]
const NO_DATA_FOLDER: &str =
    "Blackwall couldn't find its data folder, so it won't sandbox this command.";
#[cfg(target_os = "linux")]
const WHOLE_COMPUTER: &str = "Blackwall won't sandbox the whole computer. Choose a project folder.";
#[cfg(target_os = "linux")]
const PROJECT_CHANGED: &str =
    "The project folder changed while Blackwall was checking it. The command was not run.";

/// [`prepare`] with `availability` given, rather than found. Linux only: Read-only runs under
/// bubblewrap, and anything that can't be checked or set up fails closed without running.
#[cfg(target_os = "linux")]
pub(crate) async fn prepare_with(
    workspace: &Workspace,
    mut process: tokio::process::Command,
    unsandboxed: crate::child_env::Scope,
    pass_through: &[String],
    availability: &linux::Availability,
) -> Result<(tokio::process::Command, RunGuard), ToolError> {
    use crate::child_env::{from_process, Scope};
    let execution = &workspace.execution;
    if execution.profile == ExecutionProfile::Unsandboxed {
        from_process(unsandboxed, &workspace.path, pass_through).apply(&mut process);
        return Ok((process, RunGuard::empty()));
    }
    ready_with(workspace, availability)?;
    let linux::Availability::Available { bwrap, .. } = availability else {
        return Err(ToolError::Execution(linux::LINUX_UNAVAILABLE.into()));
    };
    let bwrap = bwrap.clone();
    // `Workspace::open` canonicalizes, so `/` is the whole file system however it was named. The
    // location check refuses it too (it holds the home folder); this doesn't depend on that.
    if workspace.path.parent().is_none() {
        return Err(ToolError::Execution(WHOLE_COMPUTER.into()));
    }
    let data = execution
        .data_directory
        .clone()
        .ok_or_else(|| ToolError::Execution(NO_DATA_FOLDER.into()))?;
    let seccomp =
        seccomp::program().ok_or_else(|| ToolError::Execution(linux::LINUX_UNAVAILABLE.into()))?;
    let (project, network) = (workspace.path.clone(), execution.network);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    // Everything that reads or writes files runs off the async workers. The command itself is
    // spawned later by the caller, from the runtime: bubblewrap's --die-with-parent follows the
    // thread that starts it, and a blocking-pool thread can exit while the command still runs.
    let (guard, run) = tokio::task::spawn_blocking(move || -> Result<_, ToolError> {
        walk::check_location(&project, home.as_deref(), Some(&data))?;
        // Pin the folder first, then walk it: the walk must start at the pinned folder, which is
        // what gets mounted (by descriptor), so the path can't be swapped during or after it.
        let changed = || ToolError::Execution(PROJECT_CHANGED.into());
        let (project_fd, pinned) = linux::pin_project(&project).ok_or_else(changed)?;
        let found = walk::walk(&project, walk::MAX_ENTRIES)?;
        if found.root != pinned {
            return Err(changed());
        }
        let roots = bwrap::host_roots();
        let plan = bwrap::MountPlan {
            project: &project,
            project_fd: std::os::fd::AsRawFd::as_raw_fd(&project_fd),
            network,
            system_masks: bwrap::host_system_masks(&roots),
            roots,
            etc: bwrap::host_etc(),
            masks: &found.masks,
        };
        let failed = |_| ToolError::Execution(PREPARE_FAILED.into());
        let args = bwrap::args_file(&bwrap::args(&plan))
            .ok_or_else(|| ToolError::Execution(PREPARE_FAILED.into()))?;
        let run = linux::run_directory(&data).map_err(failed)?;
        // From here on, dropping the guard removes the folder, even on an early return.
        let guard = RunGuard {
            run: Some(run.clone()),
            project: Some(project_fd),
        };
        linux::write_private(&run.join("seccomp.bpf"), &seccomp::to_bytes(&seccomp))
            .map_err(failed)?;
        linux::write_private(&run.join("args"), &args).map_err(failed)?;
        Ok((guard, run))
    })
    .await
    .map_err(|_| ToolError::Execution(PREPARE_FAILED.into()))??;
    let original = process.as_std();
    let program_args: Vec<std::ffi::OsString> =
        original.get_args().map(ToOwned::to_owned).collect();
    let project_fd = guard
        .project
        .as_ref()
        .map(std::os::fd::AsRawFd::as_raw_fd)
        .ok_or_else(|| ToolError::Execution(PREPARE_FAILED.into()))?;
    let mut launcher = bwrap::launcher(
        &bwrap,
        &run.join("seccomp.bpf"),
        &run.join("args"),
        project_fd,
        original.get_program(),
        &program_args,
    )
    .ok_or_else(|| ToolError::Execution(PREPARE_FAILED.into()))?;
    // The launcher starts with an empty environment; the command gets only the sandbox set.
    // Owner pass-through names are not honoured inside the sandbox.
    from_process(Scope::Sandboxed, &workspace.path, &[]).apply(&mut launcher);
    Ok((launcher, guard))
}

#[cfg(not(target_os = "linux"))]
fn wrap_with_backend(
    workspace: &Workspace,
    mut process: tokio::process::Command,
    backend: &Path,
    unsandboxed: crate::child_env::Scope,
    pass_through: &[String],
) -> Result<tokio::process::Command, ToolError> {
    use crate::child_env::from_process;
    if workspace.execution.profile == ExecutionProfile::Unsandboxed {
        from_process(unsandboxed, &workspace.path, pass_through).apply(&mut process);
        return Ok(process);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (process, backend, unsandboxed, pass_through);
        Err(ToolError::Execution(UNSUPPORTED.into()))
    }
    #[cfg(target_os = "macos")]
    {
        if !backend.is_file() {
            return Err(ToolError::Execution(MACOS_UNAVAILABLE.into()));
        }
        let policy = seatbelt_policy(workspace)?;
        let original = process.as_std();
        let mut sandbox = tokio::process::Command::new(backend);
        sandbox
            .arg("-p")
            .arg(policy)
            .arg(original.get_program())
            .args(original.get_args());
        // Do not hand application credentials, shell startup hooks, or loader injection variables
        // to project programs. The OS policy also blocks credential stores and unrelated IPC.
        // Owner pass-through names are not honoured inside the sandbox.
        from_process(crate::child_env::Scope::Sandboxed, &workspace.path, &[]).apply(&mut sandbox);
        Ok(sandbox)
    }
}

#[cfg(target_os = "macos")]
fn quoted(path: &Path) -> Result<String, ToolError> {
    let value = path
        .to_str()
        .filter(|s| !s.chars().any(char::is_control))
        .ok_or_else(|| {
            ToolError::Execution(
                "The project or data path cannot be represented safely by the sandbox.".into(),
            )
        })?;
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

#[cfg(target_os = "macos")]
fn seatbelt_policy(workspace: &Workspace) -> Result<String, ToolError> {
    // Closed by default, including network, Unix sockets, Mach services and process inspection.
    // Fork/exec inherit Seatbelt. No sandbox extension, task port or service-launch privileges.
    let mut policy = String::from(
        r#"(version 1)
(deny default)
(allow process-exec process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
; Block session/group escape through both direct syscalls and posix_spawn attributes.
; Darwin syscall numbers are stable on supported Intel and Apple Silicon macOS.
(deny syscall-unix (syscall-number 82) (syscall-number 147) (syscall-number 244))
(allow sysctl-read)
(allow file-read-metadata)
(allow file-read* (subpath "/System") (subpath "/usr") (subpath "/bin") (subpath "/sbin") (subpath "/opt/homebrew") (subpath "/Library/Apple") (subpath "/private/var/db/dyld") (literal "/dev/null") (literal "/dev/random") (literal "/dev/urandom"))
(allow file-write-data (literal "/dev/null"))
"#,
    );
    let root = quoted(&workspace.path)?;
    // dyld and shells open the path's ancestor directories during startup/getcwd.
    for parent in workspace.path.ancestors().skip(1) {
        policy.push_str(&format!(
            "(allow file-read* (literal {}))\n",
            quoted(parent)?
        ));
    }
    policy.push_str(&format!("(allow file-read* (subpath {root}))\n"));
    if workspace.execution.profile == ExecutionProfile::WorkspaceWrite {
        policy.push_str(&format!("(allow file-write* (subpath {root}))\n"));
    }
    // Denials override the workspace grant, including when the owner selects their home or /.
    // Match names at every depth so renaming a containing directory cannot expose metadata.
    policy.push_str(r#"(deny file-read* file-write* (regex #"(^|/)(\.git|\.hg|\.svn|\.blackwall|\.ssh|\.aws|\.gnupg|\.config|\.codex|\.claude|\.agents)(/|$)"))
(deny file-write* (regex #"(^|/)(AGENTS\.md|AGENTS\.override\.md|CLAUDE\.md)(/|$)"))
"#);
    let mut protected = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        protected.extend([
            home.join(".blackwall"),
            home.join("Library"),
            home.join(".netrc"),
            home.join(".npmrc"),
        ]);
    }
    if let Some(data) = &workspace.execution.data_directory {
        // Existing data paths must canonicalize. Never silently drop a failed exclusion.
        protected.push(std::fs::canonicalize(data).map_err(|_| {
            ToolError::Execution(
                "The application data path could not be protected. The command was not run.".into(),
            )
        })?);
    }
    for path in protected {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        policy.push_str(&format!(
            "(deny file-read* file-write* (subpath {}))\n",
            quoted(&path)?
        ));
        // Renaming a parent must not move a protected directory out of its exclusion.
        for parent in path.ancestors().skip(1) {
            policy.push_str(&format!(
                "(deny file-write-unlink (literal {}))\n",
                quoted(parent)?
            ));
        }
    }
    policy.push_str(&format!("(deny file-write-unlink (literal {root}))\n"));
    if workspace.execution.network {
        policy.push_str("(allow network*)\n");
    }
    Ok(policy)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn defaults_and_owner_profile_values_are_strict() {
        assert_eq!(ExecutionProfile::default(), ExecutionProfile::ReadOnly);
        assert!(!ExecutionPolicy::default().network);
        for name in ["read-only", "workspace-write", "unsandboxed"] {
            let profile: ExecutionProfile = name.parse().unwrap();
            assert_eq!(profile.name(), name);
            assert_eq!(
                serde_json::from_str::<ExecutionProfile>(&format!("\"{name}\"")).unwrap(),
                profile
            );
        }
        assert!("full-access".parse::<ExecutionProfile>().is_err());
        assert!(serde_json::from_str::<ExecutionProfile>("\"unknown\"").is_err());
    }

    #[test]
    fn only_linux_gates_the_profile_selector() {
        let data =
            std::env::temp_dir().join(format!("blackwall-selector-{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(&data).unwrap();
        assert_eq!(selector_status(&data).is_some(), cfg!(target_os = "linux"));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn the_pre_approval_check_changes_nothing_off_linux() {
        // The backend still decides when the command starts, as before.
        let mut workspace = Workspace::open(&std::env::temp_dir()).unwrap();
        for profile in [
            ExecutionProfile::ReadOnly,
            ExecutionProfile::WorkspaceWrite,
            ExecutionProfile::Unsandboxed,
        ] {
            workspace.execution.profile = profile;
            assert!(ready(&workspace).await.is_ok());
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn unsupported_platform_fails_closed_but_explicit_opt_out_is_available() {
        let mut workspace = Workspace::open(&std::env::temp_dir()).unwrap();
        let owner = crate::child_env::Scope::Owner;
        assert!(prepare(
            &workspace,
            tokio::process::Command::new("unused"),
            owner,
            &[]
        )
        .await
        .is_err());
        workspace.execution.profile = ExecutionProfile::Unsandboxed;
        assert!(prepare(
            &workspace,
            tokio::process::Command::new("unused"),
            owner,
            &[]
        )
        .await
        .is_ok());
    }

    #[cfg(target_os = "linux")]
    mod linux_backend {
        use super::*;
        use crate::sandbox::linux::{
            Availability, LINUX_BLOCKED, LINUX_RETRYING, LINUX_UNAVAILABLE,
        };
        use std::os::unix::fs::PermissionsExt;
        use std::time::Duration;

        /// `<tmp>/blackwall-linux-sandbox-<random>/{project,data}`, removed when dropped.
        struct Project {
            root: PathBuf,
            workspace: Workspace,
        }
        impl Project {
            fn new() -> Self {
                let root = std::env::temp_dir().join(format!(
                    "blackwall-linux-sandbox-{:016x}",
                    rand::random::<u64>()
                ));
                std::fs::create_dir_all(root.join("project")).unwrap();
                std::fs::create_dir_all(root.join("data")).unwrap();
                let root = std::fs::canonicalize(root).unwrap();
                let mut workspace = Workspace::open(&root.join("project")).unwrap();
                workspace.execution = ExecutionPolicy {
                    profile: ExecutionProfile::ReadOnly,
                    network: false,
                    data_directory: Some(root.join("data")),
                    ..Default::default()
                };
                Self { root, workspace }
            }
            fn data(&self) -> PathBuf {
                self.root.join("data")
            }
            fn file(&self, relative: &str, text: &str) {
                let path = self.workspace.path.join(relative);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, text).unwrap();
            }
            /// The run folders left under `<data>/staging`.
            fn runs_left(&self) -> Vec<String> {
                std::fs::read_dir(self.data().join("staging"))
                    .map(|entries| {
                        entries
                            .flatten()
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .filter(|name| name.starts_with("run-"))
                            .collect()
                    })
                    .unwrap_or_default()
            }
            async fn run_for(
                &self,
                command: &str,
                availability: &Availability,
                timeout: Duration,
            ) -> Result<String, ToolError> {
                let mut process = tokio::process::Command::new("/bin/sh");
                process.args(["-c", command]);
                let (process, mut guard) = prepare_with(
                    &self.workspace,
                    process,
                    crate::child_env::Scope::Shell,
                    &[],
                    availability,
                )
                .await?;
                let outcome = crate::tools::run_with_input(
                    &self.workspace.path,
                    process,
                    None,
                    timeout,
                    &Default::default(),
                    Some(&mut guard),
                )
                .await;
                guard.finish(outcome).await
            }
            async fn run(
                &self,
                command: &str,
                availability: &Availability,
            ) -> Result<String, ToolError> {
                self.run_for(command, availability, Duration::from_secs(10))
                    .await
            }
        }
        impl Drop for Project {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }

        #[tokio::test]
        async fn unavailable_bwrap_fails_closed_with_the_note_and_never_runs() {
            let mut project = Project::new();
            let ran = project.workspace.path.join("ran");
            for (availability, note) in [
                (Availability::Unavailable, LINUX_UNAVAILABLE),
                (Availability::Retrying, LINUX_RETRYING),
                (Availability::Blocked, LINUX_BLOCKED),
            ] {
                let error = project
                    .run("printf x > ran", &availability)
                    .await
                    .unwrap_err();
                assert_eq!(error.to_string(), note);
                assert!(!ran.exists());
            }
            // Even with bubblewrap, Workspace-write isn't on Linux yet, and nothing starts.
            let fake = Availability::Available {
                bwrap: "/no/such/bwrap".into(),
                version: (0, 13, 1),
            };
            project.workspace.execution.profile = ExecutionProfile::WorkspaceWrite;
            let error = project.run("printf x > ran", &fake).await.unwrap_err();
            assert_eq!(error.to_string(), LINUX_WORKSPACE_WRITE_LATER);
            // Data inside the project, and the whole file system as the project, are refused.
            project.workspace.execution.profile = ExecutionProfile::ReadOnly;
            project.workspace.execution.data_directory = Some(project.workspace.path.join(".d"));
            let error = project.run("printf x > ran", &fake).await.unwrap_err();
            assert!(error
                .to_string()
                .contains("data folder is inside this project"));
            let execution = project.workspace.execution.clone();
            project.workspace = Workspace::open(Path::new("/")).unwrap();
            project.workspace.execution = execution;
            project.workspace.execution.data_directory = Some(project.data());
            let error = project.run("printf x > ran", &fake).await.unwrap_err();
            assert_eq!(
                error.to_string(),
                "Blackwall won't sandbox the whole computer. Choose a project folder."
            );
            assert!(!ran.exists());
            assert!(project.runs_left().is_empty());
            // Choosing Unsandboxed still runs, without bubblewrap.
            let mut project = Project::new();
            project.workspace.execution.profile = ExecutionProfile::Unsandboxed;
            project
                .run("printf x > ran", &Availability::Unavailable)
                .await
                .unwrap();
            assert!(project.workspace.path.join("ran").exists());
        }

        #[tokio::test]
        async fn prepare_writes_private_run_files_and_the_guard_removes_them() {
            let project = Project::new();
            project.file(".git/HEAD", "ref");
            let fake = Availability::Available {
                bwrap: "/bin/true".into(),
                version: (0, 13, 1),
            };
            let mut process = tokio::process::Command::new("/bin/sh");
            process.args(["-c", "printf x > ran"]).env("LEAK", "1");
            let (launcher, guard) = prepare_with(
                &project.workspace,
                process,
                crate::child_env::Scope::Shell,
                &[],
                &fake,
            )
            .await
            .unwrap();
            let std = launcher.as_std();
            let argv: Vec<String> = std
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            // seccomp file, args file, the project descriptor's number, bubblewrap, then the
            // original command after `--`.
            assert_eq!(
                argv[5],
                std::os::fd::AsRawFd::as_raw_fd(guard.project.as_ref().unwrap()).to_string()
            );
            assert_eq!(
                argv[6..],
                [
                    "/bin/true",
                    "--args",
                    "4",
                    "--",
                    "/bin/sh",
                    "-c",
                    "printf x > ran"
                ]
            );
            let run = Path::new(&argv[3]).parent().unwrap().to_path_buf();
            assert!(run.starts_with(project.data().join("staging")));
            assert_eq!(Path::new(&argv[4]).parent(), Some(run.as_path()));
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&run), 0o700);
            assert_eq!(mode(Path::new(&argv[3])), 0o600);
            assert_eq!(mode(Path::new(&argv[4])), 0o600);
            let args = std::fs::read(&argv[4]).unwrap();
            assert!(args.starts_with(b"--unshare-all\0"));
            let mask = format!(
                "--tmpfs\0{}\0",
                project.workspace.path.join(".git").display()
            );
            assert!(args.windows(mask.len()).any(|w| w == mask.as_bytes()));
            // The project is mounted from the descriptor the guard holds for the checked folder.
            let fd = guard.project.as_ref().unwrap();
            let bind = format!(
                "--ro-bind-fd\0{}\0{}\0",
                std::os::fd::AsRawFd::as_raw_fd(fd),
                project.workspace.path.display()
            );
            assert!(args.windows(bind.len()).any(|w| w == bind.as_bytes()));
            let ino = |meta: std::fs::Metadata| std::os::unix::fs::MetadataExt::ino(&meta);
            assert_eq!(
                ino(std::fs::File::from(fd.try_clone().unwrap())
                    .metadata()
                    .unwrap()),
                ino(std::fs::metadata(&project.workspace.path).unwrap())
            );
            // The launcher runs from / with only the sandbox environment.
            assert_eq!(std.get_current_dir(), Some(Path::new("/")));
            let names: Vec<String> = std
                .get_envs()
                .map(|(k, _)| k.to_string_lossy().into_owned())
                .collect();
            assert!(!names.contains(&"LEAK".to_owned()));
            assert!(names
                .iter()
                .all(|name| ["HOME", "LANG", "LC_ALL", "TERM", "TMPDIR", "PATH"]
                    .contains(&name.as_str())));
            // finish passes the outcome through and removes the run folder; so does drop.
            assert_eq!(guard.finish(Ok("done".into())).await.unwrap(), "done");
            assert!(!run.exists());
            let (_, guard) = prepare_with(
                &project.workspace,
                tokio::process::Command::new("/bin/true"),
                crate::child_env::Scope::Shell,
                &[],
                &fake,
            )
            .await
            .unwrap();
            assert_eq!(project.runs_left().len(), 1);
            drop(guard);
            assert!(project.runs_left().is_empty());
        }

        #[tokio::test]
        async fn a_command_that_cannot_be_sandboxed_fails_before_any_approval_prompt() {
            use crate::{approvals::Approvals, command_tools::Isolation, protocol::AgentEvent};
            let mut project = Project::new();
            for availability in [
                Availability::Unavailable,
                Availability::Retrying,
                Availability::Blocked,
            ] {
                assert_eq!(
                    ready_with(&project.workspace, &availability)
                        .unwrap_err()
                        .to_string(),
                    availability.note().unwrap()
                );
            }
            let prompts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = prompts.clone();
            let emit = move |event| {
                if matches!(event, AgentEvent::ApprovalRequest { .. }) {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            };
            let approvals = Approvals::default();
            let shell = crate::model::ToolCall {
                id: "shell".into(),
                function: crate::model::ToolFunction {
                    name: "shell".into(),
                    arguments: r#"{"command":"printf x > ran"}"#.into(),
                },
            };
            let tool: crate::command_tools::CommandTool = serde_json::from_value(serde_json::json!({"name":"touch_probe", "description":"Linux fixture", "command":["/usr/bin/touch", "ran"]})).unwrap();
            project.workspace.execution.command_tools = Isolation::Sandboxed;
            // Whatever this host has: no data folder to stage in, and Workspace-write (not on
            // Linux yet, or no bubblewrap at all) both stop the command before the prompt.
            let no_data = project.workspace.execution.clone();
            project.workspace.execution.data_directory = None;
            let mut cases = vec![project.workspace.execution.clone()];
            project.workspace.execution = no_data;
            project.workspace.execution.profile = ExecutionProfile::WorkspaceWrite;
            cases.push(project.workspace.execution.clone());
            for execution in cases {
                project.workspace.execution = execution;
                let timeout = Duration::from_secs(5);
                let cap = Default::default();
                // An unanswered prompt would wait forever, so a time limit means one was shown.
                let shell_result = tokio::time::timeout(
                    timeout,
                    crate::tools::execute(
                        &project.workspace,
                        &shell,
                        "run",
                        &approvals,
                        &emit,
                        timeout,
                        &cap,
                    ),
                )
                .await
                .expect("the shell asked for approval");
                assert!(shell_result.is_err());
                let tool_result = tokio::time::timeout(
                    timeout,
                    tool.run(
                        &project.workspace,
                        "{}",
                        "run",
                        &approvals,
                        &emit,
                        timeout,
                        &cap,
                    ),
                )
                .await
                .expect("the command tool asked for approval");
                assert!(tool_result.is_err());
            }
            // A project that is the home folder fails before the prompt too, even where
            // bubblewrap works. The check doesn't walk the project, so it is cheap.
            let home = PathBuf::from(std::env::var_os("HOME").unwrap());
            project.workspace.execution.profile = ExecutionProfile::ReadOnly;
            let mut at_home = Workspace::open(&home).unwrap();
            at_home.execution = project.workspace.execution.clone();
            let works = Availability::Available {
                bwrap: "/no/such/bwrap".into(),
                version: (0, 13, 1),
            };
            assert_eq!(
                ready_for(&at_home, &works).await.unwrap_err().to_string(),
                "This project folder is your home folder or contains it. Choose a smaller project folder."
            );
            assert!(ready_for(&project.workspace, &works).await.is_ok());
            let shell_result = tokio::time::timeout(
                Duration::from_secs(5),
                crate::tools::execute(
                    &at_home,
                    &shell,
                    "run",
                    &approvals,
                    &emit,
                    Duration::from_secs(5),
                    &Default::default(),
                ),
            )
            .await
            .expect("the shell asked for approval");
            assert!(shell_result.is_err());
            assert_eq!(prompts.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(!project.workspace.path.join("ran").exists());
        }

        #[test]
        fn status_reports_the_note_and_no_workspace_write_on_linux() {
            let available = Availability::Available {
                bwrap: "/usr/lib/blackwall/bwrap".into(),
                version: (0, 13, 1),
            };
            assert_eq!(
                linux_status(&available),
                SandboxStatus {
                    available: true,
                    note: None,
                    workspace_write: false
                }
            );
            for (availability, note) in [
                (Availability::Unavailable, LINUX_UNAVAILABLE),
                (Availability::Retrying, LINUX_RETRYING),
                (Availability::Blocked, LINUX_BLOCKED),
            ] {
                assert_eq!(
                    linux_status(&availability),
                    SandboxStatus {
                        available: false,
                        note: Some(note),
                        workspace_write: false
                    }
                );
            }
            assert_eq!(
                serde_json::to_value(linux_status(&Availability::Unavailable)).unwrap(),
                serde_json::json!({"available": false, "note": LINUX_UNAVAILABLE, "workspaceWrite": false})
            );
        }

        // ---- with the real bubblewrap -------------------------------------------------------

        fn test_bwrap() -> Availability {
            let bwrap = std::env::var_os("BLACKWALL_TEST_BWRAP")
                .expect("set BLACKWALL_TEST_BWRAP to a bubblewrap binary");
            Availability::Available {
                bwrap: std::fs::canonicalize(bwrap).unwrap(),
                version: (0, 13, 1),
            }
        }

        #[tokio::test]
        #[ignore = "needs bubblewrap; run by the linux-sandbox CI job"]
        async fn linux_sandbox_read_only_denies_writes_and_outside_reads() {
            let bw = test_bwrap();
            let project = Project::new();
            project.file("input", "project text");
            std::fs::write(project.root.join("secret"), "outside secret").unwrap();
            let read = project.run("cat input", &bw).await.unwrap();
            assert_eq!(read, "Exit: 0\nproject text");
            assert!(project.run("printf x > out", &bw).await.is_err());
            assert!(!project.workspace.path.join("out").exists());
            let outside = project.run("cat ../secret", &bw).await.unwrap_err();
            assert!(!outside.to_string().contains("outside secret"));
            // Only the standard streams reach the command, and standard input is empty.
            let fds = project.run("ls /proc/$$/fd", &bw).await.unwrap();
            assert_eq!(fds, "Exit: 0\n0\n1\n2\n");
            assert_eq!(project.run("cat", &bw).await.unwrap(), "Exit: 0\n");
            assert!(project.runs_left().is_empty());
        }

        #[tokio::test]
        #[ignore = "needs bubblewrap; run by the linux-sandbox CI job"]
        async fn linux_sandbox_binds_the_project_it_checked_even_if_the_path_is_swapped() {
            let bw = test_bwrap();
            let project = Project::new();
            project.file("input", "CHECKED");
            let mut process = tokio::process::Command::new("/bin/sh");
            process.args(["-c", "cat input; ls -A"]);
            let (process, mut guard) = prepare_with(
                &project.workspace,
                process,
                crate::child_env::Scope::Shell,
                &[],
                &bw,
            )
            .await
            .unwrap();
            // After the check and before the start, the folder at the project's path is swapped
            // for another one, as a process outside the sandbox could.
            let path = &project.workspace.path;
            std::fs::rename(path, project.root.join("checked")).unwrap();
            std::fs::create_dir(path).unwrap();
            std::fs::write(path.join("input"), "SWAPPED").unwrap();
            std::fs::write(path.join("secret"), "SWAPPED").unwrap();
            let outcome = crate::tools::run_with_input(
                path,
                process,
                None,
                Duration::from_secs(10),
                &Default::default(),
                Some(&mut guard),
            )
            .await;
            let output = guard.finish(outcome).await.unwrap();
            assert_eq!(output, "Exit: 0\nCHECKEDinput\n");
        }

        #[tokio::test]
        #[ignore = "needs bubblewrap; run by the linux-sandbox CI job"]
        async fn linux_sandbox_closes_descriptors_blackwall_inherited() {
            use rustix::fs::{Mode, OFlags};
            use rustix::io::{fcntl_dupfd_cloexec, fcntl_setfd, FdFlags};
            let bw = test_bwrap();
            let project = Project::new();
            // Descriptors this process holds without close-on-exec, as when whatever started
            // Blackwall (a CI runner, a desktop launcher) leaves some open: a low one and a high
            // one, since /bin/sh (dash) can't name a descriptor above 9.
            let low = rustix::fs::open("/dev/null", OFlags::RDONLY, Mode::empty()).unwrap();
            let high = fcntl_dupfd_cloexec(&low, 142).unwrap();
            fcntl_setfd(&high, FdFlags::empty()).unwrap();
            let fds = project.run("ls /proc/$$/fd", &bw).await.unwrap();
            assert_eq!(fds, "Exit: 0\n0\n1\n2\n");
            drop((low, high));
        }

        #[tokio::test]
        #[ignore = "needs bubblewrap; run by the linux-sandbox CI job"]
        async fn linux_sandbox_hides_protected_names_data_and_home() {
            let bw = test_bwrap();
            let project = Project::new();
            project.file(".git/HEAD", "HEAD-CONTENT");
            project.file("nested/.git/x", "NESTED-CONTENT");
            std::fs::write(project.data().join("secret"), "DATA-CONTENT").unwrap();
            std::fs::create_dir(project.root.join("outside")).unwrap();
            std::fs::write(project.root.join("outside/key"), "KEY-CONTENT").unwrap();
            std::os::unix::fs::symlink(
                project.root.join("outside"),
                project.workspace.path.join(".ssh"),
            )
            .unwrap();
            let data = project.data().join("secret");
            for command in [
                "cat .git/HEAD".to_owned(),
                "cat nested/.git/x".to_owned(),
                format!("cat {}", data.display()),
                "cat ../data/secret".to_owned(),
                "cat .ssh/key".to_owned(),
            ] {
                let error = project.run(&command, &bw).await.unwrap_err().to_string();
                assert!(!error.contains("-CONTENT"), "{command}: {error}");
            }
            assert_eq!(project.run("ls -A .git", &bw).await.unwrap(), "Exit: 0\n");
            // $HOME is set, but none of the host's home folder is there.
            let home = PathBuf::from(std::env::var_os("HOME").unwrap());
            let inside = project.run(r#"printf %s "$HOME""#, &bw).await.unwrap();
            assert_eq!(inside, format!("Exit: 0\n{}", home.display()));
            let listing = project
                .run(r#"ls -A "$HOME" 2>/dev/null; true"#, &bw)
                .await
                .unwrap();
            // Only the way down to the project, when the project is inside the home folder.
            let expected = project
                .workspace
                .path
                .strip_prefix(&home)
                .ok()
                .and_then(|rest| rest.components().next())
                .map(|first| format!("{}\n", first.as_os_str().to_string_lossy()))
                .unwrap_or_default();
            assert_eq!(listing, format!("Exit: 0\n{expected}"));
        }

        #[tokio::test]
        #[ignore = "needs bubblewrap; run by the linux-sandbox CI job"]
        async fn linux_sandbox_network_off_blocks_local_listener() {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let bw = test_bwrap();
            let mut project = Project::new();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let curl = format!("curl -sS --max-time 1 http://127.0.0.1:{port}/");
            assert!(project.run(&curl, &bw).await.is_err());
            assert!(
                tokio::time::timeout(Duration::from_millis(300), listener.accept())
                    .await
                    .is_err()
            );
            project.workspace.execution.network = true;
            let serve = async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request).await.unwrap();
                stream
                    .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await
                    .unwrap();
            };
            let (output, served) = tokio::join!(
                project.run(&curl, &bw),
                tokio::time::timeout(Duration::from_secs(5), serve)
            );
            served.unwrap();
            assert_eq!(output.unwrap(), "Exit: 0\nok");
        }

        #[tokio::test]
        #[ignore = "needs bubblewrap; run by the linux-sandbox CI job"]
        async fn linux_sandbox_blocks_af_unix_even_with_network_on() {
            let bw = test_bwrap();
            let mut project = Project::new();
            for network in [false, true] {
                project.workspace.execution.network = network;
                let error = project
                    .run(
                        "python3 -c 'import socket; socket.socket(socket.AF_UNIX)'",
                        &bw,
                    )
                    .await
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains("[Errno 1] Operation not permitted"),
                    "network {network}: {error}"
                );
                // Internet sockets are still created; whether they reach anything is the network's call.
                project
                    .run(
                        "python3 -c 'import socket; socket.socket(socket.AF_INET)'",
                        &bw,
                    )
                    .await
                    .unwrap();
            }
        }

        #[tokio::test]
        #[ignore = "needs bubblewrap; run by the linux-sandbox CI job"]
        async fn linux_sandbox_allows_only_connected_unix_socketpairs() {
            let bw = test_bwrap();
            let project = Project::new();
            project.file(
                "socketpair.py",
                r#"import errno
import socket

socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
try:
    socket.socketpair(socket.AF_UNIX, socket.SOCK_DGRAM)
except OSError as error:
    assert error.errno == errno.EPERM
else:
    raise AssertionError("SOCK_DGRAM socketpair succeeded")
"#,
            );
            let output = project.run("python3 socketpair.py", &bw).await.unwrap();
            assert_eq!(output, "Exit: 0\n");
        }

        #[tokio::test]
        #[ignore = "needs bubblewrap; run by the linux-sandbox CI job"]
        async fn linux_sandbox_can_spawn_a_subprocess() {
            let bw = test_bwrap();
            let project = Project::new();
            project
                .run(
                    "python3 -c 'import subprocess; subprocess.run([\"true\"], check=True)'",
                    &bw,
                )
                .await
                .unwrap();
        }

        #[tokio::test]
        #[ignore = "needs bubblewrap; run by the linux-sandbox CI job"]
        async fn linux_sandbox_cancel_and_timeout_kill_setsid_descendants() {
            let bw = test_bwrap();
            let mut project = Project::new();
            project.workspace.execution.network = true;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            project.file(
                "connect.py",
                &format!("import socket\nsocket.create_connection(('127.0.0.1', {port}), 2)\n"),
            );
            let accept = |within: Duration| tokio::time::timeout(within, listener.accept());
            // Control: left alone, a detached descendant does connect back.
            let control =
                "python3 connect.py; setsid sh -c 'sleep 0.3; python3 connect.py' & sleep 2";
            let (output, ready, late) = tokio::join!(
                project.run(control, &bw),
                accept(Duration::from_secs(5)),
                async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    accept(Duration::from_secs(5)).await
                }
            );
            output.unwrap();
            ready.unwrap().unwrap();
            late.unwrap().unwrap();
            let detached =
                "python3 connect.py; setsid sh -c 'sleep 1.5; python3 connect.py' & wait";
            // Cancelled once it has started.
            let mut run = Box::pin(project.run(detached, &bw));
            tokio::select! {
                result = &mut run => panic!("ended before cancellation: {result:?}"),
                ready = accept(Duration::from_secs(5)) => { ready.unwrap().unwrap(); }
            }
            drop(run);
            assert!(
                accept(Duration::from_millis(2500)).await.is_err(),
                "outlived cancellation"
            );
            // Stopped by the time limit.
            let (output, ready) = tokio::join!(
                project.run_for(detached, &bw, Duration::from_millis(800)),
                accept(Duration::from_secs(5))
            );
            assert_eq!(
                output.unwrap_err().to_string(),
                crate::tools::TIME_LIMIT_MESSAGE
            );
            ready.unwrap().unwrap();
            assert!(
                accept(Duration::from_millis(2500)).await.is_err(),
                "outlived the time limit"
            );
            assert!(project.runs_left().is_empty());
        }

        #[tokio::test]
        #[ignore = "needs bubblewrap; run by the linux-sandbox CI job"]
        async fn linux_sandbox_approval_does_not_expand_access() {
            use crate::{
                approvals::Approvals,
                command_tools::Isolation,
                model::{ToolCall, ToolFunction},
                protocol::{AgentEvent, ApprovalDecision, ResolveApprovalRequest},
            };
            let mut project = Project::new();
            // This goes through the real `prepare`, which finds bubblewrap itself.
            let data = project.data();
            let found = tokio::task::spawn_blocking(move || super::linux::availability(&data))
                .await
                .unwrap();
            assert_eq!(
                found,
                &test_bwrap(),
                "install BLACKWALL_TEST_BWRAP root-owned at /usr/lib/blackwall/bwrap"
            );
            project.file("input", "project text");
            project.workspace.execution.command_tools = Isolation::Sandboxed;
            let approvals = Approvals::default();
            let decisions = approvals.clone();
            let allow = move |event| {
                if let AgentEvent::ApprovalRequest {
                    request_id,
                    approval_id,
                    detail,
                    ..
                } = event
                {
                    assert!(detail.contains("Approval cannot expand this boundary"));
                    decisions
                        .resolve(ResolveApprovalRequest {
                            request_id,
                            approval_id,
                            decision: ApprovalDecision::Allow,
                        })
                        .unwrap();
                }
            };
            let shell = |command: &str| ToolCall {
                id: "shell".into(),
                function: ToolFunction {
                    name: "shell".into(),
                    arguments: serde_json::json!({ "command": command }).to_string(),
                },
            };
            let execute = |call: ToolCall| {
                let (workspace, approvals, allow) = (&project.workspace, &approvals, &allow);
                async move {
                    crate::tools::execute(
                        workspace,
                        &call,
                        "run",
                        approvals,
                        allow,
                        Duration::from_secs(10),
                        &Default::default(),
                    )
                    .await
                }
            };
            // The sandbox runs: an allowed read works.
            assert_eq!(
                execute(shell("cat input")).await.unwrap(),
                "Exit: 0\nproject text"
            );
            assert!(execute(shell("printf blocked > approved-write"))
                .await
                .is_err());
            assert!(!project.workspace.path.join("approved-write").exists());
            let tool: crate::command_tools::CommandTool = serde_json::from_value(serde_json::json!({"name":"write_probe", "description":"Linux fixture", "command":["/usr/bin/touch", "{path}"]})).unwrap();
            assert!(tool
                .run(
                    &project.workspace,
                    r#"{"path":"tool-write"}"#,
                    "run",
                    &approvals,
                    &allow,
                    Duration::from_secs(10),
                    &Default::default(),
                )
                .await
                .is_err());
            assert!(!project.workspace.path.join("tool-write").exists());
            assert!(project.runs_left().is_empty());
        }
    }

    #[cfg(target_os = "macos")]
    mod native {
        use super::*;
        use rand::{rand_core::UnwrapErr, rngs::SysRng, Rng};
        use std::time::Duration;

        struct Project {
            root: PathBuf,
            workspace: Workspace,
        }
        impl Project {
            fn new(profile: ExecutionProfile) -> Self {
                let root = std::env::temp_dir().join(format!(
                    "blackwall-sandbox-{:016x}",
                    UnwrapErr(SysRng).next_u64()
                ));
                std::fs::create_dir_all(root.join("project")).unwrap();
                std::fs::create_dir_all(root.join("data")).unwrap();
                let mut workspace = Workspace::open(&root.join("project")).unwrap();
                workspace.execution = ExecutionPolicy {
                    profile,
                    network: false,
                    data_directory: Some(root.join("data")),
                    ..Default::default()
                };
                Self {
                    root: std::fs::canonicalize(root).unwrap(),
                    workspace,
                }
            }
            async fn run(&self, command: &str) -> Result<String, ToolError> {
                self.run_backend(command, Path::new(SANDBOX_EXEC)).await
            }
            async fn run_backend(
                &self,
                command: &str,
                backend: &Path,
            ) -> Result<String, ToolError> {
                let mut process = tokio::process::Command::new("/bin/sh");
                process.args(["-c", command]);
                let process = wrap_with_backend(
                    &self.workspace,
                    process,
                    backend,
                    crate::child_env::Scope::Owner,
                    &[],
                )?;
                crate::tools::run_with_timeout(
                    &self.workspace.path,
                    process,
                    Duration::from_secs(5),
                    &Default::default(),
                )
                .await
            }
        }
        impl Drop for Project {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }

        #[tokio::test]
        async fn native_profiles_enforce_reads_writes_symlinks_and_descendants() {
            let mut project = Project::new(ExecutionProfile::ReadOnly);
            std::fs::write(project.workspace.path.join("input"), "project text").unwrap();
            std::fs::write(project.root.join("secret"), "outside secret").unwrap();
            assert!(project
                .run("/bin/cat input")
                .await
                .unwrap()
                .contains("project text"));
            assert!(project.run("printf blocked > output").await.is_err());
            assert!(project.run("/bin/cat ../secret").await.is_err());
            assert!(project
                .run(&format!(
                    "/bin/cat /System/Volumes/Data{}/secret",
                    project.root.display()
                ))
                .await
                .is_err());
            project.workspace.execution.profile = ExecutionProfile::WorkspaceWrite;
            project
                .run("printf allowed > output; /bin/sh -c 'printf child > child-output'")
                .await
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(project.workspace.path.join("child-output")).unwrap(),
                "child"
            );
            assert!(project.run("printf blocked > ../outside").await.is_err());
            assert!(project
                .run("/bin/sh -c 'printf blocked > ../child-outside'")
                .await
                .is_err());
            std::os::unix::fs::symlink(
                project.root.join("secret"),
                project.workspace.path.join("escape"),
            )
            .unwrap();
            assert!(project.run("printf blocked > escape").await.is_err());
            assert_eq!(
                std::fs::read_to_string(project.root.join("secret")).unwrap(),
                "outside secret"
            );
            assert!(!project.root.join("outside").exists());
            assert!(!project.root.join("child-outside").exists());
        }

        #[tokio::test]
        async fn native_protected_metadata_and_nested_application_data_stay_protected() {
            let mut project = Project::new(ExecutionProfile::WorkspaceWrite);
            for directory in [
                ".git",
                "nested/.git",
                ".agents",
                ".blackwall",
                ".ssh",
                "custom-data",
            ] {
                std::fs::create_dir_all(project.workspace.path.join(directory)).unwrap();
                std::fs::write(
                    project.workspace.path.join(directory).join("secret"),
                    "original",
                )
                .unwrap();
            }
            project.workspace.execution.data_directory =
                Some(project.workspace.path.join("custom-data"));
            for path in [
                ".git/secret",
                "nested/.git/secret",
                ".agents/secret",
                ".blackwall/secret",
                ".ssh/secret",
                "custom-data/secret",
            ] {
                assert!(
                    project.run(&format!("/bin/cat {path}")).await.is_err(),
                    "read {path}"
                );
                assert!(
                    project
                        .run(&format!("printf blocked > {path}"))
                        .await
                        .is_err(),
                    "write {path}"
                );
            }
            assert!(project.run("/bin/mv custom-data moved-data").await.is_err());
            assert!(project.run("/bin/mv .git moved-git").await.is_err());
            std::fs::write(
                project.workspace.path.join("AGENTS.md"),
                "owner instructions",
            )
            .unwrap();
            assert!(project.run("/bin/cat AGENTS.md").await.is_ok());
            assert!(project.run("printf blocked > AGENTS.md").await.is_err());
            assert!(project.run("/bin/rm AGENTS.md").await.is_err());
        }

        #[tokio::test]
        async fn native_network_and_descendant_network_are_denied_until_owner_enables_them() {
            let mut project = Project::new(ExecutionProfile::WorkspaceWrite);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let command = format!("/usr/bin/nc -z -G 1 127.0.0.1 {port}");
            assert!(project.run(&command).await.is_err());
            assert!(project
                .run(&format!("/bin/sh -c '{command}'"))
                .await
                .is_err());
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
            project.workspace.execution.network = true;
            project.run(&command).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_secs(1), listener.accept())
                    .await
                    .is_ok()
            );
        }

        #[tokio::test]
        async fn native_approval_does_not_expand_shell_or_command_tool_access() {
            use crate::{
                approvals::Approvals,
                command_tools::Isolation,
                model::{ToolCall, ToolFunction},
                protocol::{AgentEvent, ApprovalDecision, ResolveApprovalRequest},
            };
            let mut project = Project::new(ExecutionProfile::ReadOnly);
            project.workspace.execution.command_tools = Isolation::Sandboxed;
            let approvals = Approvals::default();
            let decisions = approvals.clone();
            let allow = move |event| {
                if let AgentEvent::ApprovalRequest {
                    request_id,
                    approval_id,
                    detail,
                    ..
                } = event
                {
                    assert!(detail.contains("Approval cannot expand this boundary"));
                    decisions
                        .resolve(ResolveApprovalRequest {
                            request_id,
                            approval_id,
                            decision: ApprovalDecision::Allow,
                        })
                        .unwrap();
                }
            };
            let call = ToolCall {
                id: "shell".into(),
                function: ToolFunction {
                    name: "shell".into(),
                    arguments: r#"{"command":"printf blocked > approved-write"}"#.into(),
                },
            };
            assert!(crate::tools::execute(
                &project.workspace,
                &call,
                "run",
                &approvals,
                &allow,
                Duration::from_secs(5),
                &Default::default()
            )
            .await
            .is_err());
            assert!(!project.workspace.path.join("approved-write").exists());
            let tool: crate::command_tools::CommandTool = serde_json::from_value(serde_json::json!({"name":"write_probe", "description":"Native fixture", "command":["/usr/bin/touch", "{path}"]})).unwrap();
            assert!(tool
                .run(
                    &project.workspace,
                    r#"{"path":"tool-write"}"#,
                    "run",
                    &approvals,
                    &allow,
                    Duration::from_secs(5),
                    &Default::default(),
                )
                .await
                .is_err());
            assert!(!project.workspace.path.join("tool-write").exists());
            project.workspace.execution.profile = ExecutionProfile::WorkspaceWrite;
            tool.run(
                &project.workspace,
                r#"{"path":"tool-write"}"#,
                "next-run",
                &approvals,
                &allow,
                Duration::from_secs(5),
                &Default::default(),
            )
            .await
            .unwrap();
            assert!(project.workspace.path.join("tool-write").exists());
        }

        #[tokio::test]
        async fn native_default_command_tools_leave_the_shell_sandbox_in_place() {
            use crate::{
                approvals::Approvals,
                model::{ToolCall, ToolFunction},
                protocol::{AgentEvent, ApprovalDecision, ResolveApprovalRequest},
            };
            // `command_tools` stays at its default, which runs command tools outside.
            let project = Project::new(ExecutionProfile::ReadOnly);
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
            let call = ToolCall {
                id: "shell".into(),
                function: ToolFunction {
                    name: "shell".into(),
                    arguments: r#"{"command":"printf blocked > approved-write"}"#.into(),
                },
            };
            assert!(crate::tools::execute(
                &project.workspace,
                &call,
                "run",
                &approvals,
                &allow,
                Duration::from_secs(5),
                &Default::default()
            )
            .await
            .is_err());
            assert!(!project.workspace.path.join("approved-write").exists());
        }

        #[tokio::test]
        async fn native_sandboxed_tool_failures_that_are_not_denials_get_no_hint() {
            use crate::{
                approvals::Approvals,
                command_tools::Isolation,
                protocol::{AgentEvent, ApprovalDecision, ResolveApprovalRequest},
            };
            let mut project = Project::new(ExecutionProfile::ReadOnly);
            project.workspace.execution.command_tools = Isolation::Sandboxed;
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
            let run = |command: serde_json::Value, timeout| {
                let tool: crate::command_tools::CommandTool = serde_json::from_value(
                    serde_json::json!({"name":"probe", "description":"Native fixture", "command":command}),
                )
                .unwrap();
                let (workspace, approvals, allow) = (&project.workspace, &approvals, &allow);
                async move {
                    tool.run(
                        workspace,
                        "{}",
                        "run",
                        approvals,
                        allow,
                        timeout,
                        &Default::default(),
                    )
                    .await
                    .unwrap_err()
                    .to_string()
                }
            };
            let exited = run(
                serde_json::json!(["/bin/sh", "-c", "exit 3"]),
                Duration::from_secs(5),
            )
            .await;
            assert_eq!(exited, "Exit: 3\n");
            let timed_out = run(
                serde_json::json!(["/bin/sleep", "5"]),
                Duration::from_secs(1),
            )
            .await;
            assert_eq!(timed_out, crate::tools::TIME_LIMIT_MESSAGE);
        }

        #[tokio::test]
        async fn native_stdin_json_reaches_a_sandboxed_command_tool() {
            use crate::{
                approvals::Approvals,
                command_tools::Isolation,
                protocol::{AgentEvent, ApprovalDecision, ResolveApprovalRequest},
            };
            let mut project = Project::new(ExecutionProfile::WorkspaceWrite);
            project.workspace.execution.command_tools = Isolation::Sandboxed;
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
            let tool: crate::command_tools::CommandTool = serde_json::from_value(serde_json::json!({"name":"echo_input", "description":"Native fixture", "command":["/bin/cat"], "stdin":"json", "parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}})).unwrap();
            let output = tool
                .run(
                    &project.workspace,
                    r#"{"text":"sandboxed"}"#,
                    "run",
                    &approvals,
                    &allow,
                    Duration::from_secs(5),
                    &Default::default(),
                )
                .await
                .unwrap();
            assert_eq!(output, "Exit: 0\n{\"arguments\":{\"text\":\"sandboxed\"}}");
        }

        #[tokio::test]
        async fn native_path_quoting_and_explicit_unsandboxed_selection() {
            let mut project = Project::new(ExecutionProfile::WorkspaceWrite);
            let renamed = project.root.join("project \"quoted\" \\ path");
            std::fs::rename(&project.workspace.path, &renamed).unwrap();
            let execution = project.workspace.execution.clone();
            project.workspace = Workspace::open(&renamed).unwrap();
            project.workspace.execution = execution;
            project.run("printf safe > inside").await.unwrap();
            assert!(project.run("printf blocked > ../outside").await.is_err());
            project.workspace.execution.profile = ExecutionProfile::Unsandboxed;
            project
                .run_backend(
                    "printf owner-selected > ../outside",
                    &project.root.join("missing-backend"),
                )
                .await
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(project.root.join("outside")).unwrap(),
                "owner-selected"
            );
        }

        #[tokio::test]
        async fn native_backend_failure_never_runs_the_payload() {
            let project = Project::new(ExecutionProfile::WorkspaceWrite);
            for backend in [
                project.root.join("missing-backend"),
                PathBuf::from("/usr/bin/false"),
            ] {
                assert!(project
                    .run_backend("printf unsafe > should-not-exist", &backend)
                    .await
                    .is_err());
                assert!(!project.workspace.path.join("should-not-exist").exists());
            }
            let mut process = tokio::process::Command::new("/usr/bin/sandbox-exec");
            process.args([
                "-p",
                "invalid policy",
                "/bin/sh",
                "-c",
                "printf unsafe > should-not-exist",
            ]);
            assert!(crate::tools::run_with_timeout(
                &project.workspace.path,
                process,
                Duration::from_secs(5),
                &Default::default()
            )
            .await
            .is_err());
            assert!(!project.workspace.path.join("should-not-exist").exists());
        }

        #[tokio::test]
        async fn native_descendants_cannot_detach_from_cancellation_group() {
            let project = Project::new(ExecutionProfile::WorkspaceWrite);
            for escape in ["POSIX::setsid()", "POSIX::setpgid(0,0)"] {
                let result = project
                    .run(&format!(
                        "/usr/bin/perl -MPOSIX -e 'if(fork()==0){{my $r={escape}; print defined($r)?$r:-1; exit;}} wait;'"
                    ))
                    .await
                    .unwrap();
                assert!(result.ends_with("-1"), "{escape}: {result}");
            }
            // posix_spawn can change group/session inside the kernel without calling setsid.
            // Exercise that distinct escape path with a small native fixture.
            let source = project.workspace.path.join("spawn.c");
            std::fs::write(&source, include_str!("../tests/fixtures/sandbox_spawn.c")).unwrap();
            let status = std::process::Command::new("/usr/bin/cc")
                .arg(&source)
                .arg("-o")
                .arg(project.workspace.path.join("spawn-probe"))
                .status()
                .unwrap();
            assert!(status.success());
            project.run("./spawn-probe").await.unwrap();
            assert!(!project.workspace.path.join("escaped-spawn").exists());
        }

        #[tokio::test]
        async fn native_cancellation_and_timeout_stop_descendants() {
            let project = Project::new(ExecutionProfile::WorkspaceWrite);
            let command = "(/bin/sleep 1; printf escaped > escaped) & printf ready > ready; wait";
            let mut run = Box::pin(project.run(command));
            tokio::select! {
                result = &mut run => panic!("command ended before cancellation: {result:?}"),
                _ = async {
                    for _ in 0..100 {
                        if project.workspace.path.join("ready").exists() { return; }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    panic!("command never started");
                } => {}
            }
            drop(run);
            tokio::time::sleep(Duration::from_millis(1200)).await;
            assert!(!project.workspace.path.join("escaped").exists());
            let mut process = tokio::process::Command::new("/bin/sh");
            process.args(["-c", "(/bin/sleep 1; printf escaped > timed-out) & wait"]);
            let (process, _guard) = prepare(
                &project.workspace,
                process,
                crate::child_env::Scope::Owner,
                &[],
            )
            .await
            .unwrap();
            assert!(crate::tools::run_with_timeout(
                &project.workspace.path,
                process,
                Duration::from_millis(100),
                &Default::default()
            )
            .await
            .is_err());
            tokio::time::sleep(Duration::from_millis(1200)).await;
            assert!(!project.workspace.path.join("timed-out").exists());
        }
    }
}
