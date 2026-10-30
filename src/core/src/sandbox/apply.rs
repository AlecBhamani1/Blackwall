//! Copy what a Workspace-write command changed (the overlay's upper folder) back into the
//! project, through descriptor-relative, no-follow operations only.
//!
//! It runs after the sandbox has fully exited, so nothing writes to `upper` any more; the only
//! writers left are the owner's own processes in the project. It works in two passes:
//!
//! 1. **Plan.** Walk `upper` read-only and decide every entry: refused (a protected name, an
//!    instruction file, set-id bits or a file capability, a device, pipe or socket, a symlink
//!    that leads out), or a planned file, symlink, folder or deletion. More than
//!    [`MAX_ENTRIES`] entries, or overlay metadata this does not understand (redirect, metacopy,
//!    `trusted.*`, any `user.overlay.*` but origin, impure, nlink, uuid, protattr and opaque `y`)
//!    anywhere, and nothing is applied at all.
//! 2. **Apply.** Carry out the plan. Every project folder is opened one name at a time from its
//!    already-open parent with `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV)`,
//!    so no symlink and no other mount in the project is ever followed. A file is copied into a
//!    fresh temp file (`.<name>.blackwall-<hex>`) next to its target, with no xattrs and only the
//!    `0o777` mode bits, synced, and renamed into place. The staged inode itself never enters
//!    the project.
//!
//! What the owner changed since the command started wins. An entry may replace or delete only
//! what is there now if it is the same regular file, unchanged, that the start walk recorded
//! (`start`), or if nothing is there and nothing was. Anything else is a conflict: the project's
//! version stays and the path is reported. The start walk records regular files only, so a
//! symlink, a file below a skipped folder (`node_modules`, `target`, ...) or anything else it did
//! not see looks new and is never replaced or deleted. That is the safe direction. Folders are
//! merged; an existing folder keeps its mode.
//!
//! Each entry is checked again just before its rename, unlink or symlink, from the pinned parent.
//! That makes each entry's change atomic, but only best-effort against a writer racing that very
//! moment, and the apply as a whole is not atomic: there is no journal, and a crash part way
//! leaves the entries done so far (and possibly a temp file) in place. A folder that an outside
//! process moves while it is pinned takes its changes with it.
//!
//! Known limits, accepted on purpose: user xattrs exist only on files and folders, so those are
//! the only entries whose overlay metadata is read; `trusted.*` is visible only to a privileged
//! process. A symlink's target must be relative, stay inside the project lexically, use `..` only
//! at its start, name no protected component and run through no symlink that already exists.

use super::walk::{is_protected, Fingerprint, INSTRUCTION_FILES, MAX_ENTRIES};
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, RenameFlags, ResolveFlags, Stat};
use rustix::io::Errno;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

/// The deepest folder nesting applied or deleted. Each level holds a descriptor open.
const MAX_DEPTH: usize = 256;
/// How many paths each list in the report shows.
const LIST_LIMIT: usize = 200;
const SET_ID: u32 = 0o6000;

/// Why a staged change was not applied.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    ProtectedName,
    InstructionFile,
    LinkOutside,
    SpecialFile,
    /// Set-user-ID or set-group-ID bits, or a file capability.
    SetId,
    UnsupportedOverlay,
    Io,
}
impl Refusal {
    fn reason(self) -> &'static str {
        match self {
            Self::ProtectedName => "a protected name such as .git or .ssh",
            Self::InstructionFile => "an agent instruction file",
            Self::LinkOutside => "a link that leads outside the project, or a path through a link",
            Self::SpecialFile => "a device, pipe or socket",
            Self::SetId => "set-user-ID, set-group-ID or file capabilities",
            Self::UnsupportedOverlay => "staged in a form Blackwall can't apply safely",
            Self::Io => "couldn't be read or written safely, or there were too many changes",
        }
    }
}

/// What [`apply`] did, each list sorted by path. An empty path stands for the whole apply.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Report {
    pub applied: Vec<PathBuf>,
    pub refused: Vec<(PathBuf, Refusal)>,
    pub conflicted: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
}
impl Report {
    fn refuse(&mut self, path: &Path, why: Refusal) {
        self.refused.push((path.to_path_buf(), why));
    }
    fn conflict(&mut self, path: &Path) {
        self.conflicted.push(path.to_path_buf());
    }
    fn sorted(mut self) -> Self {
        self.applied.sort();
        self.deleted.sort();
        self.conflicted.sort();
        self.refused.sort_by(|a, b| a.0.cmp(&b.0));
        self
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Workspace changes: {} applied, {} deleted, {} refused, {} conflicted",
            self.applied.len(),
            self.deleted.len(),
            self.refused.len(),
            self.conflicted.len()
        )?;
        list(f, "Applied", self.applied.iter().map(|p| shown(p)))?;
        list(f, "Deleted", self.deleted.iter().map(|p| shown(p)))?;
        list(
            f,
            "Refused",
            self.refused
                .iter()
                .map(|(p, why)| format!("{} ({})", shown(p), why.reason())),
        )?;
        list(
            f,
            "Conflicted, so Blackwall kept your version",
            self.conflicted.iter().map(|p| shown(p)),
        )
    }
}
fn list(
    f: &mut fmt::Formatter<'_>,
    title: &str,
    items: impl ExactSizeIterator<Item = String>,
) -> fmt::Result {
    let total = items.len();
    if total == 0 {
        return Ok(());
    }
    write!(f, "\n{title}:")?;
    for item in items.take(LIST_LIMIT) {
        write!(f, "\n  {item}")?;
    }
    if total > LIST_LIMIT {
        write!(f, "\n  … and {} more", total - LIST_LIMIT)?;
    }
    Ok(())
}
/// A path as one line of plain text: control and bidirectional-override characters are escaped,
/// so a file name can't add or disguise a line of the report.
fn shown(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        return "(all changes)".into();
    }
    let mut out = String::new();
    for c in path.to_string_lossy().chars() {
        let hidden = matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        if c.is_control() || hidden {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// Apply the changes staged in `upper` to `project`, given what the project's regular files were
/// at the start of the command (`start`, from the walk). Blocking: call it in `spawn_blocking`,
/// and only after the sandbox has exited.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn apply(upper: &Path, project: &Path, start: &HashMap<PathBuf, Fingerprint>) -> Report {
    apply_within(upper, project, start, MAX_ENTRIES)
}

fn apply_within(
    upper: &Path,
    project: &Path,
    start: &HashMap<PathBuf, Fingerprint>,
    limit: usize,
) -> Report {
    let nothing = |why| Report {
        refused: vec![(PathBuf::new(), why)],
        ..Report::default()
    };
    let open_root = |path: &Path| {
        fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
    };
    let (Ok(upper), Ok(project)) = (open_root(upper), open_root(project)) else {
        return nothing(Refusal::Io);
    };
    let Ok(root) = fs::fstat(&project) else {
        return nothing(Refusal::Io);
    };
    // openat2 came in Linux 5.6 and an overlay in a user namespace needs 5.11, so this is not
    // expected; without it nothing here can be done safely.
    if let Err(Errno::NOSYS) = open_dir(&project, OsStr::new(".")) {
        return nothing(Refusal::Io);
    }

    let mut plan = Planner {
        limit,
        seen: 0,
        too_many: false,
        unsupported: false,
        buffer: vec![0; 64 * 1024],
        refused: Vec::new(),
    };
    let entries = match plan.metadata(&upper) {
        Ok(Meta::Plain) => plan
            .dir(&upper, Path::new(""), 0)
            .map(|(_, entries)| entries),
        // The root is never opaque or redirected, so it is something this doesn't know.
        Ok(_) => {
            plan.unsupported = true;
            None
        }
        Err(_) => None,
    };
    let Some(entries) = entries else {
        let mut report = nothing(if plan.unsupported {
            Refusal::UnsupportedOverlay
        } else {
            Refusal::Io
        });
        report.refused.extend(plan.refused);
        return report.sorted();
    };
    let mut report = Report {
        refused: plan.refused,
        ..Report::default()
    };
    if plan.too_many || plan.unsupported {
        let why = if plan.too_many {
            Refusal::Io
        } else {
            Refusal::UnsupportedOverlay
        };
        let mut paths = Vec::new();
        every_path(&entries, Path::new(""), &mut paths);
        report
            .refused
            .extend(paths.into_iter().map(|path| (path, why)));
        return report.sorted();
    }
    let mut applier = Applier {
        upper: &upper,
        project: &project,
        root_dev: dev(&root),
        start,
        report,
    };
    applier.entries(project.as_fd(), Path::new(""), &entries, false);
    applier.report.sorted()
}

/// One planned change, from pass 1.
struct Entry {
    name: OsString,
    node: Node,
}
enum Node {
    Dir {
        mode: u32,
        opaque: bool,
        /// Every name in the staged folder, refused ones too: what an opaque folder keeps.
        names: HashSet<OsString>,
        entries: Vec<Entry>,
    },
    File {
        source: Fingerprint,
        mode: u32,
    },
    Symlink {
        target: OsString,
    },
    Whiteout,
}

fn every_path(entries: &[Entry], parent: &Path, out: &mut Vec<PathBuf>) {
    for entry in entries {
        let path = parent.join(&entry.name);
        if let Node::Dir { entries, .. } = &entry.node {
            every_path(entries, &path, out);
        }
        out.push(path);
    }
}

fn beneath() -> ResolveFlags {
    ResolveFlags::BENEATH
        | ResolveFlags::NO_SYMLINKS
        | ResolveFlags::NO_XDEV
        | ResolveFlags::NO_MAGICLINKS
}
fn open_dir(dir: impl AsFd, name: &OsStr) -> rustix::io::Result<OwnedFd> {
    fs::openat2(
        dir,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        beneath(),
    )
}
/// Opened for reading only, never blocking (a FIFO swapped in can't hang it).
fn open_file(dir: impl AsFd, path: impl AsRef<Path>) -> rustix::io::Result<OwnedFd> {
    fs::openat2(
        dir,
        path.as_ref(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
        beneath(),
    )
}
fn stat_at(dir: impl AsFd, name: &OsStr) -> rustix::io::Result<Stat> {
    fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
}
fn kind(stat: &Stat) -> FileType {
    FileType::from_raw_mode(stat.st_mode)
}
// `Stat`'s field types differ between architectures.
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
fn dev(stat: &Stat) -> u64 {
    stat.st_dev as u64
}
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
fn fingerprint(stat: &Stat) -> Fingerprint {
    let ns = |secs: i64, nsec: i64| i128::from(secs) * 1_000_000_000 + i128::from(nsec);
    Fingerprint {
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
        size: stat.st_size as u64,
        mtime_ns: ns(stat.st_mtime as i64, stat.st_mtime_nsec as i64),
        ctime_ns: ns(stat.st_ctime as i64, stat.st_ctime_nsec as i64),
    }
}
fn same_inode(a: &Stat, b: &Stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}
fn permissions(stat: &Stat) -> u32 {
    stat.st_mode & 0o777
}
fn is_instruction(name: &OsStr) -> bool {
    INSTRUCTION_FILES
        .iter()
        .any(|f| name.eq_ignore_ascii_case(f))
}
/// A folder's names, sorted, without `.` and `..`; at most `max` of them, so a huge folder can't
/// fill memory before a limit is checked.
fn names(dir: impl AsFd, max: usize) -> rustix::io::Result<Vec<OsString>> {
    let mut out = Vec::new();
    for entry in fs::Dir::read_from(dir)? {
        let name = entry?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            if out.len() == max {
                break;
            }
            out.push(OsString::from_vec(name));
        }
    }
    out.sort();
    Ok(out)
}

/// The project-relative components a symlink at `link` (project-relative) leads to, or `None`
/// when its target is absolute or empty, leaves the project, has a `..` after a name (which a
/// symlink in the project could turn into an escape) or names a protected component.
fn link_destination(link: &Path, target: &OsStr) -> Option<Vec<OsString>> {
    let target = Path::new(target);
    if target.as_os_str().is_empty() || target.is_absolute() {
        return None;
    }
    let mut out: Vec<OsString> = link
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .map(|c| c.as_os_str().to_os_string())
        .collect();
    let mut named = false;
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if !named => {
                out.pop()?;
            }
            Component::Normal(name) if !is_protected(name) => {
                named = true;
                out.push(name.to_os_string());
            }
            _ => return None,
        }
    }
    Some(out)
}

#[derive(PartialEq)]
enum Meta {
    Plain,
    Opaque,
    Capability,
    Unsupported,
}

/// Pass 1: what to do with each entry in `upper`, read-only.
struct Planner {
    limit: usize,
    seen: usize,
    too_many: bool,
    /// Overlay metadata that isn't understood was found: nothing is applied.
    unsupported: bool,
    buffer: Vec<u8>,
    refused: Vec<(PathBuf, Refusal)>,
}
impl Planner {
    fn refuse(&mut self, path: PathBuf, why: Refusal) {
        self.refused.push((path, why));
    }

    /// What a file's or folder's xattrs say about it.
    fn metadata(&mut self, fd: &OwnedFd) -> rustix::io::Result<Meta> {
        let len = match fs::flistxattr(fd, &mut self.buffer[..]) {
            Ok(len) => len,
            Err(Errno::OPNOTSUPP) => 0,
            Err(error) => return Err(error),
        };
        let (mut opaque, mut capability) = (false, false);
        for name in self.buffer[..len].split(|b| *b == 0) {
            match name {
                // What overlay writes without changing what an entry holds: origin, impure and nlink
                // bookkeeping, the upper root's uuid (Linux 6.6 and later) and saved inode flags
                // (protattr; they are not copied).
                b""
                | b"user.overlay.origin"
                | b"user.overlay.impure"
                | b"user.overlay.nlink"
                | b"user.overlay.uuid"
                | b"user.overlay.protattr" => {}
                b"security.capability" => capability = true,
                b"user.overlay.opaque" => {
                    let mut value = [0u8; 2];
                    match fs::fgetxattr(fd, OsStr::from_bytes(name), &mut value[..]) {
                        Ok(1) if value[0] == b'y' => opaque = true,
                        // `x` (a folder holding xattr whiteouts), or anything else.
                        Ok(_) | Err(Errno::RANGE) => return Ok(Meta::Unsupported),
                        Err(error) => return Err(error),
                    }
                }
                _ if name.starts_with(b"trusted.") || name.starts_with(b"user.overlay.") => {
                    return Ok(Meta::Unsupported)
                }
                _ => {}
            }
        }
        Ok(if capability {
            Meta::Capability
        } else if opaque {
            Meta::Opaque
        } else {
            Meta::Plain
        })
    }

    /// The names in the folder `dir` at `rel` and the plan for them, or `None` when it can't be
    /// listed. Stops early (with `too_many` set) past the entry limit.
    fn dir(
        &mut self,
        dir: &OwnedFd,
        rel: &Path,
        depth: usize,
    ) -> Option<(Vec<OsString>, Vec<Entry>)> {
        let names = names(dir, self.limit.saturating_sub(self.seen).saturating_add(1)).ok()?;
        let mut entries = Vec::new();
        for name in names.iter().cloned() {
            self.seen += 1;
            if self.seen > self.limit {
                self.too_many = true;
                self.refuse(rel.join(&name), Refusal::Io);
                break;
            }
            let path = rel.join(&name);
            match self.entry(dir, &path, &name, depth) {
                Ok(Some(node)) => entries.push(Entry { name, node }),
                Ok(None) => {}
                Err(why) => self.refuse(path, why),
            }
            if self.too_many {
                break;
            }
        }
        Some((names, entries))
    }

    fn entry(
        &mut self,
        dir: &OwnedFd,
        path: &Path,
        name: &OsStr,
        depth: usize,
    ) -> Result<Option<Node>, Refusal> {
        if is_protected(name) {
            return Err(Refusal::ProtectedName);
        }
        if is_instruction(name) {
            return Err(Refusal::InstructionFile);
        }
        let stat = stat_at(dir, name).map_err(|_| Refusal::Io)?;
        let file_type = kind(&stat);
        if file_type == FileType::CharacterDevice && stat.st_rdev == 0 {
            return Ok(Some(Node::Whiteout));
        }
        if !matches!(
            file_type,
            FileType::RegularFile | FileType::Directory | FileType::Symlink
        ) {
            return Err(Refusal::SpecialFile);
        }
        if stat.st_mode & SET_ID != 0 {
            return Err(Refusal::SetId);
        }
        if file_type == FileType::Symlink {
            let target = fs::readlinkat(dir, name, Vec::new()).map_err(|_| Refusal::Io)?;
            let target = OsString::from_vec(target.into_bytes());
            return match link_destination(path, &target) {
                Some(_) => Ok(Some(Node::Symlink { target })),
                None => Err(Refusal::LinkOutside),
            };
        }
        let is_dir = file_type == FileType::Directory;
        if is_dir && depth >= MAX_DEPTH {
            return Err(Refusal::Io);
        }
        let fd = if is_dir {
            open_dir(dir, name)
        } else {
            open_file(dir, name)
        }
        .map_err(|_| Refusal::Io)?;
        let opened = fs::fstat(&fd).map_err(|_| Refusal::Io)?;
        if !same_inode(&opened, &stat) || kind(&opened) != file_type {
            return Err(Refusal::Io);
        }
        let opaque = match self.metadata(&fd).map_err(|_| Refusal::Io)? {
            Meta::Plain => false,
            Meta::Opaque if is_dir => true,
            Meta::Capability => return Err(Refusal::SetId),
            Meta::Opaque | Meta::Unsupported => {
                self.unsupported = true;
                return Err(Refusal::UnsupportedOverlay);
            }
        };
        if !is_dir {
            return Ok(Some(Node::File {
                source: fingerprint(&opened),
                mode: permissions(&opened),
            }));
        }
        let (names, entries) = self.dir(&fd, path, depth + 1).ok_or(Refusal::Io)?;
        Ok(Some(Node::Dir {
            mode: permissions(&opened),
            opaque,
            names: names.into_iter().collect(),
            entries,
        }))
    }
}

/// What the project has at a path now, by the start rule.
#[derive(Clone, Copy, PartialEq)]
enum Live {
    /// Nothing, and nothing was there at the start.
    Absent,
    /// The same regular file as at the start, unchanged.
    Unchanged,
    /// Anything else: something new, changed, or gone since the start.
    Changed,
}
enum Removal {
    Removed,
    Gone,
    Kept,
}

/// Pass 2: carry out the plan.
struct Applier<'a> {
    upper: &'a OwnedFd,
    project: &'a OwnedFd,
    root_dev: u64,
    start: &'a HashMap<PathBuf, Fingerprint>,
    report: Report,
}
impl Applier<'_> {
    fn entries(&mut self, dir: BorrowedFd<'_>, rel: &Path, entries: &[Entry], opaque: bool) {
        for entry in entries {
            let path = rel.join(&entry.name);
            let name = entry.name.as_os_str();
            match &entry.node {
                Node::Dir { .. } => self.dir(dir, &path, entry, opaque),
                Node::File { source, mode } => self.file(dir, &path, name, source, *mode),
                Node::Symlink { target } => self.link(dir, &path, name, target),
                Node::Whiteout => {
                    let mut removed = Vec::new();
                    self.remove(dir, &path, name, &mut removed);
                    self.report.deleted.extend(removed);
                }
            }
        }
    }

    fn live(&self, dir: BorrowedFd<'_>, path: &Path, name: &OsStr) -> rustix::io::Result<Live> {
        match stat_at(dir, name) {
            Err(Errno::NOENT) if self.start.contains_key(path) => Ok(Live::Changed),
            Err(Errno::NOENT) => Ok(Live::Absent),
            Err(error) => Err(error),
            Ok(stat)
                if kind(&stat) == FileType::RegularFile
                    && self.start.get(path) == Some(&fingerprint(&stat)) =>
            {
                Ok(Live::Unchanged)
            }
            Ok(_) => Ok(Live::Changed),
        }
    }

    /// Open the project folder `name` in `parent` and check it is the folder `stat` describes, on
    /// the project's file system.
    fn open_project_dir(
        &self,
        parent: BorrowedFd<'_>,
        name: &OsStr,
        stat: &Stat,
    ) -> Result<OwnedFd, Refusal> {
        let fd = open_dir(parent, name).map_err(|error| match error {
            Errno::LOOP => Refusal::LinkOutside,
            _ => Refusal::Io,
        })?;
        let opened = fs::fstat(&fd).map_err(|_| Refusal::Io)?;
        if !same_inode(&opened, stat) || dev(&opened) != self.root_dev {
            return Err(Refusal::Io);
        }
        Ok(fd)
    }

    fn dir(&mut self, parent: BorrowedFd<'_>, path: &Path, entry: &Entry, opaque: bool) {
        let Node::Dir {
            mode,
            opaque: own,
            names,
            entries,
        } = &entry.node
        else {
            return;
        };
        // Below an opaque folder nothing comes from the project: every folder replaces its own.
        let opaque = opaque || *own;
        let name = entry.name.as_os_str();
        let stat = match stat_at(parent, name) {
            Ok(stat) => Some(stat),
            Err(Errno::NOENT) => None,
            Err(_) => return self.report.refuse(path, Refusal::Io),
        };
        match stat {
            Some(stat) if kind(&stat) == FileType::Directory => {
                let fd = match self.open_project_dir(parent, name, &stat) {
                    Ok(fd) => fd,
                    Err(why) => return self.report.refuse(path, why),
                };
                if opaque {
                    self.clear(fd.as_fd(), path, names);
                }
                self.entries(fd.as_fd(), path, entries, opaque);
            }
            Some(stat) if kind(&stat) == FileType::Symlink => {
                self.report.refuse(path, Refusal::LinkOutside);
            }
            // An unchanged file the command replaced with a folder.
            Some(_) if self.live(parent, path, name) == Ok(Live::Unchanged) => {
                match fs::unlinkat(parent, name, AtFlags::empty()) {
                    Ok(()) => self.new_dir(parent, path, name, *mode, entries),
                    Err(_) => self.report.refuse(path, Refusal::Io),
                }
            }
            Some(_) => self.report.conflict(path),
            None if self.start.contains_key(path) => self.report.conflict(path),
            None => self.new_dir(parent, path, name, *mode, entries),
        }
    }

    fn new_dir(
        &mut self,
        parent: BorrowedFd<'_>,
        path: &Path,
        name: &OsStr,
        mode: u32,
        entries: &[Entry],
    ) {
        match fs::mkdirat(parent, name, Mode::from_raw_mode(0o700)) {
            Ok(()) => {}
            Err(Errno::EXIST) => return self.report.conflict(path),
            Err(_) => return self.report.refuse(path, Refusal::Io),
        }
        let made = stat_at(parent, name)
            .map_err(|_| Refusal::Io)
            .and_then(|stat| {
                let fd = self.open_project_dir(parent, name, &stat)?;
                // Explicitly, whatever the umask: the owner can fill it now; its own mode comes last.
                fs::fchmod(&fd, Mode::from_raw_mode(0o700)).map_err(|_| Refusal::Io)?;
                Ok(fd)
            });
        let fd = match made {
            Ok(fd) => fd,
            Err(why) => return self.report.refuse(path, why),
        };
        // Nothing in it comes from the project, so there is nothing for opacity to hide.
        self.entries(fd.as_fd(), path, entries, false);
        match fs::fchmod(&fd, Mode::from_raw_mode(mode & 0o777)) {
            Ok(()) => self.report.applied.push(path.to_path_buf()),
            Err(_) => self.report.refuse(path, Refusal::Io),
        }
    }

    /// Remove what an opaque folder hides: the project entries whose names are not staged.
    fn clear(&mut self, dir: BorrowedFd<'_>, path: &Path, keep: &HashSet<OsString>) {
        let Ok(names) = names(dir, MAX_ENTRIES) else {
            return self.report.refuse(path, Refusal::Io);
        };
        let mut removed = Vec::new();
        for name in names.iter().filter(|name| !keep.contains(*name)) {
            self.remove(dir, &path.join(name), name, &mut removed);
        }
        self.report.deleted.extend(removed);
    }

    /// Delete `name` in `parent`, if it is an unchanged file from the start, or a folder (on the
    /// project's file system) holding only such files and folders. Anything else in it is kept
    /// and reported, and so is the folder. A folder removed whole is listed in `removed` once;
    /// from a folder that stays, what was removed from it is listed instead.
    fn remove(
        &mut self,
        parent: BorrowedFd<'_>,
        path: &Path,
        name: &OsStr,
        removed: &mut Vec<PathBuf>,
    ) -> Removal {
        if is_protected(name) {
            self.report.refuse(path, Refusal::ProtectedName);
            return Removal::Kept;
        }
        if is_instruction(name) {
            self.report.refuse(path, Refusal::InstructionFile);
            return Removal::Kept;
        }
        let stat = match stat_at(parent, name) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Removal::Gone,
            Err(_) => {
                self.report.refuse(path, Refusal::Io);
                return Removal::Kept;
            }
        };
        match kind(&stat) {
            FileType::Directory => {
                let opened = if path.components().count() > MAX_DEPTH {
                    Err(Refusal::Io)
                } else {
                    self.open_project_dir(parent, name, &stat)
                };
                let (fd, names) = match opened.and_then(|fd| match names(&fd, MAX_ENTRIES) {
                    Ok(names) => Ok((fd, names)),
                    Err(_) => Err(Refusal::Io),
                }) {
                    Ok(found) => found,
                    Err(why) => {
                        self.report.refuse(path, why);
                        return Removal::Kept;
                    }
                };
                let mut inside = Vec::new();
                let mut whole = true;
                for child in &names {
                    let child_path = path.join(child);
                    if let Removal::Kept = self.remove(fd.as_fd(), &child_path, child, &mut inside)
                    {
                        whole = false;
                    }
                }
                drop(fd);
                if whole {
                    match fs::unlinkat(parent, name, AtFlags::REMOVEDIR) {
                        Ok(()) => {
                            removed.push(path.to_path_buf());
                            return Removal::Removed;
                        }
                        Err(Errno::NOENT) => return Removal::Gone,
                        // Something new appeared in it.
                        Err(Errno::NOTEMPTY | Errno::EXIST) => self.report.conflict(path),
                        Err(_) => self.report.refuse(path, Refusal::Io),
                    }
                }
                removed.extend(inside);
                Removal::Kept
            }
            FileType::RegularFile if self.start.get(path) == Some(&fingerprint(&stat)) => {
                match fs::unlinkat(parent, name, AtFlags::empty()) {
                    Ok(()) => {
                        removed.push(path.to_path_buf());
                        Removal::Removed
                    }
                    Err(Errno::NOENT) => Removal::Gone,
                    Err(_) => {
                        self.report.refuse(path, Refusal::Io);
                        Removal::Kept
                    }
                }
            }
            _ => {
                self.report.conflict(path);
                Removal::Kept
            }
        }
    }

    fn file(
        &mut self,
        dir: BorrowedFd<'_>,
        path: &Path,
        name: &OsStr,
        source: &Fingerprint,
        mode: u32,
    ) {
        match self.live(dir, path, name) {
            Ok(Live::Changed) => self.report.conflict(path),
            Ok(before) => match self.copy_in(dir, path, name, source, mode, before) {
                Ok(true) => self.report.applied.push(path.to_path_buf()),
                Ok(false) => self.report.conflict(path),
                Err(_) => self.report.refuse(path, Refusal::Io),
            },
            Err(_) => self.report.refuse(path, Refusal::Io),
        }
    }

    /// Copy the staged file into a fresh temp file next to `name`, and rename that into place if
    /// the project still has what `before` says. `Ok(false)` is a conflict. On any failure the
    /// temp file goes and the project's entry stays.
    fn copy_in(
        &self,
        dir: BorrowedFd<'_>,
        path: &Path,
        name: &OsStr,
        source: &Fingerprint,
        mode: u32,
        before: Live,
    ) -> std::io::Result<bool> {
        let staged = open_file(self.upper, path)?;
        let stat = fs::fstat(&staged)?;
        if kind(&stat) != FileType::RegularFile || fingerprint(&stat) != *source {
            return Err(std::io::Error::other("the staged file changed"));
        }
        let temp = temp_name(name);
        let out = fs::openat2(
            dir,
            &temp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
            beneath(),
        )?;
        let commit = || -> std::io::Result<bool> {
            let mut out = std::fs::File::from(out);
            // Bounded: `io::copy` uses copy_file_range, or a small buffer.
            let copied = std::io::copy(&mut std::fs::File::from(staged), &mut out)?;
            if copied != source.size {
                return Err(std::io::Error::other("the staged file changed size"));
            }
            fs::fchmod(&out, Mode::from_raw_mode(mode & 0o777))?;
            fs::fsync(&out)?;
            drop(out);
            if self.live(dir, path, name)? != before {
                return Ok(false);
            }
            Ok(rename_into(dir, &temp, name, before)?)
        };
        let result = commit();
        if !matches!(result, Ok(true)) {
            let _ = fs::unlinkat(dir, &temp, AtFlags::empty());
        }
        result
    }

    fn link(&mut self, dir: BorrowedFd<'_>, path: &Path, name: &OsStr, target: &OsStr) {
        let before = match self.live(dir, path, name) {
            Ok(Live::Changed) => return self.report.conflict(path),
            Ok(before) => before,
            Err(_) => return self.report.refuse(path, Refusal::Io),
        };
        if !self.through_real_entries(path, target) {
            return self.report.refuse(path, Refusal::LinkOutside);
        }
        let result = if before == Live::Absent {
            // symlinkat never replaces anything.
            match fs::symlinkat(target, dir, name) {
                Ok(()) => Ok(true),
                Err(Errno::EXIST) => Ok(false),
                Err(error) => Err(error),
            }
        } else {
            let temp = temp_name(name);
            let result = fs::symlinkat(target, dir, &temp).and_then(|()| {
                if self.live(dir, path, name)? != before {
                    return Ok(false);
                }
                fs::renameat(dir, &temp, dir, name).map(|()| true)
            });
            if !matches!(result, Ok(true)) {
                let _ = fs::unlinkat(dir, &temp, AtFlags::empty());
            }
            result
        };
        match result {
            Ok(true) => self.report.applied.push(path.to_path_buf()),
            Ok(false) => self.report.conflict(path),
            Err(_) => self.report.refuse(path, Refusal::Io),
        }
    }

    /// Whether a symlink at `link` with `target` stays in the project and runs through no
    /// symlink already there: from the project root, one name at a time, it finds only real
    /// entries, and then perhaps nothing (a dangling link is fine).
    fn through_real_entries(&self, link: &Path, target: &OsStr) -> bool {
        let Some(destination) = link_destination(link, target) else {
            return false;
        };
        let mut current: Option<OwnedFd> = None;
        for name in &destination {
            let dir = current.as_ref().map_or(self.project.as_fd(), AsFd::as_fd);
            let flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            match fs::openat2(dir, name, flags, Mode::empty(), beneath()) {
                Ok(fd) => match fs::fstat(&fd) {
                    Ok(stat) if kind(&stat) != FileType::Symlink => current = Some(fd),
                    _ => return false,
                },
                Err(Errno::NOENT | Errno::NOTDIR) => return true,
                Err(_) => return false,
            }
        }
        true
    }
}

/// `.<name>.blackwall-<16 hex>`, with the name cut so the whole stays within `NAME_MAX`.
fn temp_name(name: &OsStr) -> OsString {
    let bytes = name.as_bytes();
    let mut temp = OsString::from(".");
    temp.push(OsStr::from_bytes(&bytes[..bytes.len().min(200)]));
    temp.push(format!(".blackwall-{:016x}", rand::random::<u64>()));
    temp
}

/// Rename `temp` over `name`. When nothing was there, the rename may not replace anything that
/// appeared since (`Ok(false)`); a file system without `RENAME_NOREPLACE` falls back to the
/// check just made.
fn rename_into(
    dir: BorrowedFd<'_>,
    temp: &OsStr,
    name: &OsStr,
    before: Live,
) -> rustix::io::Result<bool> {
    let renamed = if before == Live::Absent {
        match fs::renameat_with(dir, temp, dir, name, RenameFlags::NOREPLACE) {
            Err(Errno::INVAL) => fs::renameat(dir, temp, dir, name),
            other => other,
        }
    } else {
        fs::renameat(dir, temp, dir, name)
    };
    match renamed {
        Ok(()) => Ok(true),
        Err(Errno::EXIST) => Ok(false),
        Err(error) => Err(error),
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::walk::walk_with_files;
    use super::*;
    use rustix::fs::{FileType, Mode, XattrFlags};
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    use std::time::Duration;
    use Refusal::*;

    /// An `upper`, a `project` and an `outside` folder side by side, on tmpfs where it is writable
    /// (a container's overlay `/tmp` refuses whiteout devices), removed on drop.
    struct Tree {
        root: PathBuf,
        upper: PathBuf,
        project: PathBuf,
        outside: PathBuf,
    }
    impl Tree {
        fn new() -> Self {
            let name = format!("blackwall-apply-{:016x}", rand::random::<u64>());
            let shm = Path::new("/dev/shm").join(&name);
            let root = if std::fs::create_dir(&shm).is_ok() {
                shm
            } else {
                let root = std::env::temp_dir().join(&name);
                std::fs::create_dir(&root).unwrap();
                root
            };
            let root = std::fs::canonicalize(root).unwrap();
            let tree = Self {
                upper: root.join("upper"),
                project: root.join("project"),
                outside: root.join("outside"),
                root,
            };
            for dir in [&tree.upper, &tree.project, &tree.outside] {
                std::fs::create_dir(dir).unwrap();
            }
            tree
        }
        fn write(base: &Path, relative: &str, content: &str) -> PathBuf {
            let path = base.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            path
        }
        fn up(&self, relative: &str, content: &str) -> PathBuf {
            Self::write(&self.upper, relative, content)
        }
        fn proj(&self, relative: &str, content: &str) -> PathBuf {
            Self::write(&self.project, relative, content)
        }
        fn up_dir(&self, relative: &str) -> PathBuf {
            let path = self.upper.join(relative);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
        fn start(&self) -> HashMap<PathBuf, Fingerprint> {
            walk_with_files(&self.project, MAX_ENTRIES).unwrap().files
        }
        fn apply(&self, start: &HashMap<PathBuf, Fingerprint>) -> Report {
            apply(&self.upper, &self.project, start)
        }
        fn read(&self, relative: &str) -> String {
            std::fs::read_to_string(self.project.join(relative)).unwrap()
        }
        fn has(&self, relative: &str) -> bool {
            std::fs::symlink_metadata(self.project.join(relative)).is_ok()
        }
        /// Every path in the project, sorted.
        fn listing(&self) -> Vec<String> {
            fn visit(base: &Path, dir: &Path, out: &mut Vec<String>) {
                for entry in std::fs::read_dir(dir).unwrap() {
                    let path = entry.unwrap().path();
                    out.push(path.strip_prefix(base).unwrap().display().to_string());
                    if std::fs::symlink_metadata(&path).unwrap().is_dir() {
                        visit(base, &path, out);
                    }
                }
            }
            let mut out = Vec::new();
            visit(&self.project, &self.project, &mut out);
            out.sort();
            out
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// An overlay whiteout (a 0/0 character device), or `false` when this file system or user
    /// can't make one, so the test can skip.
    fn whiteout(path: &Path) -> bool {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        match rustix::fs::mknodat(
            rustix::fs::CWD,
            path,
            FileType::CharacterDevice,
            Mode::empty(),
            0,
        ) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("skipped: can't make a whiteout here ({error})");
                false
            }
        }
    }
    /// Set an xattr, or `false` when this file system or user can't, so the test can skip.
    fn xattr(path: &Path, name: &str, value: &[u8]) -> bool {
        match rustix::fs::lsetxattr(path, name, value, XattrFlags::empty()) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("skipped: can't set {name} here ({error})");
                false
            }
        }
    }
    fn refusals(report: &Report) -> Vec<(&str, Refusal)> {
        report
            .refused
            .iter()
            .map(|(path, why)| (path.to_str().unwrap(), *why))
            .collect()
    }
    fn paths(list: &[PathBuf]) -> Vec<&str> {
        list.iter().map(|path| path.to_str().unwrap()).collect()
    }
    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }
    fn ino(path: &Path) -> u64 {
        std::fs::symlink_metadata(path).unwrap().ino()
    }
    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn apply_writes_new_and_changed_files_and_reports_them() {
        let t = Tree::new();
        t.proj("changed.txt", "old");
        t.proj("same.txt", "same");
        t.proj("run.sh", "x");
        let lib = t.proj("lib/a.rs", "a").parent().unwrap().to_path_buf();
        chmod(&lib, 0o700);
        let start = t.start();
        t.up("changed.txt", "new");
        t.up("new.txt", "fresh");
        t.up("n/m/deep.txt", "deep");
        chmod(&t.upper.join("n"), 0o750);
        t.up_dir("empty");
        chmod(&t.up("run.sh", "y"), 0o755);
        chmod(t.up("lib/b.rs", "b").parent().unwrap(), 0o755);
        // What a real overlay (Linux 6.8, userxattr) leaves in upper; none of it stops the apply.
        let _ = xattr(&t.upper, "user.overlay.uuid", b"0123456789abcdef")
            && xattr(&t.upper, "user.overlay.impure", b"y")
            && xattr(
                &t.upper.join("changed.txt"),
                "user.overlay.origin",
                b"\0\xfb!",
            );

        let r = t.apply(&start);
        assert_eq!(
            paths(&r.applied),
            [
                "changed.txt",
                "empty",
                "lib/b.rs",
                "n",
                "n/m",
                "n/m/deep.txt",
                "new.txt",
                "run.sh"
            ]
        );
        assert_eq!(
            (r.refused.len(), r.conflicted.len(), r.deleted.len()),
            (0, 0, 0),
            "{r:?}"
        );
        assert_eq!(t.read("changed.txt"), "new");
        assert_eq!(t.read("same.txt"), "same");
        assert_eq!(t.read("new.txt"), "fresh");
        assert_eq!(t.read("n/m/deep.txt"), "deep");
        assert_eq!(t.read("lib/b.rs"), "b");
        assert_eq!(mode(&t.project.join("run.sh")), 0o755);
        // New folders get their staged mode; an existing folder keeps its own.
        assert_eq!(mode(&t.project.join("n")), 0o750);
        assert_eq!(mode(&lib), 0o700);
        assert!(
            t.listing().iter().all(|p| !p.contains(".blackwall-")),
            "temp left: {:?}",
            t.listing()
        );
    }

    #[test]
    fn apply_refuses_protected_names_and_instruction_files_at_any_depth() {
        let t = Tree::new();
        t.up(".git/config", "[core]");
        t.up("a/b/.claude/x", "x");
        t.up("a/b/fine.txt", "fine");
        t.up(".agents/moved.rs", "moved"); // `mv src .agents`
        t.up("AGENTS.md", "obey");
        t.up("docs/claude.md", "obey");
        t.up("docs/ok.md", "ok");

        let r = t.apply(&HashMap::new());
        assert_eq!(
            refusals(&r),
            [
                (".agents", ProtectedName),
                (".git", ProtectedName),
                ("AGENTS.md", InstructionFile),
                ("a/b/.claude", ProtectedName),
                ("docs/claude.md", InstructionFile),
            ]
        );
        for refused in [
            ".git",
            ".agents",
            "AGENTS.md",
            "a/b/.claude",
            "docs/claude.md",
        ] {
            assert!(!t.has(refused), "{refused}");
        }
        assert_eq!(t.read("a/b/fine.txt"), "fine");
        assert_eq!(t.read("docs/ok.md"), "ok");
    }

    #[test]
    fn apply_refuses_case_variants_symlinked_parents_and_links_hard_links_as_copies() {
        let t = Tree::new();
        t.up(".Git/config", "[core]");
        t.up("a/f", "through the link");
        symlink("../outside", t.project.join("a")).unwrap();
        let h1 = t.up("h1", "linked");
        std::fs::hard_link(&h1, t.upper.join("h2")).unwrap();
        let start = t.start();

        let r = t.apply(&start);
        assert_eq!(refusals(&r), [(".Git", ProtectedName), ("a", LinkOutside)]);
        assert!(!t.outside.join("f").exists());
        assert!(!t.has(".Git"));
        assert_eq!(
            (t.read("h1"), t.read("h2")),
            ("linked".into(), "linked".into())
        );
        assert_ne!(ino(&t.project.join("h1")), ino(&t.project.join("h2")));
        assert_eq!(paths(&r.applied), ["h1", "h2"]);
    }

    #[test]
    fn apply_refuses_escaping_symlinks_setid_and_special_files() {
        let t = Tree::new();
        let link = |target: &str, name: &str| symlink(target, t.upper.join(name)).unwrap();
        link("/etc/passwd", "abs");
        link("../../x", "up_escape");
        link("lib/../../etc", "sneaky");
        link(".git/config", "to_git");
        link("ext/passwd", "via_owner");
        t.up_dir("d");
        link("../target.txt", "d/ok");
        t.up("target.txt", "t");
        symlink("/etc", t.project.join("ext")).unwrap(); // the owner's own link
        chmod(&t.up("suid", "x"), 0o4755);
        let sgid = t.up("sgid/inner", "x").parent().unwrap().to_path_buf();
        chmod(&sgid, 0o2755);
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            t.upper.join("fifo"),
            Mode::from_raw_mode(0o644),
        )
        .unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(t.upper.join("sock")).unwrap();
        let mut expected = vec![
            ("abs", LinkOutside),
            ("fifo", SpecialFile),
            ("sgid", SetId),
            ("sneaky", LinkOutside),
            ("sock", SpecialFile),
            ("suid", SetId),
            ("to_git", LinkOutside),
            ("up_escape", LinkOutside),
            ("via_owner", LinkOutside),
        ];
        // /dev/null's numbers; only where this user may make devices.
        let null = rustix::fs::mknodat(
            rustix::fs::CWD,
            t.upper.join("null"),
            FileType::CharacterDevice,
            Mode::from_raw_mode(0o666),
            rustix::fs::makedev(1, 3),
        );
        match null {
            Ok(()) => expected.push(("null", SpecialFile)),
            Err(error) => eprintln!("skipped the device case: {error}"),
        }
        // A file capability (VFS_CAP_REVISION_2 granting CAP_NET_RAW); needs CAP_SETFCAP.
        let cap = t.up("cap", "x");
        let mut value = [0u8; 20];
        value[..4].copy_from_slice(&0x0200_0000u32.to_le_bytes());
        value[4..8].copy_from_slice(&(1u32 << 13).to_le_bytes());
        if xattr(&cap, "security.capability", &value) {
            expected.push(("cap", SetId));
        } else {
            std::fs::remove_file(&cap).unwrap();
        }
        expected.sort_by_key(|(path, _)| *path);
        let start = t.start();

        let r = t.apply(&start);
        assert_eq!(refusals(&r), expected);
        assert_eq!(paths(&r.applied), ["d", "d/ok", "target.txt"]);
        assert_eq!(
            std::fs::read_link(t.project.join("d/ok")).unwrap(),
            Path::new("../target.txt")
        );
        assert_eq!(t.listing(), ["d", "d/ok", "ext", "target.txt"]);
    }

    #[test]
    fn apply_keeps_real_file_on_conflict() {
        let t = Tree::new();
        t.proj("f", "mine");
        let start = t.start();
        t.proj("f", "owner edit!");
        t.up("f", "command");
        let r = t.apply(&start);
        assert_eq!(r.conflicted, [PathBuf::from("f")]);
        assert_eq!(t.read("f"), "owner edit!");
        assert!(r.applied.is_empty());
    }

    #[test]
    fn apply_keeps_anything_new_at_a_path_it_would_create() {
        let t = Tree::new();
        let start = t.start();
        // After the start: a file, a folder and a symlink where the command made files.
        t.proj("g", "owner");
        std::fs::create_dir(t.project.join("dir_vs_file")).unwrap();
        symlink("g", t.project.join("lnk")).unwrap();
        for name in ["g", "dir_vs_file", "lnk"] {
            t.up(name, "command");
        }
        symlink("g", t.upper.join("new_link")).unwrap();
        t.proj("new_link", "owner");
        let r = t.apply(&start);
        assert_eq!(
            paths(&r.conflicted),
            ["dir_vs_file", "g", "lnk", "new_link"]
        );
        assert_eq!(t.read("g"), "owner");
        assert_eq!(t.read("new_link"), "owner");
        assert!(t.project.join("dir_vs_file").is_dir());
        assert_eq!(
            std::fs::read_link(t.project.join("lnk")).unwrap(),
            Path::new("g")
        );
    }

    #[test]
    fn apply_counts_a_restored_size_and_mtime_as_a_conflict() {
        let t = Tree::new();
        let f = t.proj("f", "aaaa");
        let mtime = std::fs::metadata(&f).unwrap().modified().unwrap();
        // Past the kernel's coarse timestamp tick, so the edit gets a new ctime.
        std::thread::sleep(Duration::from_millis(50));
        let start = t.start();
        std::fs::write(&f, "bbbb").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(std::fs::metadata(&f).unwrap().modified().unwrap(), mtime);
        t.up("f", "command");
        let r = t.apply(&start);
        assert_eq!(paths(&r.conflicted), ["f"]);
        assert_eq!(t.read("f"), "bbbb");
    }

    #[test]
    fn apply_breaks_project_hard_links_instead_of_writing_through_them() {
        let t = Tree::new();
        let a = t.proj("a", "old");
        std::fs::hard_link(&a, t.project.join("b")).unwrap();
        let start = t.start();
        t.up("a", "new");
        let r = t.apply(&start);
        assert_eq!(paths(&r.applied), ["a"]);
        assert_eq!((t.read("a"), t.read("b")), ("new".into(), "old".into()));
    }

    #[test]
    fn apply_copies_no_xattrs() {
        let t = Tree::new();
        let f = t.up("f", "x");
        if !xattr(&f, "user.blackwall-test", b"1") {
            return;
        }
        let r = t.apply(&HashMap::new());
        assert_eq!(paths(&r.applied), ["f"]);
        let mut list = [0u8; 256];
        let len = rustix::fs::listxattr(t.project.join("f"), &mut list[..]).unwrap();
        assert!(
            !list[..len].windows(14).any(|w| w == b"user.blackwall"),
            "{:?}",
            String::from_utf8_lossy(&list[..len])
        );
    }

    #[test]
    fn apply_whiteouts_delete_only_unchanged_entries() {
        let t = Tree::new();
        t.proj("gone.txt", "g");
        t.proj("edited.txt", "e");
        t.proj("tree/a", "a");
        t.proj("tree/b/c", "c");
        t.proj("tree2/a", "a");
        let start = t.start();
        t.proj("edited.txt", "owner edit");
        t.proj("tree2/new_outside", "owner");
        for name in ["gone.txt", "edited.txt", "tree", "tree2", "never_existed"] {
            if !whiteout(&t.upper.join(name)) {
                return;
            }
        }
        let r = t.apply(&start);
        assert_eq!(paths(&r.deleted), ["gone.txt", "tree", "tree2/a"]);
        assert_eq!(paths(&r.conflicted), ["edited.txt", "tree2/new_outside"]);
        assert!(r.refused.is_empty() && r.applied.is_empty(), "{r:?}");
        assert_eq!(t.listing(), ["edited.txt", "tree2", "tree2/new_outside"]);
    }

    #[test]
    fn apply_opaque_dir_replaces_contents_but_never_deletes_protected_descendants() {
        let t = Tree::new();
        for file in [
            "d/old",
            "d/.git/config",
            "d/sub/.ssh/key",
            "d/sub/gone.txt",
            "d/AGENTS.md",
            "d/keep",
            "d/whole/x",
            "d/whole/y/z",
        ] {
            t.proj(file, "before");
        }
        let start = t.start();
        t.proj("d/outside_new", "owner");
        t.up("d/new", "new");
        t.up("d/keep", "kept by the command");
        if !xattr(&t.upper.join("d"), "user.overlay.opaque", b"y") {
            return;
        }
        let r = t.apply(&start);
        assert_eq!(paths(&r.deleted), ["d/old", "d/sub/gone.txt", "d/whole"]);
        assert_eq!(
            refusals(&r),
            [
                ("d/.git", ProtectedName),
                ("d/AGENTS.md", InstructionFile),
                ("d/sub/.ssh", ProtectedName)
            ]
        );
        assert_eq!(paths(&r.conflicted), ["d/outside_new"]);
        assert_eq!(paths(&r.applied), ["d/keep", "d/new"]);
        assert_eq!(
            t.listing(),
            [
                "d",
                "d/.git",
                "d/.git/config",
                "d/AGENTS.md",
                "d/keep",
                "d/new",
                "d/outside_new",
                "d/sub",
                "d/sub/.ssh",
                "d/sub/.ssh/key"
            ]
        );
        assert_eq!(t.read("d/keep"), "kept by the command");
    }

    #[test]
    fn apply_changes_nothing_when_the_entry_limit_is_passed_last() {
        let t = Tree::new();
        t.proj("keep", "k");
        let start = t.start();
        for name in ["a", "b", "c", "d"] {
            t.up(name, name);
        }
        t.up("keep", "changed"); // the fifth entry, and the limit is four
        let r = apply_within(&t.upper, &t.project, &start, 4);
        assert!(r.applied.is_empty() && r.deleted.is_empty(), "{r:?}");
        assert!(!r.refused.is_empty() && r.refused.iter().all(|(_, why)| *why == Io));
        assert_eq!(t.listing(), ["keep"]);
        assert_eq!(t.read("keep"), "k");
    }

    #[test]
    fn apply_changes_nothing_when_overlay_metadata_comes_last() {
        for (name, is_dir, key) in [
            ("zz", true, "user.overlay.redirect"),
            ("zz", false, "user.overlay.metacopy"),
            ("zz", true, "user.overlay.opaque"), // `x` is not an opaque folder
        ] {
            let t = Tree::new();
            t.proj("keep", "k");
            let start = t.start();
            t.up("a", "a");
            t.up("keep", "changed");
            let path = if is_dir {
                t.up_dir(name)
            } else {
                t.up(name, "z")
            };
            let value: &[u8] = if key.ends_with("opaque") {
                b"x"
            } else {
                b"/elsewhere"
            };
            if !xattr(&path, key, value) {
                return;
            }
            let r = t.apply(&start);
            assert!(r.applied.is_empty() && r.deleted.is_empty(), "{key}: {r:?}");
            assert_eq!(
                refusals(&r),
                [
                    ("a", UnsupportedOverlay),
                    ("keep", UnsupportedOverlay),
                    ("zz", UnsupportedOverlay)
                ],
                "{key}"
            );
            assert_eq!(t.listing(), ["keep"], "{key}");
            assert_eq!(t.read("keep"), "k");
        }
    }

    #[test]
    fn report_display_is_capped_and_plain() {
        let report = Report {
            applied: (0..250)
                .map(|i| PathBuf::from(format!("f{i:03}")))
                .collect(),
            refused: vec![
                (PathBuf::from(".git"), ProtectedName),
                (
                    PathBuf::from("evil\nWorkspace changes: 0 applied"),
                    LinkOutside,
                ),
            ],
            conflicted: vec![PathBuf::from("mine.txt")],
            deleted: vec![],
        };
        let text = report.to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "Workspace changes: 250 applied, 0 deleted, 2 refused, 1 conflicted"
        );
        assert_eq!(lines[1], "Applied:");
        assert_eq!(lines[2], "  f000");
        assert_eq!(lines[201], "  f199");
        assert_eq!(lines[202], "  … and 50 more");
        assert!(!text.contains("Deleted"));
        assert!(text.contains("  .git (a protected name such as .git or .ssh)"));
        // A name can't fake a line of the report.
        assert!(text.contains(r"evil\nWorkspace changes: 0 applied (a link"));
        assert_eq!(text.matches("Workspace changes:").count(), 2);
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.starts_with("Workspace changes:"))
                .count(),
            1
        );
        assert!(text.contains("\n  mine.txt"));
        assert!(text.contains("kept your version"));
    }
}
