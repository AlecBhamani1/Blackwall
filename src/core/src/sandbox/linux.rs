//! Linux sandbox backend: find a trusted bubblewrap and prove it works on this kernel, for
//! Read-only and, separately, for Workspace-write's overlay.
use std::collections::HashSet;
use std::ffi::{CString, OsStr, OsString};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    /// Read-only commands can run. Workspace-write ones can too only when `workspace_write`: a
    /// real overlay worked in a sandbox (see [`probe_overlay`]). An overlay that doesn't never
    /// takes Read-only away.
    Available {
        bwrap: PathBuf,
        version: (u32, u32, u32),
        workspace_write: bool,
    },
    /// No trusted, new-enough bubblewrap that works here. Asking again won't change it.
    Unavailable,
    /// A trusted bubblewrap failed to start just now. It's asked again after about 30 seconds.
    Retrying,
    /// Every trusted, new-enough bubblewrap ran but could not start a sandbox, as when AppArmor
    /// on Ubuntu 24.04 stops one without a profile. It's asked again after about 30 seconds too.
    Blocked,
}
impl Availability {
    /// What to tell the owner when sandboxed commands can't run, or `None` when they can.
    pub fn note(&self) -> Option<&'static str> {
        match self {
            Self::Available { .. } => None,
            Self::Unavailable => Some(LINUX_UNAVAILABLE),
            Self::Retrying => Some(LINUX_RETRYING),
            Self::Blocked => Some(LINUX_BLOCKED),
        }
    }
    /// What to tell the owner when Workspace-write commands can't run, or `None` when they can.
    pub fn workspace_write_note(&self) -> Option<&'static str> {
        match self {
            Self::Available {
                workspace_write: true,
                ..
            } => None,
            Self::Available { .. } => Some(LINUX_NO_OVERLAY),
            other => other.note(),
        }
    }
}
pub const LINUX_UNAVAILABLE: &str = "Sandboxed commands on Linux need Bubblewrap (bwrap) 0.12 or newer. Install Blackwall's .deb or AppImage, which include it, or a newer bwrap, then restart Blackwall.";
pub const LINUX_RETRYING: &str = "Bubblewrap didn't start just now. Try again in a moment.";
pub const LINUX_BLOCKED: &str = "Bubblewrap is installed but this system won't let it start a sandbox. On Ubuntu 24.04 or later, install Blackwall's .deb, which adds the AppArmor profile it needs. Blackwall checks again in a moment.";
pub const LINUX_NO_OVERLAY: &str = "Workspace-write isn't available on this computer: the overlay mount it needs didn't work here. It needs Linux 5.11 or newer, with Blackwall's data folder on a file system that allows overlays. Read-only commands still run in the sandbox.";
/// Also enough for Workspace-write: bubblewrap has had `--overlay` since 0.11.0. The overlay
/// probe, not this number, decides whether it works.
pub(crate) const MIN_VERSION: (u32, u32, u32) = (0, 12, 0);
/// After a Retrying or Blocked answer, ask again no sooner than this.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// How long the overlay probe may take. It mounts an overlay and starts a sandbox on top of the
/// Read-only probe's work, and a busy machine can take well over the 2 seconds that one gets.
const OVERLAY_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// The lowest number a descriptor handed to bubblewrap gets: clear of the standard streams and of
/// the seccomp (3) and argument (4) files that the launcher opens.
const FIRST_HANDED_FD: RawFd = 10;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProbeError {
    #[error("could not start bubblewrap: {0}")]
    Spawn(std::io::Error),
    #[error("could not wait for bubblewrap: {0}")]
    Wait(std::io::Error),
    #[error("bubblewrap did not finish in time")]
    TimedOut,
    #[error("bubblewrap exited with {0}")]
    Failed(std::process::ExitStatus),
    #[error("bubblewrap ran but the command inside did not print the expected output")]
    UnexpectedOutput,
    #[error("could not write the sandbox files: {0}")]
    Files(std::io::Error),
}

/// "bubblewrap X.Y[.Z]" on the first line, nothing else.
pub(crate) fn parse_version(stdout: &str) -> Option<(u32, u32, u32)> {
    let numbers = stdout.lines().next()?.strip_prefix("bubblewrap ")?;
    let mut parts = numbers.trim().split('.');
    let mut next = |required: bool| -> Option<Option<u32>> {
        match parts.next() {
            None if required => None,
            None => Some(None),
            Some(part) if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) => {
                part.parse().ok().map(Some)
            }
            Some(_) => None,
        }
    };
    let major = next(true)??;
    let minor = next(true)??;
    let patch = next(false)?.unwrap_or(0);
    parts.next().is_none().then_some((major, minor, patch))
}

/// Where to look for bubblewrap, best first. Always absolute, never found through PATH.
pub(crate) fn candidates(current_exe: &Path) -> Vec<PathBuf> {
    let bundled = current_exe
        .parent()
        .and_then(Path::parent)
        .map(|prefix| prefix.join("lib/blackwall/bwrap"));
    bundled
        .into_iter()
        .chain(
            ["/usr/lib/blackwall/bwrap", "/usr/bin/bwrap", "/bin/bwrap"]
                .into_iter()
                .map(PathBuf::from),
        )
        .filter(|path| path.is_absolute())
        .collect()
}

/// The mounted AppImage, when the process says it runs from one. Its files may belong to the
/// current user, because the image is a mount that user started. Only that mount gets the
/// exception: every directory above it must still belong to root.
pub(crate) fn appimage_dir(
    appimage: Option<OsString>,
    appdir: Option<OsString>,
    current_exe: &Path,
) -> Option<PathBuf> {
    let appdir = PathBuf::from(appdir.filter(|value| !value.is_empty())?);
    appimage.filter(|value| !value.is_empty())?;
    appdir.is_absolute().then_some(())?;
    let appdir = std::fs::canonicalize(appdir).ok()?;
    // "/" would make every file "inside the image". Blackwall itself must be running from it.
    (appdir.parent().is_some()
        && std::fs::canonicalize(current_exe)
            .ok()?
            .starts_with(&appdir))
    .then_some(appdir)
}

/// Root owns it, or (inside the AppImage mount only) the user running Blackwall does.
fn owner_trusted(owner: u32, euid: u32, in_appimage: bool) -> bool {
    owner == 0 || (in_appimage && owner == euid)
}

fn directory_trusted(directory: &Path, euid: u32, in_appimage: bool) -> bool {
    let Ok(meta) = std::fs::metadata(directory) else {
        return false;
    };
    let mode = meta.mode();
    // /tmp-style directories: anyone may write, but only an entry's owner can replace it.
    let sticky_root = mode & 0o1000 != 0 && meta.uid() == 0;
    meta.is_dir()
        && owner_trusted(meta.uid(), euid, in_appimage)
        && (sticky_root || mode & 0o022 == 0)
}

/// The canonical path of `path` when it is a regular executable that only root (or, in the
/// AppImage mount, the user) can change, is not setuid or setgid, and sits in directories that
/// nobody else can change either. `appimage` is the canonical AppImage mount.
pub(crate) fn trusted_file(path: &Path, appimage: Option<&Path>) -> Option<PathBuf> {
    trusted_file_as(path, appimage, rustix::process::geteuid().as_raw())
}
fn trusted_file_as(path: &Path, appimage: Option<&Path>, euid: u32) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    let inside = |path: &Path| appimage.is_some_and(|mount| path.starts_with(mount));
    let in_appimage = inside(&canonical);
    let meta = std::fs::metadata(&canonical).ok()?;
    let mode = meta.mode();
    (meta.is_file()
        && mode & 0o6000 == 0
        && mode & 0o022 == 0
        && mode & 0o111 != 0
        && owner_trusted(meta.uid(), euid, in_appimage)
        && canonical
            .ancestors()
            .skip(1)
            .all(|directory| directory_trusted(directory, euid, inside(directory))))
    .then_some(canonical)
}

fn kill_group(child: &Child) {
    if let Some(group) = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
}
/// Whether the child has exited, without reaping it (so its process group id stays reserved).
fn has_exited(child: &Child) -> std::io::Result<bool> {
    use rustix::process::{waitid, Pid, WaitId, WaitIdOptions};
    let Some(pid) = i32::try_from(child.id()).ok().and_then(Pid::from_raw) else {
        return Err(std::io::Error::other("invalid process id"));
    };
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    Ok(waitid(WaitId::Pid(pid), options)?.is_some())
}
fn kill_and_reap(child: &mut Child) {
    kill_group(child);
    let _ = child.kill();
    let _ = child.wait();
}

/// Run `program` in its own process group with a cleared environment and no stdin, and return its
/// exit status and (at most 4 KiB of) stdout. Blocks for at most `timeout`, then kills the group.
fn run(
    program: &Path,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, Vec<u8>), ProbeError> {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd).env_clear();
    run_command(command, timeout, None)
}
/// [`run`] for a command that already has its program, arguments, directory and environment.
/// `handed`, when given, is passed to it with [`spawn_with`].
fn run_command(
    mut command: Command,
    timeout: Duration,
    handed: Option<OwnedFd>,
) -> Result<(std::process::ExitStatus, Vec<u8>), ProbeError> {
    command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let spawned = match handed {
        Some(fd) => spawn_with(fd, || command.spawn()),
        None => command.spawn(),
    };
    let mut child = spawned.map_err(ProbeError::Spawn)?;
    let Some(stdout) = child.stdout.take() else {
        kill_and_reap(&mut child);
        return Err(ProbeError::UnexpectedOutput);
    };
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::Builder::new()
        .name("bwrap-output".into())
        .spawn(move || {
            let mut output = Vec::new();
            let _ = stdout.take(4096).read_to_end(&mut output);
            let _ = sender.send(output);
        });
    if let Err(error) = reader {
        kill_and_reap(&mut child);
        return Err(ProbeError::Spawn(error));
    }
    let deadline = Instant::now() + timeout;
    loop {
        match has_exited(&child) {
            Ok(true) => break,
            Ok(false) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(false) => {
                kill_and_reap(&mut child);
                return Err(ProbeError::TimedOut);
            }
            Err(error) => {
                kill_and_reap(&mut child);
                return Err(ProbeError::Wait(error));
            }
        }
    }
    // The leader has exited but is not yet reaped, so its group id cannot have been reused:
    // end anything it left behind, then collect it.
    kill_group(&child);
    let status = child.wait().map_err(ProbeError::Wait)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    let output = receiver
        .recv_timeout(remaining.max(Duration::from_millis(50)))
        .map_err(|_| ProbeError::TimedOut)?;
    Ok((status, output))
}

/// The first candidate that is trusted, new enough and able to start a sandbox on this kernel.
/// Without one, the answer is `Unavailable` (final) when no trusted, new-enough candidate failed,
/// `Blocked` when each that failed ran and could not start a sandbox, and `Retrying` when any
/// failed in a way that may pass (it could not be started or waited for, timed out, or its files
/// could not be written). `scratch` is an existing private directory. Blocking: async callers use
/// `spawn_blocking`.
///
/// The second half of the answer is true when the answer is `Available` but the overlay probe
/// failed in a way that may pass (it timed out, could not be started or waited for, or its files
/// could not be written): `workspace_write` is false for now, and should be asked again later.
pub(crate) fn resolve_with_retry(
    candidates: &[PathBuf],
    scratch: &Path,
    timeout: Duration,
    overlay_timeout: Duration,
    appimage: Option<&Path>,
) -> (Availability, bool) {
    let (mut blocked, mut transient) = (false, false);
    let mut seen = HashSet::new();
    for candidate in candidates {
        let Some(bwrap) = trusted_file(candidate, appimage) else {
            continue;
        };
        if !seen.insert(bwrap.clone()) {
            continue;
        }
        // Failing to run a file we trust (out of memory or processes, a busy file, a timeout) can
        // pass; a file that runs and says it is too old or not bubblewrap cannot.
        let Ok((status, stdout)) = run(&bwrap, &["--version"], scratch, timeout) else {
            transient = true;
            continue;
        };
        let version = status
            .success()
            .then(|| parse_version(&String::from_utf8_lossy(&stdout)))
            .flatten();
        let Some(version) = version.filter(|version| *version >= MIN_VERSION) else {
            continue;
        };
        match probe(&bwrap, scratch, timeout) {
            Ok(()) => {
                // Read-only works. Whatever the overlay does, it stays available.
                let overlay = probe_overlay(&bwrap, scratch, overlay_timeout);
                let overlay_may_pass = matches!(
                    overlay,
                    Err(ProbeError::Spawn(_)
                        | ProbeError::Wait(_)
                        | ProbeError::TimedOut
                        | ProbeError::Files(_))
                );
                return (
                    Availability::Available {
                        bwrap,
                        version,
                        workspace_write: overlay.is_ok(),
                    },
                    overlay_may_pass,
                );
            }
            // It ran and refused: policy (AppArmor, disabled user namespaces) most likely.
            Err(ProbeError::Failed(_) | ProbeError::UnexpectedOutput) => blocked = true,
            Err(
                ProbeError::Spawn(_)
                | ProbeError::Wait(_)
                | ProbeError::TimedOut
                | ProbeError::Files(_),
            ) => transient = true,
        }
    }
    let failed = if transient {
        Availability::Retrying
    } else if blocked {
        Availability::Blocked
    } else {
        Availability::Unavailable
    };
    (failed, false)
}

/// [`resolve_with_retry`] without the retry flag, the overlay probe getting the same `timeout`.
#[cfg(test)]
pub(crate) fn resolve(
    candidates: &[PathBuf],
    scratch: &Path,
    timeout: Duration,
    appimage: Option<&Path>,
) -> Availability {
    resolve_with_retry(candidates, scratch, timeout, timeout, appimage).0
}

/// Start a sandbox exactly as a Read-only command gets one (the same mounts, user namespace,
/// `--disable-userns` and seccomp filter, with a new folder in `scratch` as the project) and print
/// `ok` from inside it. This catches kernels and security policies (disabled user namespaces,
/// AppArmor) that allow `--version` but not a sandbox, so such a host is unavailable up front.
pub(crate) fn probe(bwrap: &Path, scratch: &Path, timeout: Duration) -> Result<(), ProbeError> {
    let project = scratch.join(format!("{:016x}", rand::random::<u64>()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&project)
        .map_err(ProbeError::Files)?;
    let result = probe_in(bwrap, &project, timeout);
    let _ = super::staging::remove_dir(&project);
    result
}
fn probe_in(bwrap: &Path, project: &Path, timeout: Duration) -> Result<(), ProbeError> {
    let project_fd = open_directory(project).map_err(ProbeError::Files)?;
    let roots = super::bwrap::host_roots();
    let plan = super::bwrap::MountPlan {
        project,
        project_fd: project_fd.as_raw_fd(),
        profile: crate::sandbox::ExecutionProfile::ReadOnly,
        overlay: None,
        network: false,
        system_masks: super::bwrap::host_system_masks(&roots),
        toolchains: Default::default(),
        roots,
        etc: super::bwrap::host_etc(),
        masks: &[],
    };
    let seccomp = super::seccomp::program().ok_or(ProbeError::UnexpectedOutput)?;
    let args =
        super::bwrap::args_file(&super::bwrap::args(&plan)).ok_or(ProbeError::UnexpectedOutput)?;
    let (seccomp_file, args_file) = (project.join("seccomp.bpf"), project.join("args"));
    write_private(&seccomp_file, &super::seccomp::to_bytes(&seccomp)).map_err(ProbeError::Files)?;
    write_private(&args_file, &args).map_err(ProbeError::Files)?;
    let launcher = super::bwrap::launcher(
        bwrap,
        &seccomp_file,
        &args_file,
        Some(project_fd.as_raw_fd()),
        OsStr::new("/bin/sh"),
        &["-c".into(), "printf ok".into()],
    )
    .ok_or(ProbeError::UnexpectedOutput)?;
    let (status, stdout) = run_command(launcher.into_std(), timeout, Some(project_fd))?;
    if !status.success() {
        return Err(ProbeError::Failed(status));
    }
    if stdout != b"ok" {
        return Err(ProbeError::UnexpectedOutput);
    }
    Ok(())
}

/// Whether Workspace-write works here: start a sandbox exactly as a Workspace-write command gets
/// one, with a real overlay over a new probe project in `scratch` (under `<data>/staging`), and
/// through the merged folder create a file, change one and delete one. Then look in `upper`
/// itself, not at what the command printed: the new file, the changed file and a whiteout (a 0/0
/// character device) for the deleted one must be there, and the project must be unchanged. Any
/// failure, of any kind, means no Workspace-write; it never affects Read-only.
pub(crate) fn probe_overlay(
    bwrap: &Path,
    scratch: &Path,
    timeout: Duration,
) -> Result<(), ProbeError> {
    let dir = scratch.join(format!("overlay-{:016x}", rand::random::<u64>()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(ProbeError::Files)?;
    let result = probe_overlay_in(bwrap, &dir, timeout);
    // The overlay leaves a mode-000 folder in `work`; the remover copes with it.
    let _ = super::staging::remove_dir(&dir);
    result
}
fn probe_overlay_in(bwrap: &Path, dir: &Path, timeout: Duration) -> Result<(), ProbeError> {
    let (project, upper, work) = (dir.join("project"), dir.join("upper"), dir.join("work"));
    for folder in [&project, &upper, &work] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(folder)
            .map_err(ProbeError::Files)?;
    }
    for name in ["changed", "deleted"] {
        write_private(&project.join(name), b"old").map_err(ProbeError::Files)?;
    }
    let roots = super::bwrap::host_roots();
    let plan = super::bwrap::MountPlan {
        project: &project,
        project_fd: -1,
        profile: crate::sandbox::ExecutionProfile::WorkspaceWrite,
        overlay: Some(super::bwrap::Overlay {
            upper: &upper,
            work: &work,
        }),
        network: false,
        system_masks: super::bwrap::host_system_masks(&roots),
        toolchains: Default::default(),
        roots,
        etc: super::bwrap::host_etc(),
        masks: &[],
    };
    let seccomp = super::seccomp::program().ok_or(ProbeError::UnexpectedOutput)?;
    let args =
        super::bwrap::args_file(&super::bwrap::args(&plan)).ok_or(ProbeError::UnexpectedOutput)?;
    let (seccomp_file, args_file) = (dir.join("seccomp.bpf"), dir.join("args"));
    write_private(&seccomp_file, &super::seccomp::to_bytes(&seccomp)).map_err(ProbeError::Files)?;
    write_private(&args_file, &args).map_err(ProbeError::Files)?;
    let script = "printf new > new && printf changed > changed && rm deleted && printf ok";
    let launcher = super::bwrap::launcher(
        bwrap,
        &seccomp_file,
        &args_file,
        None,
        OsStr::new("/bin/sh"),
        &["-c".into(), script.into()],
    )
    .ok_or(ProbeError::UnexpectedOutput)?;
    let (status, stdout) = run_command(launcher.into_std(), timeout, None)?;
    if !status.success() {
        return Err(ProbeError::Failed(status));
    }
    if stdout != b"ok" {
        return Err(ProbeError::UnexpectedOutput);
    }
    let file_holds = |path: &Path, text: &[u8]| {
        std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
            && std::fs::read(path).is_ok_and(|read| read == text)
    };
    let whiteout = std::fs::symlink_metadata(upper.join("deleted")).is_ok_and(|meta| {
        use std::os::unix::fs::FileTypeExt;
        meta.file_type().is_char_device() && meta.rdev() == 0
    });
    let staged = file_holds(&upper.join("new"), b"new")
        && file_holds(&upper.join("changed"), b"changed")
        && whiteout;
    let untouched = file_holds(&project.join("changed"), b"old")
        && file_holds(&project.join("deleted"), b"old")
        && !project.join("new").exists();
    if staged && untouched {
        Ok(())
    } else {
        Err(ProbeError::UnexpectedOutput)
    }
}

/// `directory` as a path-only descriptor (`O_PATH | O_DIRECTORY | O_NOFOLLOW`) for bubblewrap's
/// `--ro-bind-fd`. It is numbered 10 or higher and closed on exec, so only [`spawn_with`] hands it
/// on. A symlink in place of the last name is refused.
pub(crate) fn open_directory(directory: &Path) -> std::io::Result<OwnedFd> {
    use rustix::fs::{Mode, OFlags};
    let path = CString::new(directory.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("a NUL in the path"))?;
    let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let opened = rustix::fs::open(path.as_c_str(), flags, Mode::empty())?;
    Ok(rustix::io::fcntl_dupfd_cloexec(&opened, FIRST_HANDED_FD)?)
}

/// The project for `--ro-bind-fd` and its device and inode, or `None` unless `project` is still its
/// canonical path. Opened before the walk, it pins the folder: the walk must start at the same
/// device and inode, and while the descriptor is open the inode can't be freed and reused.
pub(crate) fn pin_project(project: &Path) -> Option<(OwnedFd, (u64, u64))> {
    let file = std::fs::File::from(open_directory(project).ok()?);
    let meta = file.metadata().ok()?;
    (std::fs::canonicalize(project).ok().as_deref() == Some(project))
        .then(|| (file.into(), (meta.dev(), meta.ino())))
}

/// Whether `project` is still its canonical path and still the folder [`pin_project`] pinned: the
/// last check before a Workspace-write sandbox opens it by path (see the `bwrap` module).
pub(crate) fn still_pinned(project: &Path, pinned: (u64, u64)) -> bool {
    std::fs::canonicalize(project).ok().as_deref() == Some(project)
        && std::fs::symlink_metadata(project)
            .is_ok_and(|meta| meta.is_dir() && (meta.dev(), meta.ino()) == pinned)
}

/// The overlay file system's magic number, from `statfs`.
const OVERLAYFS_SUPER_MAGIC: u64 = 0x794c_7630;
/// Whether `folder` is on an overlay file system (as in a container). The kernel can't stack
/// Workspace-write's overlay on one safely as its lower layer.
#[allow(clippy::unnecessary_cast)] // `f_type`'s type differs between architectures.
pub(crate) fn on_overlayfs(folder: impl std::os::fd::AsFd) -> bool {
    // Unknown counts as overlay: refused.
    rustix::fs::fstatfs(folder).map_or(true, |fs| fs.f_type as u64 == OVERLAYFS_SUPER_MAGIC)
}

const PROJECT_INSIDE_DATA: &str = "This project folder is inside Blackwall's data folder. Choose a project folder outside it for Workspace-write.";
const PROJECT_HOLDS_DATA: &str = "Blackwall's data folder is inside this project. Choose a project folder that doesn't contain Blackwall's data.";
const PROJECT_HOLDS_BWRAP: &str = "This project folder contains the Bubblewrap program Blackwall sandboxes commands with, so Workspace-write can't run in it. Choose a smaller project folder.";
const PROJECT_HOLDS_APP: &str = "This project folder contains Blackwall's own app files, so Workspace-write can't run in it. Choose a project folder outside them.";
pub(crate) const PROJECT_ON_OVERLAY: &str = "This project folder is on an overlay file system (as inside a container), which Workspace-write can't stage. Choose Read-only, or a project folder on another file system.";
pub(crate) const PATH_NOT_MOUNTABLE: &str = "The path of this project folder or of Blackwall's data folder has a comma, colon or backslash in it, which Workspace-write's overlay mount can't take. Choose Read-only, or a folder whose path has none of them.";
const LOCATION_UNRESOLVED: &str =
    "Blackwall couldn't resolve a folder it needs to check, so it won't run this Workspace-write command.";

/// Where a Workspace-write project may not be, on top of `walk::check_location`: inside the data
/// folder (where its staging lives) or holding it, holding the resolved bubblewrap or the
/// AppImage folder (`appdir`, canonical), whose files the apply step could then replace, or on an
/// overlay file system. Nor may the project's or the data folder's resolved path (the staging
/// folders are in the data folder) have a `,`, `:` or `\`: bubblewrap passes the overlay's paths
/// in its mount options, where those mean something. Read-only, mounted by descriptor, is not
/// affected. Blocking (it resolves paths).
pub(crate) fn check_workspace_write_location(
    project: &Path,
    data: &Path,
    bwrap: &Path,
    appdir: Option<&Path>,
) -> Result<(), crate::tools::ToolError> {
    let refuse = |message: &str| Err(crate::tools::ToolError::Execution(message.into()));
    let (Ok(project), Ok(data), Ok(bwrap)) = (
        std::fs::canonicalize(project),
        std::fs::canonicalize(data),
        std::fs::canonicalize(bwrap),
    ) else {
        return refuse(LOCATION_UNRESOLVED);
    };
    let mountable = |path: &Path| {
        !path
            .as_os_str()
            .as_bytes()
            .iter()
            .any(|byte| matches!(byte, b',' | b':' | b'\\'))
    };
    if !mountable(&project) || !mountable(&data) {
        return refuse(PATH_NOT_MOUNTABLE);
    }
    if project.starts_with(&data) {
        return refuse(PROJECT_INSIDE_DATA);
    }
    if data.starts_with(&project) {
        return refuse(PROJECT_HOLDS_DATA);
    }
    if bwrap.starts_with(&project) {
        return refuse(PROJECT_HOLDS_BWRAP);
    }
    if appdir.is_some_and(|appdir| appdir.starts_with(&project)) {
        return refuse(PROJECT_HOLDS_APP);
    }
    let Ok(folder) = open_directory(&project) else {
        return refuse(LOCATION_UNRESOLVED);
    };
    if on_overlayfs(&folder) {
        return refuse(PROJECT_ON_OVERLAY);
    }
    Ok(())
}

/// The AppImage folder Blackwall runs from, if any (see [`appimage_dir`]).
pub(crate) fn current_appdir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    appimage_dir(
        std::env::var_os("APPIMAGE"),
        std::env::var_os("APPDIR"),
        &exe,
    )
}

/// Run `spawn` with `fd` open in the new process at the same number, then close it here. The
/// descriptor is close-on-exec except during this call, and every call takes one lock, so no
/// other sandbox started at the same moment inherits it. A program started at that moment
/// without the sandbox might; it gains nothing it couldn't open itself.
pub(crate) fn spawn_with<T>(
    fd: OwnedFd,
    spawn: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    static SPAWNING: Mutex<()> = Mutex::new(());
    let _alone = SPAWNING.lock().unwrap_or_else(PoisonError::into_inner);
    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty())?;
    let spawned = spawn();
    drop(fd);
    spawned
}

fn real_directory(path: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::other("not a real directory"))
    }
}
fn private_directory(path: &Path) -> std::io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => Err(error),
        _ => real_directory(path),
    }
}

/// Create `path` for this user only (0600) and write `bytes` to it. An existing file is refused.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

/// A fresh private `<data>/staging/probe-<random>` directory. A symlinked data directory or
/// staging directory is refused. Staging folders left over from crashes are removed after a day
/// by the general sweep ([`super::staging::sweep`]).
fn prepare_scratch(data_directory: &Path) -> std::io::Result<PathBuf> {
    private_scratch(data_directory, "probe-")
}
/// A fresh private (0700) `<data>/staging/run-<random>` directory for one sandboxed command's
/// files, with the same checks and the same staging sweep as the probe's.
pub(crate) fn run_directory(data_directory: &Path) -> std::io::Result<PathBuf> {
    private_scratch(data_directory, "run-")
}
/// `<data>/staging`, made private (0700) if it is missing. A symlinked data directory or staging
/// directory is refused.
pub(crate) fn staging_directory(data_directory: &Path) -> std::io::Result<PathBuf> {
    real_directory(data_directory)?;
    let staging = data_directory.join("staging");
    private_directory(&staging)?;
    Ok(staging)
}
/// A fresh private (0700) `<data>/staging/<prefix><16 hex digits from the system's random
/// source>` directory, after the same checks and the one sweep of old folders as the probe's.
pub(crate) fn private_scratch(data_directory: &Path, prefix: &str) -> std::io::Result<PathBuf> {
    use rand::rand_core::TryRng;
    let staging = staging_directory(data_directory)?;
    super::staging::sweep(data_directory, SystemTime::now());
    let random = rand::rngs::SysRng
        .try_next_u64()
        .map_err(|_| std::io::Error::other("no system randomness"))?;
    let scratch = staging.join(format!("{prefix}{random:016x}"));
    std::fs::DirBuilder::new().mode(0o700).create(&scratch)?;
    Ok(scratch)
}

static UNAVAILABLE: Availability = Availability::Unavailable;
static RETRYING: Availability = Availability::Retrying;
static BLOCKED: Availability = Availability::Blocked;

/// What [`availability`] has found so far.
struct Memory {
    /// A working bubblewrap whose Workspace-write answer is settled, kept for the life of the
    /// process.
    found: OnceLock<Availability>,
    /// No trusted, new-enough bubblewrap: kept for the life of the process too.
    never: AtomicBool,
    /// The last Retrying or Blocked answer, or an Available one whose overlay probe failed in a
    /// way that may pass, and when to ask again. Held while looking, so
    /// concurrent first callers wait for one answer instead of probing twice.
    retry: Mutex<Option<(Instant, &'static Availability)>>,
    retry_after: Duration,
}
impl Memory {
    const fn new(retry_after: Duration) -> Self {
        Self {
            found: OnceLock::new(),
            never: AtomicBool::new(false),
            retry: Mutex::new(None),
            retry_after,
        }
    }
    /// The remembered answer, or what `look` finds now. `look` gives `None` when it could not
    /// even get ready to look (no probe folder yet): that is `Retrying` but not remembered, so a
    /// first launch, before the data folder exists, is not stuck with it for 30 seconds. Its
    /// second value is true for an `Available` answer whose Workspace-write half may still pass
    /// (see [`resolve_with_retry`]): Read-only callers get it at once, but it is asked again after
    /// the retry time instead of being kept.
    fn answer(
        &'static self,
        look: impl FnOnce() -> Option<(Availability, bool)>,
    ) -> &'static Availability {
        if let Some(found) = self.found.get() {
            return found;
        }
        if self.never.load(Ordering::Acquire) {
            return &UNAVAILABLE;
        }
        let mut retry = self
            .retry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(found) = self.found.get() {
            return found;
        }
        if self.never.load(Ordering::Acquire) {
            return &UNAVAILABLE;
        }
        if let Some((until, answer)) = *retry {
            if Instant::now() < until {
                return answer;
            }
        }
        let Some((looked, overlay_may_pass)) = look() else {
            return &RETRYING;
        };
        let answer = match looked {
            found @ Availability::Available { .. } if !overlay_may_pass => {
                return self.found.get_or_init(|| found);
            }
            // Read-only works; Workspace-write is off for now. Reuse the last copy when it is
            // the same, so asking again every retry time doesn't keep adding one.
            found @ Availability::Available { .. } => match *retry {
                Some((_, last)) if *last == found => last,
                _ => Box::leak(Box::new(found)),
            },
            Availability::Unavailable => {
                self.never.store(true, Ordering::Release);
                return &UNAVAILABLE;
            }
            Availability::Blocked => &BLOCKED,
            Availability::Retrying => &RETRYING,
        };
        *retry = Some((Instant::now() + self.retry_after, answer));
        answer
    }
}
static MEMORY: Memory = Memory::new(RETRY_AFTER);

/// Whether sandboxed commands can run on this machine. Finding a working bubblewrap and finding
/// none are both remembered for the life of the process (the first data directory wins). Any other
/// failure of a trusted candidate (it would not run, or a new-enough one would not start a
/// sandbox) is not: the answer is `Blocked` or `Retrying`, and it is asked again after about 30
/// seconds. So is Workspace-write when only its overlay probe failed in a way that may pass (a
/// timeout, say): Read-only stays `Available`, with `workspace_write` false until that is asked
/// again. A probe folder that can't be made is
/// `Retrying` and asked again on the next call. A processor the seccomp filter doesn't support is
/// never available. Blocking: async callers use `spawn_blocking`.
pub fn availability(data_directory: &Path) -> &'static Availability {
    if super::seccomp::NATIVE_ARCH.is_none() {
        return &UNAVAILABLE;
    }
    MEMORY.answer(|| {
        let scratch = prepare_scratch(data_directory).ok()?;
        let exe = std::env::current_exe().unwrap_or_default();
        let appimage = appimage_dir(
            std::env::var_os("APPIMAGE"),
            std::env::var_os("APPDIR"),
            &exe,
        );
        let found = resolve_with_retry(
            &candidates(&exe),
            &scratch,
            Duration::from_secs(2),
            OVERLAY_PROBE_TIMEOUT,
            appimage.as_deref(),
        );
        let _ = super::staging::remove_dir(&scratch);
        Some(found)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }
    /// A private, canonical temp directory that is removed when dropped. It doubles as the fake
    /// AppImage mount, so tests do not depend on who owns the files they create.
    struct Root {
        path: PathBuf,
        scratch: PathBuf,
        count: std::cell::Cell<u32>,
    }
    impl Root {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("blackwall-bwrap-{}", rand::random::<u64>()));
            std::fs::create_dir_all(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            let path = std::fs::canonicalize(path).unwrap();
            let scratch = path.join("scratch");
            std::fs::create_dir(&scratch).unwrap();
            Self {
                path,
                scratch,
                count: std::cell::Cell::new(0),
            }
        }
        fn fake_mode(&self, body: &str, mode: u32) -> PathBuf {
            self.count.set(self.count.get() + 1);
            let dir = self.path.join(format!("c{}", self.count.get()));
            std::fs::create_dir(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            let path = dir.join("bwrap");
            write_script(&path, &format!("#!/bin/sh\n{body}\n"));
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        }
        fn fake(&self, body: &str) -> PathBuf {
            self.fake_mode(body, 0o755)
        }
        /// Answers `--version` with `version_line`, and any other call like a working sandbox.
        fn passing(&self, version_line: &str, mode: u32) -> PathBuf {
            self.fake_mode(
                &format!("case $1 in --version) echo '{version_line}';; *) printf ok;; esac"),
                mode,
            )
        }
        fn resolve(&self, candidates: &[PathBuf]) -> Availability {
            resolve(candidates, &self.scratch, secs(2), Some(&self.path))
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
    /// Write a script that a test then runs, from a separate `cp` process. Written from this
    /// process, a test running in parallel could fork while the file is still open for writing,
    /// and its child would hold that descriptor until it execs: running the script in that moment
    /// fails with ETXTBSY ("text file busy"), which the resolver rightly calls transient.
    fn write_script(path: &Path, body: &str) {
        let mut source = path.as_os_str().to_owned();
        source.push(".source");
        std::fs::write(&source, body).unwrap();
        let _ = std::fs::remove_file(path);
        let copied = Command::new("/bin/cp")
            .arg(&source)
            .arg(path)
            .status()
            .unwrap();
        assert!(copied.success());
        std::fs::remove_file(&source).unwrap();
    }

    #[test]
    fn each_answer_but_available_has_its_note() {
        assert_eq!(
            Availability::Blocked.note(),
            Some("Bubblewrap is installed but this system won't let it start a sandbox. On Ubuntu 24.04 or later, install Blackwall's .deb, which adds the AppArmor profile it needs. Blackwall checks again in a moment.")
        );
        assert_eq!(Availability::Retrying.note(), Some(LINUX_RETRYING));
        assert_eq!(
            Availability::Unavailable.note(),
            Some("Sandboxed commands on Linux need Bubblewrap (bwrap) 0.12 or newer. Install Blackwall's .deb or AppImage, which include it, or a newer bwrap, then restart Blackwall.")
        );
        let available = Availability::Available {
            bwrap: "/usr/lib/blackwall/bwrap".into(),
            version: (0, 13, 1),
            workspace_write: true,
        };
        assert_eq!(available.note(), None);
        assert_eq!(available.workspace_write_note(), None);
        // Without the overlay, Read-only still has no note; Workspace-write says why it can't.
        let read_only = Availability::Available {
            bwrap: "/usr/lib/blackwall/bwrap".into(),
            version: (0, 13, 1),
            workspace_write: false,
        };
        assert_eq!(read_only.note(), None);
        assert_eq!(
            read_only.workspace_write_note(),
            Some("Workspace-write isn't available on this computer: the overlay mount it needs didn't work here. It needs Linux 5.11 or newer, with Blackwall's data folder on a file system that allows overlays. Read-only commands still run in the sandbox.")
        );
        for other in [
            Availability::Unavailable,
            Availability::Retrying,
            Availability::Blocked,
        ] {
            assert_eq!(other.workspace_write_note(), other.note());
        }
    }
    #[test]
    fn parse_version_accepts_only_bubblewrap_semver() {
        assert_eq!(parse_version("bubblewrap 0.13.1\n"), Some((0, 13, 1)));
        assert_eq!(parse_version("bubblewrap 1.0"), Some((1, 0, 0)));
        for bad in [
            "bwrap 0.13.1",
            "bubblewrap",
            "bubblewrap x.y",
            "bubblewrap 0.13.1.2",
            "bubblewrap +1.0",
            "",
        ] {
            assert_eq!(parse_version(bad), None);
        }
    }
    #[test]
    fn resolver_skips_each_bad_candidate_and_falls_through_to_the_control() {
        let root = Root::new();
        let control = root.passing("bubblewrap 0.13.1", 0o755);
        let bad = [
            ("too old", root.passing("bubblewrap 0.11.2", 0o755)),
            ("unparseable", root.passing("hi", 0o755)),
            ("world-writable", root.passing("bubblewrap 0.13.1", 0o777)),
            ("group-writable", root.passing("bubblewrap 0.13.1", 0o775)),
            ("setuid", root.passing("bubblewrap 0.13.1", 0o4755)),
            ("setgid", root.passing("bubblewrap 0.13.1", 0o2755)),
            ("not executable", root.passing("bubblewrap 0.13.1", 0o644)),
        ];
        // The bad ones would pass the probe, so only the checks under test can reject them.
        for (why, candidate) in bad {
            assert!(
                matches!(
                    &root.resolve(std::slice::from_ref(&candidate)),
                    Availability::Unavailable
                ),
                "{why} alone"
            );
            assert!(
                matches!(
                    root.resolve(&[candidate, control.clone()]),
                    Availability::Available { bwrap, version: (0, 13, 1), .. } if bwrap == control
                ),
                "{why} then control"
            );
        }
    }
    #[test]
    fn resolver_falls_through_a_failing_probe_to_the_next_candidate() {
        let root = Root::new();
        let blocked =
            root.fake("case $1 in --version) echo 'bubblewrap 0.13.1';; *) exit 1;; esac");
        let ok = root.passing("bubblewrap 0.13.1", 0o755);
        assert!(matches!(
            root.resolve(&[blocked.clone(), ok.clone()]),
            Availability::Available { bwrap, .. } if bwrap == ok
        ));
        // A usable-looking bubblewrap that runs but cannot start a sandbox is blocked (AppArmor on
        // Ubuntu 24.04, say), which is asked again later too.
        assert_eq!(root.resolve(&[blocked]), Availability::Blocked);
    }
    #[test]
    fn a_failing_overlay_probe_leaves_read_only_available() {
        let root = Root::new();
        let new = "case $1 in --version) echo 'bubblewrap 0.13.1';; *) ";
        // Only a Workspace-write sandbox's arguments (fd 4) have --overlay.
        let overlay = "tr '\\0' '\\n' <&4 | grep -qx -- --overlay";
        let fakes = [
            // The kernel or a policy refuses the overlay (EPERM, EINVAL, ENODEV...).
            format!("{new}if {overlay}; then echo 'bwrap: Can'\\''t make overlay mount' >&2; exit 1; fi; printf ok;; esac"),
            // It says it worked, but nothing reached upper.
            format!("{new}printf ok;; esac"),
            // It hangs.
            format!("{new}if {overlay}; then exec sleep 30; fi; printf ok;; esac"),
        ];
        for body in fakes {
            let fake = root.fake(&body);
            let found = resolve(
                std::slice::from_ref(&fake),
                &root.scratch,
                Duration::from_millis(500),
                Some(&root.path),
            );
            assert_eq!(
                found,
                Availability::Available {
                    bwrap: fake.clone(),
                    version: (0, 13, 1),
                    workspace_write: false,
                },
                "{body}"
            );
            assert_eq!(found.note(), None);
            assert_eq!(found.workspace_write_note(), Some(LINUX_NO_OVERLAY));
            assert!(matches!(
                probe_overlay(&fake, &root.scratch, Duration::from_millis(500)),
                Err(ProbeError::Failed(_) | ProbeError::UnexpectedOutput | ProbeError::TimedOut)
            ));
        }
        // The probes clean up after themselves, the overlay's work folder included.
        assert_eq!(std::fs::read_dir(&root.scratch).unwrap().count(), 0);
    }
    #[test]
    fn workspace_write_location_refusals_each_have_their_own_message() {
        let root = Root::new();
        let data = root.path.join("data");
        let project = root.path.join("project");
        let bwrap = root.path.join("bin/bwrap");
        for dir in [&data, &project, &bwrap.parent().unwrap().to_path_buf()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(&bwrap, "").unwrap();
        let check = |project: &Path, data: &Path, bwrap: &Path, appdir: Option<&Path>| {
            check_workspace_write_location(project, data, bwrap, appdir)
                .map_err(|error| error.to_string())
        };
        // tmpfs or ext4 in tests; an overlay project (a container's root) is refused below.
        if !on_overlayfs(open_directory(&project).unwrap()) {
            assert_eq!(check(&project, &data, &bwrap, None), Ok(()));
            assert_eq!(
                check(&project, &data, &bwrap, Some(&root.path.join("app"))),
                Ok(())
            );
        }
        let inside = data.join("staging/p");
        std::fs::create_dir_all(&inside).unwrap();
        assert_eq!(
            check(&inside, &data, &bwrap, None),
            Err(PROJECT_INSIDE_DATA.into())
        );
        assert_eq!(
            check(&data, &data, &bwrap, None),
            Err(PROJECT_INSIDE_DATA.into())
        );
        assert_eq!(
            check(&root.path, &data, &bwrap, None),
            Err(PROJECT_HOLDS_DATA.into())
        );
        let elsewhere = root.path.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let tool = project.join("tools/bwrap");
        std::fs::create_dir_all(tool.parent().unwrap()).unwrap();
        std::fs::write(&tool, "").unwrap();
        assert_eq!(
            check(&project, &elsewhere, &tool, None),
            Err(PROJECT_HOLDS_BWRAP.into())
        );
        // Through a link, the resolved path is what counts.
        let link = root.path.join("link");
        std::os::unix::fs::symlink(&tool, &link).unwrap();
        assert_eq!(
            check(&project, &elsewhere, &link, None),
            Err(PROJECT_HOLDS_BWRAP.into())
        );
        assert_eq!(
            check(&project, &elsewhere, &bwrap, Some(&project.join("tools"))),
            Err(PROJECT_HOLDS_APP.into())
        );
        assert_eq!(
            check(&project, &elsewhere, &bwrap, Some(&project)),
            Err(PROJECT_HOLDS_APP.into())
        );
        assert_eq!(
            check(&root.path.join("missing"), &data, &bwrap, None),
            Err(LOCATION_UNRESOLVED.into())
        );
        // A path bubblewrap would have to put in the overlay's mount options as it is, whether
        // the project's, the data folder's or reached through a link: refused.
        for odd in ["a,b", "a:b", "a\\b"] {
            let folder = root.path.join(odd);
            std::fs::create_dir_all(folder.join("data")).unwrap();
            assert_eq!(
                check(&folder, &data, &bwrap, None),
                Err(PATH_NOT_MOUNTABLE.into()),
                "{odd}"
            );
            assert_eq!(
                check(&project, &folder.join("data"), &bwrap, None),
                Err(PATH_NOT_MOUNTABLE.into()),
                "{odd}"
            );
            let link = root.path.join(format!("link-{}", odd.len()));
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&folder, &link).unwrap();
            assert_eq!(
                check(&link, &data, &bwrap, None),
                Err(PATH_NOT_MOUNTABLE.into()),
                "{odd}"
            );
        }
        // The root of a container is an overlay.
        if on_overlayfs(open_directory(Path::new("/")).unwrap()) {
            assert_eq!(
                check(Path::new("/usr"), &data, &bwrap, None),
                Err(PROJECT_ON_OVERLAY.into())
            );
        }
    }
    #[test]
    fn the_pin_is_checked_again_by_path_before_bubblewrap_starts() {
        let root = Root::new();
        let project = root.path.join("p");
        std::fs::create_dir(&project).unwrap();
        let (_fd, pinned) = pin_project(&project).unwrap();
        assert!(still_pinned(&project, pinned));
        // Swapped for another folder, or for a link to the pinned one: not the project any more.
        std::fs::rename(&project, root.path.join("moved")).unwrap();
        assert!(!still_pinned(&project, pinned));
        std::fs::create_dir(&project).unwrap();
        assert!(!still_pinned(&project, pinned));
        std::fs::remove_dir(&project).unwrap();
        std::os::unix::fs::symlink(root.path.join("moved"), &project).unwrap();
        assert!(!still_pinned(&project, pinned));
    }
    #[test]
    fn resolver_reports_a_timeout_as_transient() {
        let root = Root::new();
        let slow = root.fake("exec sleep 30");
        let resolution = resolve(
            &[slow],
            &root.scratch,
            Duration::from_millis(300),
            Some(&root.path),
        );
        assert_eq!(resolution, Availability::Retrying);
    }
    #[test]
    fn resolver_tells_a_blocked_sandbox_from_a_transient_failure() {
        let root = Root::new();
        let new = "case $1 in --version) echo 'bubblewrap 0.13.1';; *) ";
        let exits = root.fake(&format!("{new}exit 1;; esac"));
        let wrong = root.fake(&format!("{new}printf no;; esac"));
        let slow = root.fake(&format!("{new}exec sleep 30;; esac"));
        let short = Duration::from_millis(300);
        let resolve_in =
            |found: &[PathBuf], timeout| resolve(found, &root.scratch, timeout, Some(&root.path));
        assert_eq!(
            resolve_in(std::slice::from_ref(&exits), secs(2)),
            Availability::Blocked
        );
        assert_eq!(
            resolve_in(&[exits.clone(), wrong], secs(2)),
            Availability::Blocked
        );
        assert_eq!(
            resolve_in(std::slice::from_ref(&slow), short),
            Availability::Retrying
        );
        // One failure that may pass among blocked ones: asking again may find a working one.
        assert_eq!(resolve_in(&[exits, slow], short), Availability::Retrying);
    }
    #[test]
    fn resolver_uses_the_canonical_path_and_tries_each_file_once() {
        let root = Root::new();
        let real = root.passing("bubblewrap 0.13.1", 0o755);
        let link = root.path.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            root.resolve(&[link.clone(), real.clone()]),
            Availability::Available { bwrap, .. } if bwrap == real
        ));
        // A symlink to something untrusted is judged by its target.
        let writable = root.passing("bubblewrap 0.13.1", 0o777);
        let bad_link = root.path.join("bad-link");
        std::os::unix::fs::symlink(writable, &bad_link).unwrap();
        assert_eq!(root.resolve(&[bad_link]), Availability::Unavailable);
    }
    #[test]
    fn candidates_are_absolute_and_bundled_first() {
        let c = candidates(Path::new("/opt/x/usr/bin/blackwall"));
        assert_eq!(c[0], PathBuf::from("/opt/x/usr/lib/blackwall/bwrap"));
        assert!(c.iter().all(|p| p.is_absolute()));
        assert!(candidates(Path::new("blackwall"))
            .iter()
            .all(|p| p.is_absolute()));
    }
    #[test]
    fn probe_accepts_only_exit_zero_with_ok_and_honours_the_timeout() {
        let root = Root::new();
        assert!(probe(&root.fake("printf ok"), &root.scratch, secs(2)).is_ok());
        assert!(probe(&root.fake("printf okay"), &root.scratch, secs(2)).is_err());
        assert!(probe(&root.fake("printf ok; exit 3"), &root.scratch, secs(2)).is_err());
        let started = Instant::now();
        let slow = root.fake("exec sleep 30");
        assert!(matches!(
            probe(&slow, &root.scratch, Duration::from_millis(300)),
            Err(ProbeError::TimedOut)
        ));
        assert!(started.elapsed() < secs(5));
    }
    #[test]
    fn a_forking_program_cannot_outlive_the_timeout() {
        let root = Root::new();
        let started = Instant::now();
        // Gives up waiting while a background process is still running.
        // A second is ample for it to record the pid first, even on a slow CI runner.
        let hung = root.fake("sleep 30 &\necho $! > \"$0.pid\"\nsleep 30");
        assert!(matches!(
            probe(&hung, &root.scratch, secs(1)),
            Err(ProbeError::TimedOut)
        ));
        assert_dies(&hung);
        // Exits at once but leaves a background process holding the output pipe.
        let leaves = root.fake("sleep 30 &\necho $! > \"$0.pid\"\nprintf ok");
        assert!(probe(&leaves, &root.scratch, secs(2)).is_ok());
        assert_dies(&leaves);
        assert!(started.elapsed() < secs(5));
    }
    /// The background process the fake recorded in `<script>.pid` is gone (or a zombie).
    fn assert_dies(script: &Path) {
        let mut pid_file = script.as_os_str().to_owned();
        pid_file.push(".pid");
        let pid = std::fs::read_to_string(PathBuf::from(pid_file)).unwrap();
        let pid = pid.trim();
        assert!(!pid.is_empty());
        for _ in 0..40 {
            let alive = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|stat| !stat.contains(") Z"));
            if !alive {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("background process {pid} outlived the probe");
    }
    #[test]
    fn resolve_is_final_only_when_no_trusted_new_enough_candidate_exists() {
        let root = Root::new();
        let found = |candidates: &[PathBuf], timeout| {
            resolve(candidates, &root.scratch, timeout, Some(&root.path))
        };
        let short = Duration::from_millis(300);
        let no_interpreter = root.fake("");
        write_script(&no_interpreter, "#!/no/such/interpreter\n");
        std::fs::set_permissions(&no_interpreter, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Nothing usable was found: final.
        assert_eq!(found(&[], secs(2)), Availability::Unavailable);
        let untrusted = root.passing("bubblewrap 0.13.1", 0o777);
        assert_eq!(
            found(&[untrusted, root.path.join("missing")], secs(2)),
            Availability::Unavailable
        );
        assert_eq!(
            found(&[root.passing("bubblewrap 0.11.2", 0o755)], secs(2)),
            Availability::Unavailable
        );
        assert_eq!(
            found(&[root.passing("not bubblewrap", 0o755)], secs(2)),
            Availability::Unavailable
        );
        assert_eq!(
            found(&[root.fake("exit 1")], secs(2)),
            Availability::Unavailable,
            "--version fails"
        );
        // A trusted file that would not run, or a new-enough one that would not start a sandbox in
        // time: maybe temporary.
        assert_eq!(
            found(&[root.fake("exec sleep 30")], short),
            Availability::Retrying,
            "--version times out"
        );
        assert_eq!(
            found(&[no_interpreter], secs(2)),
            Availability::Retrying,
            "cannot start"
        );
        let new = "case $1 in --version) echo 'bubblewrap 0.13.1';; *) ";
        assert_eq!(
            found(&[root.fake(&format!("{new}exec sleep 30;; esac"))], short),
            Availability::Retrying,
            "probe times out"
        );
        // A new-enough one that runs and fails to start a sandbox: blocked, asked again too.
        assert_eq!(
            found(&[root.fake(&format!("{new}exit 1;; esac"))], secs(2)),
            Availability::Blocked,
            "probe fails"
        );
        assert_eq!(
            found(&[root.fake(&format!("{new}printf no;; esac"))], secs(2)),
            Availability::Blocked,
            "probe output"
        );
        // A later good candidate still wins.
        let blocked = root.fake(&format!("{new}exit 1;; esac"));
        assert!(matches!(
            found(
                &[blocked, root.passing("bubblewrap 0.13.1", 0o755)],
                secs(2)
            ),
            Availability::Available { .. }
        ));
    }
    #[test]
    fn availability_remembers_answers_but_not_a_failure_to_prepare() {
        let fresh =
            |retry_after| -> &'static Memory { Box::leak(Box::new(Memory::new(retry_after))) };
        let never = || -> Option<(Availability, bool)> { panic!("asked again") };
        let settled = |found: Availability| Some((found, false));
        // No probe folder (the data folder doesn't exist yet on a first launch): Retrying, but
        // the next call looks again at once.
        let memory = fresh(RETRY_AFTER);
        assert_eq!(memory.answer(|| None), &Availability::Retrying);
        assert_eq!(
            memory.answer(|| settled(Availability::Blocked)),
            &Availability::Blocked
        );
        // Blocked and Retrying are repeated until the retry time, then asked again.
        assert_eq!(memory.answer(never), &Availability::Blocked);
        let memory = fresh(RETRY_AFTER);
        assert_eq!(
            memory.answer(|| settled(Availability::Retrying)),
            &Availability::Retrying
        );
        assert_eq!(memory.answer(never), &Availability::Retrying);
        let memory = fresh(Duration::ZERO);
        assert_eq!(
            memory.answer(|| settled(Availability::Blocked)),
            &Availability::Blocked
        );
        assert_eq!(
            memory.answer(|| settled(Availability::Unavailable)),
            &Availability::Unavailable
        );
        // Unavailable and Available are final.
        assert_eq!(memory.answer(never), &Availability::Unavailable);
        let memory = fresh(Duration::ZERO);
        let found = Availability::Available {
            bwrap: "/usr/lib/blackwall/bwrap".into(),
            version: (0, 13, 1),
            workspace_write: true,
        };
        assert_eq!(memory.answer(|| settled(found.clone())), &found);
        assert_eq!(memory.answer(never), &found);
    }
    #[test]
    fn a_slow_overlay_probe_is_asked_again_while_read_only_stays_available() {
        let root = Root::new();
        let new = "case $1 in --version) echo 'bubblewrap 0.13.1';; *) ";
        let overlay = "tr '\\0' '\\n' <&4 | grep -qx -- --overlay";
        let fake = root.fake(&format!(
            "{new}if {overlay}; then exec sleep 30; fi; printf ok;; esac"
        ));
        let look = |asked: &std::cell::Cell<u32>| {
            asked.set(asked.get() + 1);
            Some(resolve_with_retry(
                std::slice::from_ref(&fake),
                &root.scratch,
                secs(2),
                Duration::from_millis(500),
                Some(&root.path),
            ))
        };
        let read_only = Availability::Available {
            bwrap: fake.clone(),
            version: (0, 13, 1),
            workspace_write: false,
        };
        let asked = std::cell::Cell::new(0);
        // Inside the retry time the answer is Available, never Retrying, and nobody probes again.
        let memory: &'static Memory = Box::leak(Box::new(Memory::new(RETRY_AFTER)));
        assert_eq!(memory.answer(|| look(&asked)), &read_only);
        assert_eq!(memory.answer(|| look(&asked)), &read_only);
        assert_eq!(asked.get(), 1);
        // After it, the overlay is probed again, and a slow one is not kept either.
        let asked = std::cell::Cell::new(0);
        let memory: &'static Memory = Box::leak(Box::new(Memory::new(Duration::ZERO)));
        let first = memory.answer(|| look(&asked));
        let second = memory.answer(|| look(&asked));
        assert_eq!((first, second), (&read_only, &read_only));
        assert_eq!(asked.get(), 2);
        assert!(memory.found.get().is_none());
        // The same answer is reused, not leaked again.
        assert!(std::ptr::eq(first, second));
    }
    #[test]
    fn a_definitive_overlay_failure_is_kept_for_the_life_of_the_process() {
        let root = Root::new();
        let new = "case $1 in --version) echo 'bubblewrap 0.13.1';; *) ";
        let overlay = "tr '\\0' '\\n' <&4 | grep -qx -- --overlay";
        for body in [
            format!("{new}if {overlay}; then exit 1; fi; printf ok;; esac"),
            format!("{new}printf ok;; esac"),
        ] {
            let fake = root.fake(&body);
            let memory: &'static Memory = Box::leak(Box::new(Memory::new(Duration::ZERO)));
            let asked = std::cell::Cell::new(0);
            let found = memory.answer(|| {
                asked.set(asked.get() + 1);
                Some(resolve_with_retry(
                    std::slice::from_ref(&fake),
                    &root.scratch,
                    secs(2),
                    Duration::from_millis(500),
                    Some(&root.path),
                ))
            });
            assert_eq!(
                found,
                &Availability::Available {
                    bwrap: fake.clone(),
                    version: (0, 13, 1),
                    workspace_write: false,
                },
                "{body}"
            );
            assert_eq!(memory.answer(|| panic!("asked again")), found);
            assert_eq!(asked.get(), 1);
            assert!(memory.found.get().is_some());
        }
    }
    #[test]
    fn trusted_file_requires_a_plain_unwritable_file_in_unwritable_directories() {
        let root = Root::new();
        let appimage = Some(root.path.as_path());
        let good = root.fake("true");
        assert_eq!(trusted_file(&good, appimage), Some(good.clone()));
        for mode in [0o775, 0o757, 0o777, 0o2755, 0o4755, 0o644] {
            assert_eq!(
                trusted_file(&root.fake_mode("true", mode), appimage),
                None,
                "{mode:o}"
            );
        }
        assert_eq!(trusted_file(&root.path, appimage), None, "a directory");
        assert_eq!(trusted_file(&root.path.join("missing"), appimage), None);
        // Anyone who can write the directory can swap the file.
        let shared = good.parent().unwrap();
        std::fs::set_permissions(shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(trusted_file(&good, appimage), None, "world-writable parent");
        std::fs::set_permissions(shared, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert_eq!(trusted_file(&good, appimage), None, "group-writable parent");
        // Sticky does not rescue a directory that root does not own.
        std::fs::set_permissions(shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        if shared.metadata().unwrap().uid() != 0 {
            assert_eq!(trusted_file(&good, appimage), None, "sticky, not root's");
        }
    }
    #[test]
    fn user_owned_files_are_trusted_only_inside_the_appimage_mount() {
        assert!(owner_trusted(0, 1000, false));
        assert!(owner_trusted(0, 1000, true));
        assert!(owner_trusted(1000, 1000, true));
        assert!(!owner_trusted(1000, 1000, false));
        assert!(!owner_trusted(1001, 1000, true));
        // Run as root, hand the file to another user; otherwise it is already ours.
        let root = Root::new();
        let file = root.fake("true");
        if file.metadata().unwrap().uid() == 0 {
            std::os::unix::fs::chown(&file, Some(12345), None).unwrap();
        }
        let owner = file.metadata().unwrap().uid();
        assert_eq!(
            trusted_file_as(&file, None, owner),
            None,
            "outside the mount"
        );
        assert_eq!(
            trusted_file_as(&file, Some(&root.path), owner),
            Some(file.clone())
        );
        let elsewhere = Root::new();
        assert_eq!(trusted_file_as(&file, Some(&elsewhere.path), owner), None);
    }
    #[test]
    fn the_user_owned_exception_stops_at_the_appimage_mount() {
        // The image is mounted inside a directory the user owns (their home, say). The mount may
        // be theirs; what it sits in may not.
        let root = Root::new();
        if root.path.metadata().unwrap().uid() == 0 {
            std::os::unix::fs::chown(&root.path, Some(12345), None).unwrap();
        }
        let owner = root.path.metadata().unwrap().uid();
        let mount = root.path.join("mnt");
        std::fs::create_dir(&mount).unwrap();
        std::fs::set_permissions(&mount, std::fs::Permissions::from_mode(0o755)).unwrap();
        let file = mount.join("bwrap");
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::chown(&file, Some(owner), None).unwrap();
        std::os::unix::fs::chown(&mount, Some(owner), None).unwrap();
        assert_eq!(
            trusted_file_as(&file, Some(&mount), owner),
            None,
            "the user-owned directory above the mount"
        );
        // Were the whole of it the mount, the same file would pass.
        assert_eq!(trusted_file_as(&file, Some(&root.path), owner), Some(file));
    }
    #[test]
    fn the_appimage_mount_needs_both_variables_and_the_running_program_inside_it() {
        let root = Root::new();
        let exe = root.fake("true");
        let dir = || Some(root.path.clone().into_os_string());
        let image = || Some(OsString::from("/home/u/Blackwall.AppImage"));
        assert_eq!(appimage_dir(image(), dir(), &exe), Some(root.path.clone()));
        assert_eq!(appimage_dir(None, dir(), &exe), None);
        assert_eq!(appimage_dir(image(), None, &exe), None);
        assert_eq!(appimage_dir(Some(OsString::new()), dir(), &exe), None);
        let relative = Some(OsString::from("relative"));
        assert_eq!(appimage_dir(image(), relative, &exe), None);
        let missing = Some(OsString::from("/no/such/dir"));
        assert_eq!(appimage_dir(image(), missing, &exe), None);
        // Not running from the image, or "/" as the image.
        let other = Root::new();
        let elsewhere = other.fake("true");
        assert_eq!(appimage_dir(image(), dir(), &elsewhere), None);
        assert_eq!(
            appimage_dir(image(), dir(), Path::new("/no/such/exe")),
            None
        );
        assert_eq!(appimage_dir(image(), Some(OsString::from("/")), &exe), None);
    }
    #[test]
    fn the_project_is_pinned_at_its_canonical_path_before_the_walk() {
        let root = Root::new();
        let project = root.path.join("p");
        std::fs::create_dir(&project).unwrap();
        let meta = std::fs::metadata(&project).unwrap();
        let (fd, pinned) = pin_project(&project).unwrap();
        assert_eq!(pinned, (meta.dev(), meta.ino()));
        assert!(fd.as_raw_fd() >= FIRST_HANDED_FD);
        let flags = rustix::io::fcntl_getfd(&fd).unwrap();
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
        assert_eq!(
            std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap(),
            project
        );
        // A link to it, or a name for it that isn't canonical, is not the project's path.
        let link = root.path.join("link");
        std::os::unix::fs::symlink(&project, &link).unwrap();
        assert!(pin_project(&link).is_none());
        assert!(pin_project(&project.join("../p")).is_none());
        // Another folder put in its place after pinning: the walk then starts somewhere else,
        // and the pinned folder can't be freed and its inode reused while the fd is open.
        std::fs::rename(&project, root.path.join("moved")).unwrap();
        std::fs::create_dir(&project).unwrap();
        let walked = super::super::walk::walk(&project, 100).unwrap();
        assert_ne!(walked.root, pinned);
        std::fs::remove_dir(&project).unwrap();
        std::os::unix::fs::symlink(root.path.join("moved"), &project).unwrap();
        assert!(pin_project(&project).is_none());
    }
    #[test]
    fn only_spawn_with_hands_the_descriptor_on() {
        let root = Root::new();
        let shell = |script: String| {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", &script]).stdout(Stdio::piped());
            command
        };
        let fd = open_directory(&root.path).unwrap();
        let number = fd.as_raw_fd();
        let mut command = shell(format!("readlink /proc/$$/fd/{number}"));
        let child = spawn_with(fd, || command.spawn()).unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim_end(),
            root.path.to_str().unwrap()
        );
        // Opened, it is close-on-exec: a plain spawn doesn't get it.
        let fd = open_directory(&root.path).unwrap();
        let status = shell(format!("test ! -e /proc/$$/fd/{}", fd.as_raw_fd()))
            .status()
            .unwrap();
        assert!(status.success());
    }
    #[test]
    fn scratch_is_private_and_refuses_a_symlinked_data_directory() {
        let root = Root::new();
        let data = root.path.join("data");
        std::fs::create_dir(&data).unwrap();
        let scratch = prepare_scratch(&data).unwrap();
        assert!(scratch.starts_with(data.join("staging")));
        assert_eq!(scratch.metadata().unwrap().mode() & 0o777, 0o700);
        let stale = data.join("staging/probe-stale");
        std::fs::create_dir(&stale).unwrap();
        std::fs::File::open(&stale)
            .unwrap()
            .set_modified(SystemTime::now() - 2 * crate::sandbox::staging::KEEP_FOR)
            .unwrap();
        prepare_scratch(&data).unwrap();
        assert!(!stale.exists());
        assert!(scratch.exists());
        let link = root.path.join("link");
        std::os::unix::fs::symlink(&data, &link).unwrap();
        assert!(prepare_scratch(&link).is_err());
        let other = root.path.join("other");
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&data, other.join("staging")).unwrap();
        assert!(prepare_scratch(&other).is_err());
    }

    #[test]
    #[ignore = "needs bubblewrap; run by the linux-sandbox CI job"]
    fn linux_sandbox_probe_passes_with_the_installed_bwrap() {
        let root = Root::new();
        let found = candidates(Path::new("/usr/bin/blackwall"));
        let Availability::Available {
            bwrap,
            version,
            workspace_write,
        } = resolve_with_retry(&found, &root.scratch, secs(5), OVERLAY_PROBE_TIMEOUT, None).0
        else {
            panic!("no usable bwrap among {found:?}");
        };
        assert!(version >= MIN_VERSION, "{bwrap:?} {version:?}");
        probe(&bwrap, &root.scratch, secs(5)).unwrap();
        // The overlay works under this bubblewrap and kernel, so Workspace-write is offered.
        assert!(workspace_write, "the overlay probe failed with {bwrap:?}");
        probe_overlay(&bwrap, &root.scratch, OVERLAY_PROBE_TIMEOUT).unwrap();
        assert_eq!(std::fs::read_dir(&root.scratch).unwrap().count(), 0);
    }

    #[test]
    #[ignore = "needs bubblewrap; run by the linux-sandbox CI job"]
    fn linux_sandbox_overlay_probe_reads_its_results_from_upper() {
        let root = Root::new();
        let bwrap = PathBuf::from(
            std::env::var_os("BLACKWALL_TEST_BWRAP")
                .expect("set BLACKWALL_TEST_BWRAP to a bubblewrap binary"),
        );
        let dir = root.scratch.join("overlay");
        std::fs::create_dir(&dir).unwrap();
        probe_overlay_in(&bwrap, &dir, OVERLAY_PROBE_TIMEOUT).unwrap();
        use std::os::unix::fs::FileTypeExt;
        let upper = dir.join("upper");
        assert_eq!(std::fs::read(upper.join("new")).unwrap(), b"new");
        assert_eq!(std::fs::read(upper.join("changed")).unwrap(), b"changed");
        let whiteout = std::fs::symlink_metadata(upper.join("deleted")).unwrap();
        assert!(whiteout.file_type().is_char_device() && whiteout.rdev() == 0);
        assert_eq!(std::fs::read(dir.join("project/changed")).unwrap(), b"old");
        assert!(dir.join("project/deleted").exists());
        // The probe's own run removes this, mode-000 work folder and all.
        super::super::staging::remove_dir(&dir).unwrap();
        assert!(!dir.exists());
    }
}
