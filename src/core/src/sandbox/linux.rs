//! Linux sandbox backend: find a trusted bubblewrap and prove it works on this kernel.
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant};

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

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProbeError {
    #[error("could not prepare the probe directory: {0}")]
    Scratch(std::io::Error),
    #[error("could not start bubblewrap: {0}")]
    Spawn(std::io::Error),
    #[error("bubblewrap did not finish in time")]
    TimedOut,
    #[error("bubblewrap exited with {0}")]
    Failed(std::process::ExitStatus),
    #[error("bubblewrap ran but the command inside did not print the expected output")]
    UnexpectedOutput,
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

/// The effective user of this process. `/proc/self` belongs to it; when that cannot be read, no
/// user-owned file is trusted (only root-owned ones), which fails closed.
fn current_uid() -> Option<u32> {
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}

/// A regular file that nobody but root or this user can change, and that is not setuid or setgid.
pub(crate) fn trusted_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let mode = meta.mode();
    meta.is_file()
        && mode & 0o6000 == 0
        && mode & 0o022 == 0
        && mode & 0o111 != 0
        && (meta.uid() == 0 || Some(meta.uid()) == current_uid())
}

/// Run `program` with a cleared environment and no stdin, and return its exit status and
/// (at most 4 KiB of) stdout. Blocks for at most `timeout`, then kills it.
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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(ProbeError::Spawn)?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ProbeError::UnexpectedOutput);
    };
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stdout.take(4096).read_to_end(&mut output);
        let _ = sender.send(output);
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProbeError::TimedOut);
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProbeError::Spawn(error));
            }
        }
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let output = receiver
        .recv_timeout(remaining.max(Duration::from_millis(50)))
        .map_err(|_| ProbeError::TimedOut)?;
    Ok((status, output))
}

/// The first candidate that is trusted, new enough and able to start a sandbox on this kernel.
/// Blocking: async callers use `spawn_blocking`.
pub(crate) fn resolve(candidates: &[PathBuf], scratch: &Path, timeout: Duration) -> Availability {
    for bwrap in candidates {
        if !trusted_file(bwrap) || std::fs::create_dir_all(scratch).is_err() {
            continue;
        }
        let Ok((status, stdout)) = run(bwrap, &["--version"], scratch, timeout) else {
            continue;
        };
        let version = status
            .success()
            .then(|| parse_version(&String::from_utf8_lossy(&stdout)))
            .flatten();
        let Some(version) = version.filter(|version| *version >= MIN_VERSION) else {
            continue;
        };
        if probe(bwrap, scratch, timeout).is_ok() {
            return Availability::Available {
                bwrap: bwrap.clone(),
                version,
            };
        }
    }
    Availability::Unavailable
}

/// Start the smallest possible sandbox and print `ok` from inside it. This catches kernels and
/// security policies (disabled user namespaces, AppArmor) that allow `--version` but not a sandbox.
pub(crate) fn probe(bwrap: &Path, scratch: &Path, timeout: Duration) -> Result<(), ProbeError> {
    std::fs::create_dir_all(scratch).map_err(ProbeError::Scratch)?;
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

static AVAILABILITY: OnceLock<Availability> = OnceLock::new();

/// Whether sandboxed commands can run on this machine, decided once per process. The first call
/// wins; later calls with another data directory get the same answer.
pub fn availability(data_directory: &Path) -> &'static Availability {
    AVAILABILITY.get_or_init(|| {
        let exe = std::env::current_exe().unwrap_or_default();
        let scratch = data_directory
            .join("staging")
            .join(format!("probe-{:016x}", rand::random::<u64>()));
        let found = resolve(&candidates(&exe), &scratch, Duration::from_secs(2));
        let _ = std::fs::remove_dir_all(&scratch);
        found
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
    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blackwall-bwrap-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn fake_mode(body: &str, mode: u32) -> PathBuf {
        let path = temp_dir().join("bwrap");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }
    fn fake(body: &str) -> PathBuf {
        fake_mode(body, 0o755)
    }
    fn fake_passing_probe() -> PathBuf {
        fake("case $1 in --version) echo 'bubblewrap 0.13.1';; *) printf ok;; esac")
    }

    #[test]
    fn parse_version_accepts_only_bubblewrap_semver() {
        assert_eq!(parse_version("bubblewrap 0.13.1\n"), Some((0, 13, 1)));
        assert_eq!(parse_version("bubblewrap 1.0"), Some((1, 0, 0)));
        for bad in ["bwrap 0.13.1", "bubblewrap", "bubblewrap x.y", ""] {
            assert_eq!(parse_version(bad), None);
        }
    }
    #[test]
    fn resolver_rejects_old_setuid_writable_and_unparseable_bwrap() {
        let scratch = temp_dir();
        let old = fake("echo 'bubblewrap 0.11.2'");
        let junk = fake("echo hi");
        let writable = fake_mode("echo 'bubblewrap 0.13.1'", 0o777);
        let setuid = fake_mode("echo 'bubblewrap 0.13.1'", 0o4755);
        assert_eq!(
            resolve(&[old, junk, writable, setuid], &scratch, secs(2)),
            Availability::Unavailable
        );
    }
    #[test]
    fn resolver_falls_through_a_failing_probe_to_the_next_candidate() {
        let scratch = temp_dir();
        let blocked = fake("case $1 in --version) echo 'bubblewrap 0.13.1';; *) exit 1;; esac");
        let ok = fake_passing_probe();
        assert!(matches!(
            resolve(&[blocked, ok.clone()], &scratch, secs(2)),
            Availability::Available { bwrap, version: (0, 13, 1) } if bwrap == ok
        ));
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
        let scratch = temp_dir();
        assert!(probe(&fake("printf ok"), &scratch, secs(2)).is_ok());
        assert!(probe(&fake("printf okay"), &scratch, secs(2)).is_err());
        assert!(probe(&fake("printf ok; exit 3"), &scratch, secs(2)).is_err());
        let started = std::time::Instant::now();
        assert!(probe(&fake("exec sleep 30"), &scratch, Duration::from_millis(300)).is_err());
        assert!(started.elapsed() < secs(5));
    }
    #[test]
    fn trusted_file_requires_a_plain_private_file_owned_by_root_or_us() {
        assert!(trusted_file(&fake("true")));
        assert!(!trusted_file(&fake_mode("true", 0o775)));
        assert!(!trusted_file(&fake_mode("true", 0o2755)));
        assert!(!trusted_file(&temp_dir()));
        assert!(!trusted_file(&temp_dir().join("missing")));
    }
    #[test]
    #[ignore = "needs bubblewrap; run by the linux-sandbox CI job"]
    fn linux_sandbox_probe_passes_with_the_installed_bwrap() {
        let scratch = temp_dir();
        let found = candidates(Path::new("/usr/bin/blackwall"));
        let Availability::Available { bwrap, version } = resolve(&found, &scratch, secs(5)) else {
            panic!("no usable bwrap among {found:?}");
        };
        assert!(version >= MIN_VERSION, "{bwrap:?} {version:?}");
        probe(&bwrap, &scratch, secs(5)).unwrap();
    }
}
