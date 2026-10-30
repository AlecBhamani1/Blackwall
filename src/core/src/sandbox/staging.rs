//! The folders one Workspace-write command stages its changes in, the locks that keep them apart,
//! and the sweep that removes the ones a crash left behind or a run kept for the owner.
//!
//! Two locks, both `flock`s on files opened without following a symlink:
//! - `<data>/staging/<dev>-<ino>.lock` (the project's device and inode in hex): one
//!   Workspace-write run per project at a time, across processes, from before the start walk
//!   until the apply step and its cleanup end ([`lock_project`]). These small files stay.
//! - `<staging folder>/.lease`: held by the run that made the folder for as long as it lives. The
//!   sweep skips a folder whose lease is held, however old it looks ([`create`], [`sweep`]).

use super::walk::Fingerprint;
use crate::context::MAX_COMMAND_TIMEOUT_SECONDS;
use crate::tools::ToolError;
use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags};
use rustix::io::Errno;
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// How long a staging folder is kept before the next sweep removes it.
///
/// A folder whose run is still going holds its lease, which the sweep honours, so age alone never
/// removes a live one. A day also far outlasts any command: the time limit defaults to 120 s and
/// cannot be set above [`MAX_COMMAND_TIMEOUT_SECONDS`] (one hour), and waiting for the project's
/// lock takes at most as long again.
pub(crate) const KEEP_FOR: Duration = Duration::from_secs(24 * 60 * 60);

// A day is at least 8 times the longest time limit a command can have.
const _: () = assert!(KEEP_FOR.as_secs() >= 8 * MAX_COMMAND_TIMEOUT_SECONDS);

/// One command's staging folder: `<data>/staging/<run-id>`, with the overlay's upper and work
/// folders in it, and what the project looked like when the command started.
pub(crate) struct Staging {
    pub root: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
    pub start: HashMap<PathBuf, Fingerprint>,
    /// `root`'s device and inode, so the remover touches only this folder.
    pub id: (u64, u64),
    /// The held lease on `root/.lease`; dropping it lets the sweep have the folder.
    _lease: std::fs::File,
}

/// Make a private staging folder under `<data>/staging` with its `upper` and `work` folders (all
/// 0700) and take its lease, after the checks and sweep every run folder gets (a symlinked data or
/// staging folder is refused). `start` is what the project looked like before the command ran.
pub(crate) fn create(
    data: &Path,
    start: HashMap<PathBuf, Fingerprint>,
) -> Result<Staging, ToolError> {
    let failed = |_| ToolError::Execution(super::PREPARE_FAILED.into());
    let root = super::linux::private_scratch(data, "").map_err(failed)?;
    let meta = std::fs::symlink_metadata(&root).map_err(failed)?;
    // Taken before anything else is made: a folder without a lease is the sweep's once it is
    // old, and this one is new.
    let lease = lock_file(&root.join(".lease")).map_err(failed)?;
    fs::flock(&lease, FlockOperation::NonBlockingLockExclusive).map_err(|e| failed(e.into()))?;
    let (upper, work) = (root.join("upper"), root.join("work"));
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
        id: (meta.dev(), meta.ino()),
        _lease: lease,
    })
}

/// `path` opened read-write for this user only (0600), made if missing, never through a symlink.
fn lock_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

pub(crate) const PROJECT_BUSY: &str =
    "Another Workspace-write command is still running in this project.";

/// Take the project's Workspace-write lock, `<data>/staging/<hex dev>-<hex ino>.lock`, waiting up
/// to `wait` for another run (in this process or another) to let it go. Hold the returned file
/// until the apply step and cleanup have ended. Blocking: call it in `spawn_blocking`.
pub(crate) fn lock_project(
    data: &Path,
    project: (u64, u64),
    wait: Duration,
) -> Result<std::fs::File, ToolError> {
    let failed = || ToolError::Execution(super::PREPARE_FAILED.into());
    let staging = super::linux::staging_directory(data).map_err(|_| failed())?;
    let path = staging.join(format!("{:x}-{:x}.lock", project.0, project.1));
    let file = lock_file(&path).map_err(|_| failed())?;
    let deadline = Instant::now() + wait;
    loop {
        match fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(file),
            Err(Errno::WOULDBLOCK | Errno::INTR) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(Errno::WOULDBLOCK | Errno::INTR) => {
                return Err(ToolError::Execution(PROJECT_BUSY.into()))
            }
            Err(_) => return Err(failed()),
        }
    }
}

/// Whether nothing holds the lease of the staging folder `dir`: there is none (a probe or run
/// folder, or a run that never got one), or this call could take it. Taken, it is held for as
/// long as the returned file is. Anything that can't be checked counts as held.
fn lease_free(dir: &Path) -> Option<Option<std::fs::File>> {
    let lease = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join(".lease"));
    match lease {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
        Ok(file) => fs::flock(&file, FlockOperation::NonBlockingLockExclusive)
            .ok()
            .map(|()| Some(file)),
    }
}

/// Remove the staging folder at `path` (see [`remove_tree`]), whatever its identity now is, as long
/// as it is a real folder.
pub(crate) fn remove_dir(path: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Err(std::io::Error::other("not a real folder"));
    }
    remove_tree(path, (meta.dev(), meta.ino()))
}

/// Remove the folder at `path` and everything in it, which a sandboxed command made and may have
/// made hostile: folders and files with mode 000, symlinks to anywhere, deep nesting. It works
/// through descriptors only, one folder at a time, and never follows a symlink:
/// - The folder must be the one at `path` now, with device and inode `id`, or nothing is removed.
/// - Each folder is opened `O_PATH | O_NOFOLLOW`, checked to be the folder just listed and on
///   the same device (no mount is crossed), given `u+rwx` through its descriptor, and only then
///   opened for reading. Symlinks, whiteouts and files are unlinked, never opened.
/// - Going back up opens `..` and checks it is the folder it came from, so only one folder is
///   open at a time however deep the tree goes.
///
/// On any failure it stops and returns the error, leaving what is left in place.
pub(crate) fn remove_tree(path: &Path, id: (u64, u64)) -> std::io::Result<()> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(std::io::Error::other("no parent folder"));
    };
    let parent = fs::open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let (dev, root_ino) = id;
    let mut current = enter(&parent, name, dev, root_ino)?;
    struct Frame {
        name: OsString,
        ino: u64,
        folders: Vec<OsString>,
    }
    let mut stack = vec![Frame {
        name: name.to_os_string(),
        ino: root_ino,
        folders: clear(&current, dev)?,
    }];
    loop {
        let Some(top) = stack.last_mut() else {
            return Ok(());
        };
        if let Some(folder) = top.folders.pop() {
            let stat = fs::statat(&current, folder.as_os_str(), AtFlags::SYMLINK_NOFOLLOW)?;
            let child = enter(&current, folder.as_os_str(), dev, ino(&stat))?;
            let folders = clear(&child, dev)?;
            stack.push(Frame {
                name: folder,
                ino: ino(&stat),
                folders,
            });
            current = child;
            continue;
        }
        // `top` is empty now: remove it from the folder above.
        let done = stack.pop().ok_or_else(|| std::io::Error::other("empty"))?;
        let Some(frame) = stack.last() else {
            return remove_empty(&parent, &done.name, (dev, done.ino));
        };
        let up = fs::openat(
            &current,
            "..",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let stat = fs::fstat(&up)?;
        if (dev_of(&stat), ino(&stat)) != (dev, frame.ino) {
            return Err(std::io::Error::other("a folder moved"));
        }
        remove_empty(&up, &done.name, (dev, done.ino))?;
        current = up;
    }
}
/// `rmdir` the empty folder `name` in `dir` if it is still the folder `id`.
fn remove_empty(dir: impl AsFd, name: &std::ffi::OsStr, id: (u64, u64)) -> std::io::Result<()> {
    let stat = fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if (dev_of(&stat), ino(&stat)) != id {
        return Err(std::io::Error::other("a folder moved"));
    }
    Ok(fs::unlinkat(&dir, name, AtFlags::REMOVEDIR)?)
}
// `Stat`'s field types differ between architectures.
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
fn ino(stat: &fs::Stat) -> u64 {
    stat.st_ino as u64
}
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
fn dev_of(stat: &fs::Stat) -> u64 {
    stat.st_dev as u64
}
/// Open the folder `name` in `dir`, which must still be the folder `(dev, ino)`, give its owner
/// `rwx` through an `O_PATH` descriptor (so a mode-000 folder can be read), and open it for reading.
fn enter(dir: impl AsFd, name: &std::ffi::OsStr, dev: u64, ino: u64) -> std::io::Result<OwnedFd> {
    let path = fs::openat(
        &dir,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let stat = fs::fstat(&path)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory
        || (dev_of(&stat), self::ino(&stat)) != (dev, ino)
    {
        return Err(std::io::Error::other("not the folder that was listed"));
    }
    if stat.st_mode & 0o700 != 0o700 {
        // fchmod can't take an O_PATH descriptor; its /proc link names exactly that folder.
        let link = format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&path));
        fs::chmod(
            link.as_str(),
            Mode::from_raw_mode((stat.st_mode & 0o7777) | 0o700),
        )?;
    }
    let opened = fs::openat(
        &path,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let again = fs::fstat(&opened)?;
    if (dev_of(&again), self::ino(&again)) != (dev, ino) {
        return Err(std::io::Error::other("not the folder that was listed"));
    }
    Ok(opened)
}
/// Unlink everything in `dir` but its folders, and return their names. A folder on another device
/// (a mount) stops the removal.
fn clear(dir: impl AsFd, dev: u64) -> std::io::Result<Vec<OsString>> {
    let mut folders = Vec::new();
    for entry in fs::Dir::read_from(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let name = OsString::from_vec(name.to_vec());
        let stat = fs::statat(&dir, name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            if dev_of(&stat) != dev {
                return Err(std::io::Error::other("another file system inside"));
            }
            folders.push(name);
        } else {
            fs::unlinkat(&dir, name.as_os_str(), AtFlags::empty())?;
        }
    }
    Ok(folders)
}

/// Remove the staging folders under `<data>/staging` that were last changed more than
/// [`KEEP_FOR`] before `now` and whose lease nobody holds (the leftovers of a crash, or a run's
/// changes kept for the owner), with [`remove_tree`], and say how many went. Nothing is removed
/// unless `<data>` and `<data>/staging` are both real folders, not symlinks. Symlinks and plain
/// files inside (the projects' lock files among them) are left alone. Every run folder is made
/// through [`create`] or the run and probe helpers, which sweep first, so this is the only place
/// that decides what is old.
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
        if !is_old {
            continue;
        }
        // Held while the folder is removed, so its run can't be mid-way through it.
        let Some(_lease) = lease_free(&entry.path()) else {
            continue;
        };
        if remove_dir(&entry.path()).is_ok() {
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
            2,
            "the two free folders left; the symlink is not one, and the leased one stays"
        );
        assert!(outside.exists());
        assert!(staging.root.exists());
        drop(staging);
        assert_eq!(sweep(&data, SystemTime::now() + 2 * KEEP_FOR), 1);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_leased_folder_survives_the_sweep_however_old() {
        let data = data();
        let staging = create(&data, HashMap::new()).unwrap();
        let lease = staging.root.join(".lease");
        let meta = std::fs::symlink_metadata(&lease).unwrap();
        assert!(meta.is_file());
        assert_eq!(meta.mode() & 0o777, 0o600);
        age(&staging.root, Duration::from_secs(25 * 60 * 60));
        assert_eq!(sweep(&data, SystemTime::now()), 0);
        assert!(staging.upper.exists());
        // Another process's run holds it the same way: a separate open file description.
        let root = staging.root.clone();
        drop(staging);
        let held = std::fs::File::open(root.join(".lease")).unwrap();
        fs::flock(&held, FlockOperation::NonBlockingLockExclusive).unwrap();
        assert_eq!(sweep(&data, SystemTime::now()), 0);
        drop(held);
        assert_eq!(sweep(&data, SystemTime::now()), 1);
        assert!(!root.exists());
        // A lease that is a symlink can't be checked, so the folder stays.
        let odd = data.join("staging/odd");
        std::fs::create_dir(&odd).unwrap();
        std::os::unix::fs::symlink("/dev/null", odd.join(".lease")).unwrap();
        age(&odd, Duration::from_secs(25 * 60 * 60));
        assert_eq!(sweep(&data, SystemTime::now()), 0);
        assert!(odd.exists());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn the_remover_clears_hostile_trees_without_following_links() {
        use std::os::unix::fs::PermissionsExt;
        let data = data();
        let outside = data.join("outside");
        std::fs::create_dir_all(outside.join("keep")).unwrap();
        std::fs::write(outside.join("keep/file"), "outside").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o500)).unwrap();
        let root = data.join("staging/run");
        std::fs::create_dir_all(root.join("upper/a/b/c")).unwrap();
        std::fs::write(root.join("upper/a/b/c/f"), "x").unwrap();
        std::fs::write(root.join("upper/a/g"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("upper/link")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("upper/a/b/link")).unwrap();
        std::os::unix::fs::symlink(outside.join("keep/file"), root.join("upper/a/file-link"))
            .unwrap();
        // Deep enough that one descriptor per level would run out of them under a low limit.
        let mut deep = root.join("upper/deep");
        for _ in 0..300 {
            deep.push("d");
        }
        std::fs::create_dir_all(&deep).unwrap();
        // Mode 000 on files and folders, innermost first, the root itself last.
        let lock = |path: &Path| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap()
        };
        for path in [
            "upper/a/b/c/f",
            "upper/a/b/c",
            "upper/a/b",
            "upper/a/g",
            "upper/a",
        ] {
            lock(&root.join(path));
        }
        lock(&root.join("upper"));
        lock(&root);
        let meta = std::fs::symlink_metadata(&root).unwrap();
        // The wrong identity: nothing is touched.
        assert!(remove_tree(&root, (meta.dev(), meta.ino() + 1)).is_err());
        assert!(std::fs::symlink_metadata(&root).is_ok());
        remove_tree(&root, (meta.dev(), meta.ino())).unwrap();
        assert!(std::fs::symlink_metadata(&root).is_err());
        assert_eq!(
            std::fs::read_to_string(outside.join("keep/file")).unwrap(),
            "outside"
        );
        assert_eq!(
            std::fs::metadata(&outside).unwrap().mode() & 0o777,
            0o500,
            "the outside folder's mode is untouched"
        );
        // A symlink in the folder's place is never followed.
        let link = data.join("staging/link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let target = std::fs::metadata(&outside).unwrap();
        assert!(remove_tree(&link, (target.dev(), target.ino())).is_err());
        assert!(remove_dir(&link).is_err());
        assert!(outside.join("keep/file").exists());
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn one_workspace_write_run_per_project_at_a_time() {
        use std::sync::{Arc, Barrier};
        let data = data();
        let project = (0x803, 0x1234_5678);
        let first = lock_project(&data, project, Duration::ZERO).unwrap();
        let path = data.join("staging/803-12345678.lock");
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.is_file());
        assert_eq!(meta.mode() & 0o777, 0o600);
        // Held: a second run gives up after its wait, with the plain message.
        assert_eq!(
            lock_project(&data, project, Duration::from_millis(100))
                .unwrap_err()
                .to_string(),
            "Another Workspace-write command is still running in this project."
        );
        // A different project is not held up.
        drop(lock_project(&data, (0x803, 1), Duration::ZERO).unwrap());
        // The second waits until the first lets go, then runs.
        let barrier = Arc::new(Barrier::new(2));
        let (data2, barrier2) = (data.clone(), barrier.clone());
        let second = std::thread::spawn(move || {
            barrier2.wait();
            let started = Instant::now();
            let held = lock_project(&data2, project, Duration::from_secs(10)).unwrap();
            (started.elapsed(), held)
        });
        barrier.wait();
        std::thread::sleep(Duration::from_millis(300));
        drop(first);
        let (waited, held) = second.join().unwrap();
        assert!(waited >= Duration::from_millis(250), "{waited:?}");
        drop(held);
        // A symlink in the lock file's place is refused, never followed.
        let other = (0x803, 0x99);
        std::os::unix::fs::symlink("/dev/null", data.join("staging/803-99.lock")).unwrap();
        assert!(lock_project(&data, other, Duration::ZERO).is_err());
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
