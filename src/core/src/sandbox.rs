//! Owner-selected execution boundaries. Approval never disables the selected sandbox.
use crate::tools::{ToolError, Workspace};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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

/// Prepare `process` for the owner's execution profile and give it its environment: the minimal
/// sandbox one, or, when unsandboxed, the one for `unsandboxed` (the shell's own, or the owner
/// one plus the `pass_through` variables).
pub(crate) fn wrap(
    workspace: &Workspace,
    process: tokio::process::Command,
    unsandboxed: crate::child_env::Scope,
    pass_through: &[String],
) -> Result<tokio::process::Command, ToolError> {
    wrap_with_backend(
        workspace,
        process,
        Path::new("/usr/bin/sandbox-exec"),
        unsandboxed,
        pass_through,
    )
}
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
        Err(ToolError::Execution("Sandboxed commands are not supported on this platform. Choose unsandboxed explicitly to run with full account access.".into()))
    }
    #[cfg(target_os = "macos")]
    {
        if !backend.is_file() {
            return Err(ToolError::Execution("The macOS sandbox backend is unavailable. The command was not run. Choose unsandboxed explicitly only if full account access is intended.".into()));
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

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unsupported_platform_fails_closed_but_explicit_opt_out_is_available() {
        let mut workspace = Workspace::open(&std::env::temp_dir()).unwrap();
        let owner = crate::child_env::Scope::Owner;
        assert!(wrap(
            &workspace,
            tokio::process::Command::new("unused"),
            owner,
            &[]
        )
        .is_err());
        workspace.execution.profile = ExecutionProfile::Unsandboxed;
        assert!(wrap(
            &workspace,
            tokio::process::Command::new("unused"),
            owner,
            &[]
        )
        .is_ok());
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
                self.run_backend(command, Path::new("/usr/bin/sandbox-exec"))
                    .await
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
            let process = wrap(
                &project.workspace,
                process,
                crate::child_env::Scope::Owner,
                &[],
            )
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
