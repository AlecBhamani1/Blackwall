//! Find the protected names under a project before a sandboxed Linux command runs, so they can be
//! masked. Std only, and it never follows a symlink. Callers run it in `spawn_blocking`.
//!
//! Known limits, accepted on purpose:
//! - A protected name more than one level below a skipped directory (`node_modules/x/.git`) is not
//!   found, because scanning huge dependency and build trees before every command is too slow.
//!   Only a skipped directory's direct children are checked. Network is off by default, so the
//!   exposure is read-only.
//! - The project is pinned by a descriptor before the walk, the walk must start at that folder,
//!   and that descriptor is what gets mounted, so swapping the whole folder (before, during or
//!   after the walk) is caught. Inside it, the walk goes by path: a process outside the sandbox
//!   that renames folders within the project while the walk runs (moving a protected name into a
//!   folder already walked, say) can still leave that name unmasked.
//! - The walk stops after [`TIME_LIMIT`] and never enters another mounted file system, but a read
//!   that hangs (an unresponsive network or FUSE mount as the project itself) still blocks the
//!   blocking-pool thread until it returns.
//! - Names compare as raw bytes with ASCII case folding. A case-folding filesystem that also folds
//!   non-ASCII (or non-UTF-8) names can reach a protected name this does not recognise.

use crate::tools::ToolError;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) const PROTECTED: [&str; 11] = [
    ".git",
    ".hg",
    ".svn",
    ".blackwall",
    ".ssh",
    ".aws",
    ".gnupg",
    ".config",
    ".codex",
    ".claude",
    ".agents",
];
pub(crate) const SKIP_BELOW_ROOT: [&str; 7] = [
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    "build",
];
pub(crate) const MAX_ENTRIES: usize = 500_000;
/// How long the walk may take before the command is refused.
pub(crate) const TIME_LIMIT: Duration = Duration::from_secs(10);
/// ASCII case-insensitive on the raw bytes, the way the copy-back refuses names. A name with a
/// non-UTF-8 byte is just a different name: `.G\xffIT` is not `.git` and is not protected.
pub(crate) fn is_protected(name: &OsStr) -> bool {
    PROTECTED.iter().any(|p| name.eq_ignore_ascii_case(p))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MaskKind {
    Dir,
    File,
}

/// A protected name to hide, relative to the walked root.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Mask {
    pub path: PathBuf,
    pub kind: MaskKind,
}

#[derive(Debug)]
pub(crate) struct Walk {
    /// Sorted by path.
    pub masks: Vec<Mask>,
    /// The walked folder's device and inode (from the `lstat` it started with), to check against
    /// the pinned project.
    pub root: (u64, u64),
}

/// Paths are relative to the walked root.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WalkError {
    /// Too many entries in total; `subtree` is the top-level entry holding the most.
    TooLarge {
        subtree: PathBuf,
    },
    /// The root's own direct entries are already over the limit.
    RootTooLarge,
    Unreadable {
        path: PathBuf,
    },
    /// It took longer than its time limit.
    TimedOut,
    /// A folder on another file system than the project's (a mount point), which isn't entered.
    OtherFileSystem {
        path: PathBuf,
    },
}

impl From<WalkError> for ToolError {
    fn from(error: WalkError) -> Self {
        ToolError::Execution(match error {
            WalkError::TooLarge { subtree } => format!(
                "This project is too large to sandbox safely: {} has too many files. The command was not run.",
                subtree.display()
            ),
            WalkError::RootTooLarge => "This project folder itself has too many files to sandbox safely. The command was not run.".to_string(),
            WalkError::Unreadable { path } if path.as_os_str().is_empty() => "Blackwall couldn't read this project folder, so it can't check it before sandboxing. The command was not run.".to_string(),
            WalkError::Unreadable { path } => format!(
                "Blackwall couldn't read {} in this project, so it can't check it before sandboxing. Fix its permissions or move it out of the project. The command was not run.",
                path.display()
            ),
            WalkError::TimedOut => "Blackwall took too long to check this project folder, so the command was not run.".to_string(),
            WalkError::OtherFileSystem { path } => format!(
                "This project contains another mounted file system at {}, which Blackwall can't check safely. The command was not run.",
                path.display()
            ),
        })
    }
}

/// Walk `root`, counting at most `limit` entries. Symlinks are never followed, masked or recorded.
/// A masked directory is not entered. A directory in `SKIP_BELOW_ROOT` is not entered either, but
/// its direct children are still checked for protected names and are not counted. Past the limit
/// the error names the top-level entry that has the most entries counted so far (ties go to the
/// lower name), or the root itself when its direct entries alone are over the limit. It gives up
/// after [`TIME_LIMIT`], and refuses a folder on another file system rather than enter it.
pub(crate) fn walk(root: &Path, limit: usize) -> Result<Walk, WalkError> {
    walk_within(root, limit, TIME_LIMIT)
}
/// [`walk`] with its own time limit.
pub(crate) fn walk_within(root: &Path, limit: usize, time: Duration) -> Result<Walk, WalkError> {
    let deadline = Instant::now() + time;
    let late = || Instant::now() >= deadline;
    let unreadable_root = || WalkError::Unreadable {
        path: PathBuf::new(),
    };
    let top = std::fs::symlink_metadata(root).map_err(|_| unreadable_root())?;
    if !top.is_dir() {
        return Err(unreadable_root());
    }
    let mut masks = Vec::new();
    let mut count = 0usize;
    let mut per_top: HashMap<OsString, usize> = HashMap::new();
    // (relative directory, whether only its direct children are checked)
    let mut pending = vec![(PathBuf::new(), false)];
    while let Some((relative, shallow)) = pending.pop() {
        let at_root = relative.as_os_str().is_empty();
        let unreadable = || WalkError::Unreadable {
            path: relative.clone(),
        };
        if late() {
            return Err(WalkError::TimedOut);
        }
        let entries = std::fs::read_dir(root.join(&relative)).map_err(|_| unreadable())?;
        for entry in entries {
            if late() {
                return Err(WalkError::TimedOut);
            }
            let entry = entry.map_err(|_| unreadable())?;
            let name = entry.file_name();
            let path = relative.join(&name);
            if !shallow {
                count += 1;
                let top_level = relative
                    .components()
                    .next()
                    .map_or(name.as_os_str(), |first| first.as_os_str());
                match per_top.get_mut(top_level) {
                    Some(seen) => *seen += 1,
                    None => {
                        per_top.insert(top_level.to_os_string(), 1);
                    }
                }
                if at_root && count > limit {
                    return Err(WalkError::RootTooLarge);
                }
                if count > limit {
                    let largest = per_top
                        .into_iter()
                        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
                        .map(|(name, _)| PathBuf::from(name));
                    return Err(WalkError::TooLarge {
                        subtree: largest.unwrap_or(path),
                    });
                }
            }
            let meta = match std::fs::symlink_metadata(root.join(&path)) {
                Ok(meta) => meta,
                // Gone since it was listed.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(WalkError::Unreadable { path }),
            };
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if is_protected(&name) {
                let kind = if file_type.is_dir() {
                    MaskKind::Dir
                } else {
                    MaskKind::File
                };
                masks.push(Mask { path, kind });
            } else if file_type.is_dir() && !shallow {
                // A mount (a disk, a network share, /proc) may be huge, change underneath or hang.
                if meta.dev() != top.dev() {
                    return Err(WalkError::OtherFileSystem { path });
                }
                let skipped = SKIP_BELOW_ROOT.iter().any(|s| name == OsStr::new(s));
                pending.push((path, skipped));
            }
        }
    }
    masks.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Walk {
        masks,
        root: (top.dev(), top.ino()),
    })
}

const CONTAINS_HOME: &str =
    "This project folder is your home folder or contains it. Choose a smaller project folder.";
const DATA_INSIDE_PROJECT: &str = "Blackwall's data folder is inside this project. Choose a project folder that doesn't contain Blackwall's data.";
const NO_HOME: &str = "Blackwall couldn't find your home folder, so it won't sandbox this command.";
const UNRESOLVED: &str =
    "Blackwall couldn't resolve a folder it needs to check, so it won't sandbox this command.";

/// Canonicalize a path whose last components may not exist yet: resolve the nearest existing
/// ancestor and put the missing names back.
fn canonical_allowing_missing(path: &Path) -> std::io::Result<PathBuf> {
    let mut missing = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&current) {
            Ok(mut resolved) => {
                resolved.extend(missing.iter().rev());
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = current.file_name().map(OsStr::to_os_string) else {
                    return Err(error);
                };
                missing.push(name);
                current.pop();
                if current.as_os_str().is_empty() {
                    current = PathBuf::from(".");
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// A project must not be the home folder or one of its ancestors, and the app's data folder must
/// not sit inside the project. Paths are resolved first (`..`, relative paths and symlinks), and anything
/// that cannot be resolved, including a missing home folder, is refused.
pub(crate) fn check_location(
    project: &Path,
    home: Option<&Path>,
    data: Option<&Path>,
) -> Result<(), ToolError> {
    let refuse = |message: &str| Err(ToolError::Execution(message.to_string()));
    let Some(home) = home else {
        return refuse(NO_HOME);
    };
    let Ok(home) = std::fs::canonicalize(home) else {
        return refuse(NO_HOME);
    };
    let Ok(project) = std::fs::canonicalize(project) else {
        return refuse(UNRESOLVED);
    };
    if home.starts_with(&project) {
        return refuse(CONTAINS_HOME);
    }
    if let Some(data) = data {
        let Ok(data) = canonical_allowing_missing(data) else {
            return refuse(UNRESOLVED);
        };
        if data.starts_with(&project) {
            return refuse(DATA_INSIDE_PROJECT);
        }
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("blackwall-walk-{}", rand::random::<u64>()));
            std::fs::create_dir_all(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
        fn dir(&self, relative: &str) {
            std::fs::create_dir_all(self.0.join(relative)).unwrap();
        }
        fn file(&self, relative: &str) {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn kinds(walk: &Walk) -> Vec<(&str, &MaskKind)> {
        walk.masks
            .iter()
            .map(|m| (m.path.to_str().unwrap(), &m.kind))
            .collect()
    }

    #[test]
    fn walk_masks_nested_protected_names_in_any_case() {
        let root = Root::new();
        root.dir(".git/objects");
        root.dir("a/b/.ssh");
        root.dir(".Claude");
        root.file(".codex");
        root.file("a/b/keep.txt");
        let w = walk(&root.0, MAX_ENTRIES).unwrap();
        assert_eq!(
            kinds(&w),
            [
                (".Claude", &MaskKind::Dir),
                (".codex", &MaskKind::File),
                (".git", &MaskKind::Dir),
                ("a/b/.ssh", &MaskKind::Dir),
            ]
        );
    }

    #[test]
    fn walk_skips_heavy_dirs_but_checks_their_top_level() {
        let root = Root::new();
        root.dir("node_modules/.git");
        root.dir("node_modules/x/.git");
        root.file("node_modules/x/index.js");
        root.dir("src/target/.ssh");
        root.dir("src/target/y/.ssh");
        // Three entries outside the skipped directories: node_modules, src, src/target.
        let w = walk(&root.0, 3).unwrap();
        assert_eq!(
            kinds(&w),
            [
                ("node_modules/.git", &MaskKind::Dir),
                ("src/target/.ssh", &MaskKind::Dir),
            ]
        );
    }

    #[test]
    fn walk_never_follows_or_masks_symlinks() {
        let root = Root::new();
        let outside = Root::new();
        outside.file("secret");
        root.dir("inside");
        symlink(&outside.0, root.0.join(".ssh")).unwrap();
        symlink(".", root.0.join("loop")).unwrap();
        symlink(root.0.join("inside"), root.0.join("inside/.git")).unwrap();
        let w = walk(&root.0, MAX_ENTRIES).unwrap();
        assert!(w.masks.is_empty());
    }

    #[test]
    fn walk_limit_names_the_subtree() {
        let root = Root::new();
        for n in 0..20 {
            root.file(&format!("big/deep/f{n}"));
        }
        assert_eq!(
            walk(&root.0, 10).err(),
            Some(WalkError::TooLarge {
                subtree: "big".into()
            })
        );
    }

    #[test]
    fn walk_reports_an_unreadable_directory() {
        let root = Root::new();
        assert_eq!(
            walk(&root.0.join("missing"), 10).err(),
            Some(WalkError::Unreadable {
                path: PathBuf::new()
            })
        );
        // A link in the project's place is not walked through.
        symlink(&root.0, root.0.join("link")).unwrap();
        assert_eq!(
            walk(&root.0.join("link"), 10).err(),
            Some(WalkError::Unreadable {
                path: PathBuf::new()
            })
        );
    }

    #[test]
    fn walk_records_the_identity_of_the_folder_it_walked() {
        let root = Root::new();
        let meta = std::fs::metadata(&root.0).unwrap();
        assert_eq!(walk(&root.0, 10).unwrap().root, (meta.dev(), meta.ino()));
    }

    #[test]
    fn walk_gives_up_at_its_time_limit() {
        let root = Root::new();
        root.file("a/b");
        assert_eq!(
            walk_within(&root.0, MAX_ENTRIES, Duration::ZERO).err(),
            Some(WalkError::TimedOut)
        );
        assert!(walk_within(&root.0, MAX_ENTRIES, TIME_LIMIT).is_ok());
        assert_eq!(
            ToolError::from(WalkError::TimedOut).to_string(),
            "Blackwall took too long to check this project folder, so the command was not run."
        );
    }

    #[test]
    fn walk_never_enters_another_file_system() {
        assert_eq!(
            ToolError::from(WalkError::OtherFileSystem {
                path: "mnt/disk".into()
            })
            .to_string(),
            "This project contains another mounted file system at mnt/disk, which Blackwall can't check safely. The command was not run."
        );
        // /dev nearly always has others mounted directly inside it (/dev/pts, /dev/shm). Every
        // entry is checked as its folder is listed, so the walk stops at the first of them.
        let dev = Path::new("/dev");
        let device = |path: &Path| std::fs::symlink_metadata(path).unwrap().dev();
        let mounted: Vec<PathBuf> = std::fs::read_dir(dev)
            .unwrap()
            .flatten()
            .filter(|entry| {
                std::fs::symlink_metadata(entry.path())
                    .is_ok_and(|meta| meta.is_dir() && meta.dev() != device(dev))
            })
            .map(|entry| PathBuf::from(entry.file_name()))
            .collect();
        if mounted.is_empty() {
            eprintln!("skipped: nothing is mounted directly inside /dev here");
            return;
        }
        match walk(dev, MAX_ENTRIES) {
            Err(WalkError::OtherFileSystem { path }) => {
                assert!(mounted.contains(&path), "{path:?}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn is_protected_ignores_ascii_case_only() {
        assert!(is_protected(OsStr::new(".GIT")));
        assert!(is_protected(OsStr::new(".Agents")));
        assert!(!is_protected(OsStr::new(".gitignore")));
        assert!(!is_protected(OsStr::new("git")));
    }

    #[test]
    fn is_protected_does_not_fold_non_utf8_names() {
        use std::os::unix::ffi::OsStrExt;
        assert!(!is_protected(OsStr::from_bytes(b".G\xffIT")));
        assert!(is_protected(OsStr::from_bytes(b".GIT")));
    }

    #[test]
    fn walk_limit_names_the_largest_top_level_entry_not_the_one_that_tips_it() {
        // `big` fits under the limit by itself (13 entries of 20), but the total does not (25), so
        // whichever entry tips the limit, only counting per top-level entry names `big`. The
        // order the directories are read in is up to the filesystem, so try several names.
        for big in ["a_big", "m_big", "z_big"] {
            let root = Root::new();
            for n in 0..12 {
                root.file(&format!("{big}/f{n}"));
            }
            for small in ["b", "d", "n", "y"] {
                for n in 0..2 {
                    root.file(&format!("{small}/f{n}"));
                }
            }
            assert_eq!(
                walk(&root.0, 20).err(),
                Some(WalkError::TooLarge {
                    subtree: big.into()
                })
            );
        }
    }

    #[test]
    fn walk_limit_on_a_flat_root_names_the_root() {
        let root = Root::new();
        for n in 0..20 {
            root.file(&format!("f{n}"));
        }
        assert_eq!(walk(&root.0, 10).err(), Some(WalkError::RootTooLarge));
        let message = ToolError::from(WalkError::RootTooLarge).to_string();
        assert_eq!(
            message,
            "This project folder itself has too many files to sandbox safely. The command was not run."
        );
        let named = ToolError::from(WalkError::TooLarge {
            subtree: "big".into(),
        })
        .to_string();
        assert_eq!(
            named,
            "This project is too large to sandbox safely: big has too many files. The command was not run."
        );
    }

    #[test]
    fn unreadable_messages_name_the_folder() {
        let with_path = ToolError::from(WalkError::Unreadable {
            path: "a/locked".into(),
        })
        .to_string();
        assert_eq!(
            with_path,
            "Blackwall couldn't read a/locked in this project, so it can't check it before sandboxing. Fix its permissions or move it out of the project. The command was not run."
        );
        let root = ToolError::from(WalkError::Unreadable {
            path: PathBuf::new(),
        })
        .to_string();
        assert_eq!(
            root,
            "Blackwall couldn't read this project folder, so it can't check it before sandboxing. The command was not run."
        );
    }

    #[test]
    fn walk_masks_a_fifo_with_a_protected_name_as_a_file() {
        let root = Root::new();
        let status = std::process::Command::new("mkfifo")
            .arg(root.0.join(".git"))
            .status()
            .unwrap();
        assert!(status.success());
        let w = walk(&root.0, MAX_ENTRIES).unwrap();
        assert_eq!(kinds(&w), [(".git", &MaskKind::File)]);
    }

    #[test]
    fn walk_masks_a_protected_file_directly_inside_a_skipped_dir() {
        let root = Root::new();
        root.file("node_modules/.codex");
        root.file("node_modules/pkg/.codex");
        let w = walk(&root.0, MAX_ENTRIES).unwrap();
        assert_eq!(kinds(&w), [("node_modules/.codex", &MaskKind::File)]);
    }

    #[test]
    fn walk_reports_an_unreadable_directory_in_the_tree() {
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::geteuid().is_root() {
            return; // root reads everything, so there is nothing to refuse
        }
        let root = Root::new();
        root.dir("a/locked");
        let locked = root.0.join("a/locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = walk(&root.0, 100);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            result.err(),
            Some(WalkError::Unreadable {
                path: "a/locked".into()
            })
        );
    }

    /// A real home with a project inside it and a data folder beside the project.
    struct Home {
        root: Root,
        home: PathBuf,
        project: PathBuf,
    }
    impl Home {
        fn new() -> Self {
            let root = Root::new();
            let home = root.0.join("home");
            let project = home.join("p");
            std::fs::create_dir_all(&project).unwrap();
            Self {
                root,
                home,
                project,
            }
        }
        fn check(&self, project: &Path, data: Option<&Path>) -> Result<(), String> {
            check_location(project, Some(&self.home), data).map_err(|e| e.to_string())
        }
    }
    const CONTAINS_HOME: &str =
        "This project folder is your home folder or contains it. Choose a smaller project folder.";
    const DATA_INSIDE: &str = "Blackwall's data folder is inside this project. Choose a project folder that doesn't contain Blackwall's data.";

    #[test]
    fn location_refuses_home_its_ancestors_and_data_inside_project() {
        let h = Home::new();
        assert_eq!(h.check(&h.home, None), Err(CONTAINS_HOME.into()));
        assert_eq!(h.check(&h.root.0, None), Err(CONTAINS_HOME.into()));
        assert_eq!(h.check(Path::new("/"), None), Err(CONTAINS_HOME.into()));
        // The data folder does not exist yet.
        assert_eq!(
            h.check(&h.project, Some(&h.project.join(".data/deeper"))),
            Err(DATA_INSIDE.into())
        );
        assert_eq!(
            h.check(&h.project, Some(&h.project)),
            Err(DATA_INSIDE.into())
        );
        assert_eq!(
            h.check(&h.project, Some(&h.home.join(".blackwall"))),
            Ok(())
        );
        assert_eq!(h.check(&h.project, None), Ok(()));
        // A sibling that merely shares a name prefix is not the home folder.
        let sibling = h.root.0.join("home2");
        std::fs::create_dir(&sibling).unwrap();
        assert_eq!(h.check(&sibling, None), Ok(()));
    }

    #[test]
    fn location_resolves_dotdot_and_relative_paths() {
        let h = Home::new();
        assert_eq!(
            h.check(&h.project.join(".."), None),
            Err(CONTAINS_HOME.into()),
            "p/.. is the home folder"
        );
        // data written with .. that lands inside the project
        std::fs::create_dir(h.home.join("x")).unwrap();
        assert_eq!(
            h.check(&h.project, Some(&h.home.join("x/../p/.data"))),
            Err(DATA_INSIDE.into())
        );
        // A relative path to the home folder, built from the current directory.
        let cwd = std::env::current_dir().unwrap();
        let up = "../".repeat(cwd.components().count() - 1);
        let relative = PathBuf::from(format!(
            "{up}{}",
            h.home.strip_prefix("/").unwrap().display()
        ));
        assert!(relative.is_relative());
        assert_eq!(h.check(&relative, None), Err(CONTAINS_HOME.into()));
    }

    #[test]
    fn location_resolves_symlinked_home_and_project() {
        let h = Home::new();
        let home_link = h.root.0.join("home-link");
        symlink(&h.home, &home_link).unwrap();
        let project_link = h.root.0.join("project-link");
        symlink(&h.project, &project_link).unwrap();
        // The home folder through a link is still the home folder.
        assert_eq!(h.check(&home_link, None), Err(CONTAINS_HOME.into()));
        assert_eq!(
            check_location(&h.home, Some(&home_link), None).map_err(|e| e.to_string()),
            Err(CONTAINS_HOME.into())
        );
        // A project reached through a link, and its data reached through another.
        assert_eq!(
            check_location(&project_link, Some(&home_link), None).map_err(|e| e.to_string()),
            Ok(())
        );
        assert_eq!(
            h.check(&h.home.join("p"), Some(&project_link.join(".data"))),
            Err(DATA_INSIDE.into())
        );
        // A link inside home that points outside it: the real place is what counts.
        let elsewhere = Home::new();
        let escape = h.home.join("escape");
        symlink(&elsewhere.root.0, &escape).unwrap();
        assert_eq!(h.check(&escape, None), Ok(()));
    }

    #[test]
    fn location_fails_closed_without_a_resolvable_home_or_project() {
        let h = Home::new();
        let no_home = "Blackwall couldn't find your home folder, so it won't sandbox this command.";
        let unresolved =
            "Blackwall couldn't resolve a folder it needs to check, so it won't sandbox this command.";
        let err =
            |p: &Path, home: Option<&Path>| check_location(p, home, None).unwrap_err().to_string();
        assert_eq!(err(&h.project, None), no_home);
        assert_eq!(err(&h.project, Some(&h.root.0.join("nope"))), no_home);
        assert_eq!(err(&h.root.0.join("nope"), Some(&h.home)), unresolved);
    }
}
