//! The folders one Workspace-write command stages its changes in, and the sweep that removes the
//! ones a crash left behind.

use super::walk::Fingerprint;
use crate::context::MAX_COMMAND_TIMEOUT_SECONDS;
use crate::tools::ToolError;
use std::collections::HashMap;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How long a staging folder is kept before the next sweep removes it.
///
/// The sweep goes by a folder's age alone and takes no lock, so this must far outlast any
/// command that uses one. Every sandboxed command runs under `tools::run_with_input` with the
/// command time limit, which defaults to 120 s (the shell tool's cap) and cannot be set above
/// [`MAX_COMMAND_TIMEOUT_SECONDS`] (one hour), so a day leaves a wide margin.
pub(crate) const KEEP_FOR: Duration = Duration::from_secs(24 * 60 * 60);

// The sweep must never reach a folder a running command still uses: a day is at least 8 times
// the longest time limit a command can have.
const _: () = assert!(KEEP_FOR.as_secs() >= 8 * MAX_COMMAND_TIMEOUT_SECONDS);

/// One command's staging folder: `<data>/staging/<run-id>`, with the overlay's upper and work
/// folders in it, and what the project looked like when the command started.
#[cfg_attr(not(test), allow(dead_code))] // used by the overlay mount and the apply step
pub(crate) struct Staging {
    pub root: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
    pub start: HashMap<PathBuf, Fingerprint>,
}

/// Make a private staging folder under `<data>/staging` with its `upper` and `work` folders (all
/// 0700), after the checks and sweep every run folder gets (a symlinked data or staging folder is
/// refused). `start` is what the project looked like before the command ran.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn create(
    data: &Path,
    start: HashMap<PathBuf, Fingerprint>,
) -> Result<Staging, ToolError> {
    let failed = |_| ToolError::Execution(super::PREPARE_FAILED.into());
    let root = super::linux::private_scratch(data, "").map_err(failed)?;
    let (upper, work) = (root.join("upper"), root.join("work"));
    // A half-made folder goes away with the sweep; nothing else points at it yet.
    for dir in [&upper, &work] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .map_err(failed)?;
    }
    Ok(Staging {
        root,
        upper,
        work,
        start,
    })
}

/// Remove the staging folders under `<data>/staging` that were last changed more than
/// [`KEEP_FOR`] before `now` (the leftovers of a crash), and say how many went. Nothing is
/// removed unless `<data>` and `<data>/staging` are both real folders, not symlinks. Symlinks
/// and plain files inside are left alone. Every run folder is made through [`create`] or the
/// run and probe helpers, which sweep first, so this is the only place that decides what is old.
pub(crate) fn sweep(data: &Path, now: SystemTime) -> usize {
    let staging = data.join("staging");
    let real_dir =
        |path: &Path| std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_dir());
    if !real_dir(data) || !real_dir(&staging) {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(&staging) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let is_old = entry
            .metadata()
            .ok()
            .filter(|meta| meta.is_dir())
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > KEEP_FOR);
        if is_old && std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn data() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("blackwall-staging-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&path).unwrap();
        std::fs::canonicalize(path).unwrap()
    }
    fn age(path: &Path, by: Duration) {
        std::fs::File::open(path)
            .unwrap()
            .set_modified(SystemTime::now() - by)
            .unwrap();
    }

    #[test]
    fn staging_dirs_are_private_and_sweep_only_removes_old_ones() {
        let data = data();
        let fingerprint = Fingerprint {
            dev: 1,
            ino: 2,
            size: 3,
            mtime_ns: 4,
            ctime_ns: 5,
        };
        let start = HashMap::from([(PathBuf::from("a.txt"), fingerprint)]);
        let staging = create(&data, start).unwrap();
        assert_eq!(staging.root.parent().unwrap(), data.join("staging"));
        let id = staging.root.file_name().unwrap().to_str().unwrap();
        assert_eq!(id.len(), 16);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(staging.upper, staging.root.join("upper"));
        assert_eq!(staging.work, staging.root.join("work"));
        assert_eq!(staging.start[Path::new("a.txt")], fingerprint);
        for dir in [
            &data.join("staging"),
            &staging.root,
            &staging.upper,
            &staging.work,
        ] {
            assert_eq!(dir.metadata().unwrap().mode() & 0o777, 0o700, "{dir:?}");
        }
        assert_ne!(create(&data, HashMap::new()).unwrap().root, staging.root);

        let old = data.join("staging/old");
        let recent = data.join("staging/recent");
        for dir in [&old, &recent] {
            std::fs::create_dir_all(dir.join("upper/deep")).unwrap();
            std::fs::write(dir.join("upper/deep/f"), "x").unwrap();
        }
        age(&old, Duration::from_secs(25 * 60 * 60));
        age(&recent, Duration::from_secs(60 * 60));
        age(&staging.root, Duration::from_secs(60 * 60));
        assert_eq!(sweep(&data, SystemTime::now()), 1);
        assert!(!old.exists());
        assert!(recent.exists());
        assert!(staging.root.exists());
        // A symlink that looks old is left alone, and so is what it points at.
        let outside = data.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, data.join("staging/link")).unwrap();
        assert_eq!(
            sweep(&data, SystemTime::now() + 2 * KEEP_FOR),
            3,
            "the three folders left; the symlink is not one"
        );
        assert!(outside.exists());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn create_refuses_a_symlinked_data_or_staging_directory() {
        let data = data();
        let link = data.join("link");
        std::os::unix::fs::symlink(&data, &link).unwrap();
        assert!(create(&link, HashMap::new()).is_err());
        let other = data.join("other");
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&data, other.join("staging")).unwrap();
        assert!(create(&other, HashMap::new()).is_err());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn sweep_checks_its_own_folders_and_ignores_symlinked_ones() {
        let data = data();
        let old = Duration::from_secs(25 * 60 * 60);
        // A symlinked data folder: the old folder behind it stays.
        let real = data.join("real");
        std::fs::create_dir_all(real.join("staging/old")).unwrap();
        age(&real.join("staging/old"), old);
        let link = data.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(sweep(&link, SystemTime::now()), 0);
        assert!(real.join("staging/old").exists());
        // A symlinked staging folder: the old folder behind it stays.
        let target = data.join("target");
        std::fs::create_dir_all(target.join("old")).unwrap();
        age(&target.join("old"), old);
        let other = data.join("other");
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&target, other.join("staging")).unwrap();
        assert_eq!(sweep(&other, SystemTime::now()), 0);
        assert!(target.join("old").exists());
        // The real folder still sweeps.
        assert_eq!(sweep(&real, SystemTime::now()), 1);
        assert!(!real.join("staging/old").exists());
        let _ = std::fs::remove_dir_all(&data);
    }
}
