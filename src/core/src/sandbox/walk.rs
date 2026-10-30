//! Find the protected names under a project before a sandboxed Linux command runs, so they can be
//! masked, and record the start state of every file for the copy-back. Std only, and it never
//! follows a symlink. Callers run it in `spawn_blocking`.
// The mask builder and the staging code that use this arrive in later tasks.
#![allow(dead_code)]

use crate::tools::ToolError;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

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
pub(crate) const INSTRUCTION_FILES: [&str; 3] = ["AGENTS.md", "AGENTS.override.md", "CLAUDE.md"];
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

/// ASCII case-insensitive, the way the copy-back refuses names.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
}

impl Fingerprint {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        }
    }
}

pub(crate) struct Walk {
    /// Sorted by path.
    pub masks: Vec<Mask>,
    /// Relative path to start state, for every regular file that is not masked or skipped.
    pub files: HashMap<PathBuf, Fingerprint>,
}

/// Paths are relative to the walked root.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WalkError {
    TooLarge { subtree: PathBuf },
    Unreadable { path: PathBuf },
}

/// Walk `root`, counting at most `limit` entries. Symlinks are never followed, masked or recorded.
/// A masked directory is not entered. A directory in `SKIP_BELOW_ROOT` is not entered either, but
/// its direct children are still checked for protected names and are not counted.
pub(crate) fn walk(root: &Path, limit: usize) -> Result<Walk, WalkError> {
    let mut masks = Vec::new();
    let mut files = HashMap::new();
    let mut count = 0usize;
    // (relative directory, whether only its direct children are checked)
    let mut pending = vec![(PathBuf::new(), false)];
    while let Some((relative, shallow)) = pending.pop() {
        let unreadable = || WalkError::Unreadable {
            path: relative.clone(),
        };
        let entries = std::fs::read_dir(root.join(&relative)).map_err(|_| unreadable())?;
        for entry in entries {
            let entry = entry.map_err(|_| unreadable())?;
            let name = entry.file_name();
            let path = relative.join(&name);
            if !shallow {
                count += 1;
                if count > limit {
                    let subtree = path
                        .components()
                        .next()
                        .map_or_else(|| path.clone(), |first| PathBuf::from(first.as_os_str()));
                    return Err(WalkError::TooLarge { subtree });
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
            } else if file_type.is_dir() {
                if !shallow {
                    let skipped = SKIP_BELOW_ROOT.iter().any(|s| name == OsStr::new(s));
                    pending.push((path, skipped));
                }
            } else if file_type.is_file() && !shallow {
                files.insert(path, Fingerprint::of(&meta));
            }
        }
    }
    masks.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Walk { masks, files })
}

const OUTSIDE_HOME: &str = "Choose a project folder inside your home folder";

/// A project must sit strictly inside the home folder, and the app's data folder must not sit
/// inside the project. With no home folder known, only the data folder rule applies.
pub(crate) fn check_location(
    project: &Path,
    home: Option<&Path>,
    data: Option<&Path>,
) -> Result<(), ToolError> {
    let home_is_project_or_inside_it = home.is_some_and(|home| home.starts_with(project));
    let data_inside_project = data.is_some_and(|data| data.starts_with(project));
    if home_is_project_or_inside_it || data_inside_project {
        return Err(ToolError::Execution(OUTSIDE_HOME.to_string()));
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
    fn walk_masks_protected_names_at_every_depth_and_case() {
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
        // Masked directories are not entered; ordinary files are recorded.
        assert!(w.files.contains_key(Path::new("a/b/keep.txt")));
        assert!(!w.files.keys().any(|p| p.starts_with(".git")));
        let meta = std::fs::symlink_metadata(root.0.join("a/b/keep.txt")).unwrap();
        assert_eq!(w.files[Path::new("a/b/keep.txt")], Fingerprint::of(&meta));
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
        assert!(w.files.is_empty());
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
        assert!(w.files.is_empty());
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
    }

    #[test]
    fn is_protected_ignores_ascii_case_only() {
        assert!(is_protected(OsStr::new(".GIT")));
        assert!(is_protected(OsStr::new(".Agents")));
        assert!(!is_protected(OsStr::new(".gitignore")));
        assert!(!is_protected(OsStr::new("git")));
    }

    #[test]
    fn location_refuses_home_its_ancestors_and_data_inside_project() {
        let message = "Choose a project folder inside your home folder";
        let refused = |project, home, data| {
            check_location(Path::new(project), home, data)
                .unwrap_err()
                .to_string()
        };
        let home = Some(Path::new("/home/u"));
        assert_eq!(refused("/home/u", home, None), message);
        assert_eq!(refused("/home", home, None), message);
        assert_eq!(refused("/", home, None), message);
        assert_eq!(
            refused("/home/u/p", home, Some(Path::new("/home/u/p/.data"))),
            message
        );
        assert!(check_location(
            Path::new("/home/u/p"),
            home,
            Some(Path::new("/home/u/.blackwall"))
        )
        .is_ok());
        assert!(check_location(Path::new("/home/u/p"), None, None).is_ok());
        // A sibling that merely shares a name prefix is not an ancestor.
        assert!(check_location(Path::new("/home/u2"), home, None).is_ok());
    }
}
