//! Linux sandbox backend: find a trusted bubblewrap and prove it works on this kernel.
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
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
    Unavailable,
}
pub const LINUX_UNAVAILABLE: &str = "Sandboxed commands on Linux need Bubblewrap (bwrap). Install it with your system's package manager, then restart Blackwall.";
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
}

/// What `resolve` found. `transient` means a timeout, so the answer may change if asked again.
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
/// current user, because the image is a read-only mount that user started.
pub(crate) fn appimage_dir(
    appimage: Option<OsString>,
    appdir: Option<OsString>,
) -> Option<PathBuf> {
    let appdir = PathBuf::from(appdir.filter(|value| !value.is_empty())?);
    appimage.filter(|value| !value.is_empty())?;
    appdir.is_absolute().then_some(())?;
    std::fs::canonicalize(appdir).ok()
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
    let in_appimage = appimage.is_some_and(|mount| canonical.starts_with(mount));
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
            .all(|directory| directory_trusted(directory, euid, in_appimage)))
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
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env_clear()
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
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                kill_and_reap(&mut child);
                return Err(ProbeError::TimedOut);
            }
            Err(error) => {
                kill_and_reap(&mut child);
                return Err(ProbeError::Wait(error));
            }
        }
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    match receiver.recv_timeout(remaining.max(Duration::from_millis(50))) {
        Ok(output) => Ok((status, output)),
        Err(_) => {
            // A leftover process still holds the pipe; end it so the reader thread can finish.
            kill_group(&child);
            Err(ProbeError::TimedOut)
        }
    }
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
        let output = run(&bwrap, &["--version"], scratch, timeout);
        transient |= matches!(output, Err(ProbeError::TimedOut));
        let Ok((status, stdout)) = output else {
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
            Err(error) => transient |= matches!(error, ProbeError::TimedOut),
        }
    }
    Resolution {
        availability: Availability::Unavailable,
        transient,
    }
}

/// Start the smallest possible sandbox and print `ok` from inside it. This catches kernels and
/// security policies (disabled user namespaces, AppArmor) that allow `--version` but not a sandbox.
pub(crate) fn probe(bwrap: &Path, scratch: &Path, timeout: Duration) -> Result<(), ProbeError> {
    let mut args = vec![
        "--unshare-all",
        "--die-with-parent",
        "--new-session",
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/bin",
        "/bin",
        // The dynamic loader lives under /lib (arm64) or /lib64 (x86-64) on merged-/usr systems.
        "--symlink",
        "usr/lib",
        "/lib",
    ];
    if Path::new("/usr/lib64").is_dir() {
        args.extend(["--symlink", "usr/lib64", "/lib64"]);
    }
    args.extend(["--", "/bin/sh", "-c", "printf ok"]);
    let (status, stdout) = run(bwrap, &args, scratch, timeout)?;
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

/// A fresh private `<data>/staging/probe-<random>` directory. A symlinked data directory or
/// staging directory is refused. Probe directories left over from crashes are removed after a day.
fn prepare_scratch(data_directory: &Path) -> std::io::Result<PathBuf> {
    real_directory(data_directory)?;
    let staging = data_directory.join("staging");
    private_directory(&staging)?;
    for entry in std::fs::read_dir(&staging)?.flatten() {
        let stale = entry.file_name().to_string_lossy().starts_with("probe-")
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
    let scratch = staging.join(format!("probe-{:016x}", rand::random::<u64>()));
    std::fs::DirBuilder::new().mode(0o700).create(&scratch)?;
    Ok(scratch)
}

static UNAVAILABLE: Availability = Availability::Unavailable;
static AVAILABLE: OnceLock<Availability> = OnceLock::new();
static NEVER_AVAILABLE: AtomicBool = AtomicBool::new(false);
static RETRY_NOT_BEFORE: Mutex<Option<Instant>> = Mutex::new(None);

/// Whether sandboxed commands can run on this machine. Finding a working bubblewrap and finding
/// none are both remembered for the life of the process (the first data directory wins). A
/// timeout or a scratch-directory failure is not: it is retried after about 30 seconds.
pub fn availability(data_directory: &Path) -> &'static Availability {
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
    if NEVER_AVAILABLE.load(Ordering::Acquire)
        || retry_not_before.is_some_and(|instant| Instant::now() < instant)
    {
        return &UNAVAILABLE;
    }
    let resolution = match prepare_scratch(data_directory) {
        Ok(scratch) => {
            let exe = std::env::current_exe().unwrap_or_default();
            let appimage = appimage_dir(std::env::var_os("APPIMAGE"), std::env::var_os("APPDIR"));
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
            &UNAVAILABLE
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
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
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
        // A probe that fails outright is a firm no, not something to retry.
        assert_eq!(
            root.resolve(&[blocked]),
            Resolution {
                availability: Availability::Unavailable,
                transient: false
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
        let forking = root.fake("sleep 30 &\nsleep 30");
        assert!(probe(&forking, &root.scratch, Duration::from_millis(300)).is_err());
        let leaks = root.fake("sleep 30 &\nprintf ok");
        assert!(probe(&leaks, &root.scratch, Duration::from_millis(300)).is_err());
        assert!(started.elapsed() < secs(5));
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
    fn the_appimage_mount_needs_both_environment_variables() {
        let root = Root::new();
        let dir = || Some(root.path.clone().into_os_string());
        let image = || Some(OsString::from("/home/u/Blackwall.AppImage"));
        assert_eq!(appimage_dir(image(), dir()), Some(root.path.clone()));
        assert_eq!(appimage_dir(None, dir()), None);
        assert_eq!(appimage_dir(image(), None), None);
        assert_eq!(appimage_dir(Some(OsString::new()), dir()), None);
        assert_eq!(
            appimage_dir(image(), Some(OsString::from("relative"))),
            None
        );
        assert_eq!(
            appimage_dir(image(), Some(OsString::from("/no/such/dir"))),
            None
        );
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
