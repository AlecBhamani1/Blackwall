//! Linux sandbox backend: find a trusted bubblewrap and prove it works on this kernel.
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    Available {
        bwrap: PathBuf,
        version: (u32, u32, u32),
    },
    /// No trusted, new-enough bubblewrap that works here. Asking again won't change it.
    Unavailable,
    /// A trusted bubblewrap failed to start just now. It's asked again after about 30 seconds.
    Retrying,
}
impl Availability {
    /// What to tell the owner when sandboxed commands can't run, or `None` when they can.
    pub fn note(&self) -> Option<&'static str> {
        match self {
            Self::Available { .. } => None,
            Self::Unavailable => Some(LINUX_UNAVAILABLE),
            Self::Retrying => Some(LINUX_RETRYING),
        }
    }
}
pub const LINUX_UNAVAILABLE: &str = "Sandboxed commands on Linux need Bubblewrap (bwrap). Install it with your system's package manager, then restart Blackwall.";
pub const LINUX_RETRYING: &str = "Bubblewrap didn't start just now. Try again in a moment.";
pub(crate) const MIN_VERSION: (u32, u32, u32) = (0, 12, 0);
/// After a timeout or a setup failure, ask again no sooner than this.
const RETRY_AFTER: Duration = Duration::from_secs(30);
const STALE_PROBE_DIRECTORY: Duration = Duration::from_secs(24 * 60 * 60);

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

/// What `resolve` found. `transient` means a trusted bubblewrap failed to run, or a new-enough one
/// failed to start a sandbox, so the answer may change if asked again. Without one it is final.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Resolution {
    pub availability: Availability,
    pub transient: bool,
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
    run_command(command, timeout)
}
/// [`run`] for a command that already has its program, arguments, directory and environment.
fn run_command(
    mut command: Command,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, Vec<u8>), ProbeError> {
    let mut child = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(ProbeError::Spawn)?;
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
/// `scratch` is an existing private directory. Blocking: async callers use `spawn_blocking`.
pub(crate) fn resolve(
    candidates: &[PathBuf],
    scratch: &Path,
    timeout: Duration,
    appimage: Option<&Path>,
) -> Resolution {
    let mut transient = false;
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
                return Resolution {
                    availability: Availability::Available { bwrap, version },
                    transient: false,
                }
            }
            // A trusted, new-enough bubblewrap that cannot start a sandbox may be blocked by
            // policy for good, or short of a resource for a moment. Ask again later to find out.
            Err(_) => transient = true,
        }
    }
    Resolution {
        availability: Availability::Unavailable,
        transient,
    }
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
    let _ = std::fs::remove_dir_all(&project);
    result
}
fn probe_in(bwrap: &Path, project: &Path, timeout: Duration) -> Result<(), ProbeError> {
    let plan = super::bwrap::MountPlan {
        project,
        network: false,
        roots: super::bwrap::host_roots(),
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
        OsStr::new("/bin/sh"),
        &["-c".into(), "printf ok".into()],
    )
    .ok_or(ProbeError::UnexpectedOutput)?;
    let (status, stdout) = run_command(launcher.into_std(), timeout)?;
    if !status.success() {
        return Err(ProbeError::Failed(status));
    }
    if stdout != b"ok" {
        return Err(ProbeError::UnexpectedOutput);
    }
    Ok(())
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
/// staging directory is refused. Probe directories left over from crashes are removed after a day.
fn prepare_scratch(data_directory: &Path) -> std::io::Result<PathBuf> {
    private_scratch(data_directory, "probe-")
}
/// A fresh private (0700) `<data>/staging/run-<random>` directory for one sandboxed command's
/// files, with the same checks and clean-up as the probe's.
pub(crate) fn run_directory(data_directory: &Path) -> std::io::Result<PathBuf> {
    private_scratch(data_directory, "run-")
}
fn private_scratch(data_directory: &Path, prefix: &str) -> std::io::Result<PathBuf> {
    real_directory(data_directory)?;
    let staging = data_directory.join("staging");
    private_directory(&staging)?;
    for entry in std::fs::read_dir(&staging)?.flatten() {
        let stale = entry.file_name().to_string_lossy().starts_with(prefix)
            && entry
                .metadata()
                .ok()
                .filter(|meta| meta.is_dir())
                .and_then(|meta| meta.modified().ok())
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age > STALE_PROBE_DIRECTORY);
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    let scratch = staging.join(format!("{prefix}{:016x}", rand::random::<u64>()));
    std::fs::DirBuilder::new().mode(0o700).create(&scratch)?;
    Ok(scratch)
}

static UNAVAILABLE: Availability = Availability::Unavailable;
static RETRYING: Availability = Availability::Retrying;
static AVAILABLE: OnceLock<Availability> = OnceLock::new();
static NEVER_AVAILABLE: AtomicBool = AtomicBool::new(false);
static RETRY_NOT_BEFORE: Mutex<Option<Instant>> = Mutex::new(None);

/// Whether sandboxed commands can run on this machine. Finding a working bubblewrap and finding
/// none are both remembered for the life of the process (the first data directory wins). A
/// timeout or a scratch-directory failure is not: the answer is `Retrying`, and it is asked again
/// after about 30 seconds. A processor the seccomp filter doesn't support is never available.
/// Blocking: async callers use `spawn_blocking`.
pub fn availability(data_directory: &Path) -> &'static Availability {
    if super::seccomp::NATIVE_ARCH.is_none() {
        return &UNAVAILABLE;
    }
    if let Some(found) = AVAILABLE.get() {
        return found;
    }
    if NEVER_AVAILABLE.load(Ordering::Acquire) {
        return &UNAVAILABLE;
    }
    // Held while probing, so concurrent first callers wait for one answer instead of probing twice.
    let mut retry_not_before = RETRY_NOT_BEFORE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(found) = AVAILABLE.get() {
        return found;
    }
    if NEVER_AVAILABLE.load(Ordering::Acquire) {
        return &UNAVAILABLE;
    }
    if retry_not_before.is_some_and(|instant| Instant::now() < instant) {
        return &RETRYING;
    }
    let resolution = match prepare_scratch(data_directory) {
        Ok(scratch) => {
            let exe = std::env::current_exe().unwrap_or_default();
            let appimage = appimage_dir(
                std::env::var_os("APPIMAGE"),
                std::env::var_os("APPDIR"),
                &exe,
            );
            let resolution = resolve(
                &candidates(&exe),
                &scratch,
                Duration::from_secs(2),
                appimage.as_deref(),
            );
            let _ = std::fs::remove_dir_all(&scratch);
            resolution
        }
        Err(_) => Resolution {
            availability: Availability::Unavailable,
            transient: true,
        },
    };
    match resolution {
        Resolution {
            availability: found @ Availability::Available { .. },
            ..
        } => AVAILABLE.get_or_init(|| found),
        Resolution {
            transient: true, ..
        } => {
            *retry_not_before = Some(Instant::now() + RETRY_AFTER);
            &RETRYING
        }
        _ => {
            NEVER_AVAILABLE.store(true, Ordering::Release);
            &UNAVAILABLE
        }
    }
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
        fn resolve(&self, candidates: &[PathBuf]) -> Resolution {
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
                    &root.resolve(std::slice::from_ref(&candidate)).availability,
                    Availability::Unavailable
                ),
                "{why} alone"
            );
            assert!(
                matches!(
                    root.resolve(&[candidate, control.clone()]).availability,
                    Availability::Available { bwrap, version: (0, 13, 1) } if bwrap == control
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
        let resolution = root.resolve(&[blocked.clone(), ok.clone()]);
        assert!(!resolution.transient);
        assert!(matches!(
            resolution.availability,
            Availability::Available { bwrap, .. } if bwrap == ok
        ));
        // A usable-looking bubblewrap that cannot start a sandbox may recover (resources, policy).
        assert_eq!(
            root.resolve(&[blocked]),
            Resolution {
                availability: Availability::Unavailable,
                transient: true
            }
        );
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
        assert_eq!(
            resolution,
            Resolution {
                availability: Availability::Unavailable,
                transient: true
            }
        );
    }
    #[test]
    fn resolver_uses_the_canonical_path_and_tries_each_file_once() {
        let root = Root::new();
        let real = root.passing("bubblewrap 0.13.1", 0o755);
        let link = root.path.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let resolution = root.resolve(&[link.clone(), real.clone()]);
        assert!(matches!(
            resolution.availability,
            Availability::Available { bwrap, .. } if bwrap == real
        ));
        // A symlink to something untrusted is judged by its target.
        let writable = root.passing("bubblewrap 0.13.1", 0o777);
        let bad_link = root.path.join("bad-link");
        std::os::unix::fs::symlink(writable, &bad_link).unwrap();
        assert_eq!(
            root.resolve(&[bad_link]).availability,
            Availability::Unavailable
        );
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
        let hung = root.fake("sleep 30 &\necho $! > \"$0.pid\"\nsleep 30");
        assert!(matches!(
            probe(&hung, &root.scratch, Duration::from_millis(300)),
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
        let transient = |candidates: &[PathBuf], timeout| {
            resolve(candidates, &root.scratch, timeout, Some(&root.path)).transient
        };
        let short = Duration::from_millis(300);
        let no_interpreter = root.fake("");
        write_script(&no_interpreter, "#!/no/such/interpreter\n");
        std::fs::set_permissions(&no_interpreter, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Nothing usable was found: final.
        assert!(!transient(&[], secs(2)));
        let untrusted = root.passing("bubblewrap 0.13.1", 0o777);
        assert!(!transient(&[untrusted, root.path.join("missing")], secs(2)));
        assert!(!transient(
            &[root.passing("bubblewrap 0.11.2", 0o755)],
            secs(2)
        ));
        assert!(!transient(
            &[root.passing("not bubblewrap", 0o755)],
            secs(2)
        ));
        assert!(
            !transient(&[root.fake("exit 1")], secs(2)),
            "--version fails"
        );
        // A trusted file that would not run, or a new-enough one that would not start a sandbox:
        // maybe temporary.
        assert!(
            transient(&[root.fake("exec sleep 30")], short),
            "--version times out"
        );
        assert!(transient(&[no_interpreter], secs(2)), "cannot start");
        let new = "case $1 in --version) echo 'bubblewrap 0.13.1';; *) ";
        assert!(
            transient(&[root.fake(&format!("{new}exit 1;; esac"))], secs(2)),
            "probe fails"
        );
        assert!(
            transient(&[root.fake(&format!("{new}printf no;; esac"))], secs(2)),
            "probe output"
        );
        assert!(
            transient(&[root.fake(&format!("{new}exec sleep 30;; esac"))], short),
            "probe times out"
        );
        // A later good candidate still wins, and is not transient.
        let blocked = root.fake(&format!("{new}exit 1;; esac"));
        assert!(!transient(
            &[blocked, root.passing("bubblewrap 0.13.1", 0o755)],
            secs(2)
        ));
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
            .set_modified(SystemTime::now() - 2 * STALE_PROBE_DIRECTORY)
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
        let Availability::Available { bwrap, version } =
            resolve(&found, &root.scratch, secs(5), None).availability
        else {
            panic!("no usable bwrap among {found:?}");
        };
        assert!(version >= MIN_VERSION, "{bwrap:?} {version:?}");
        probe(&bwrap, &root.scratch, secs(5)).unwrap();
    }
}
