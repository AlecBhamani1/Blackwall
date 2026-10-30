//! Copy what a Workspace-write command changed (the overlay's upper folder) back into the
//! project, through descriptor-relative, no-follow operations only.
//!
//! It runs after the sandbox has fully exited, so nothing writes to `upper` any more; the only
//! writers left are the owner's own processes in the project. The project folder must still be
//! the one the start walk saw (its device and inode), or nothing is applied.
//!
//! 1. **Plan.** Walk `upper` read-only. The xattrs of every file and folder the walk reaches are
//!    read before anything else about it, so overlay metadata this does not understand is found
//!    even on an entry refused for another reason (a protected name, say): redirect, metacopy,
//!    `trusted.*`, any `user.overlay.*` but origin, impure, nlink, uuid, protattr and opaque `y`,
//!    or a list too long to read. A refused folder's contents are not read at all; that is safe
//!    because any refusal holds back every delete (step 3). Unknown metadata, or more than
//!    [`MAX_ENTRIES`] entries, and nothing is applied at all. Otherwise each entry is refused (a protected name,
//!    an instruction file, a set-id file or file capability, a device, pipe or socket, a symlink
//!    that leads out or could stand in for an instruction file's folder) or planned.
//! 2. **Apply.** Folders, files and symlinks first. Every project folder is opened one name at a
//!    time from its already-open parent with `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
//!    RESOLVE_NO_XDEV)`, so no symlink and no other mount in the project is followed. A file is
//!    copied into a fresh temp file (`.<name>.blackwall-<hex>`) next to its target, with no
//!    xattrs, only the `0o777` mode bits (less the umask for a new file) and the staged access
//!    and modification times, synced, and then committed (below). The staged inode itself never
//!    enters the project. A new folder gets its staged `0o777` bits less the umask (no set-id);
//!    an existing folder keeps its mode.
//! 3. **Delete.** Whiteouts and what opaque folders hide, last, and only when nothing above was
//!    refused or conflicted: a refused `.agents` that a command moved `src` to comes with a
//!    whiteout at `src`, and deleting `src` would lose it. Held-back deletes are reported.
//!
//! What the owner changed since the command started wins. An existing entry may be replaced or
//! deleted only if it is the regular file the start walk recorded, unchanged (`start`), or, for an
//! entry the walk did not record (below a skipped folder such as `node_modules`, a symlink, a
//! folder), if neither it nor any folder between it and the project root has a ctime at or after
//! `run_started` less [`CLOCK_MARGIN_NS`]. ctime can't be set from userspace; the margin covers the
//! coarse clock the kernel stamps it from. A clock stepped backwards, or an NFS or FUSE server's
//! own clock, can defeat it; that is accepted. A folder changed during the run is kept (the
//! unchanged recorded files in it may still go). Anything else is a conflict: the project's
//! version stays and the path is reported.
//!
//! Commits are inode-conditional, not check-then-act:
//! - A new entry goes in with `RENAME_NOREPLACE` (or `mkdirat`/`symlinkat`, which never replace).
//! - A replacement is swapped in with `RENAME_EXCHANGE`. The displaced entry, now at the temp
//!   name, must be the one just checked (device, inode, type, size, mtime; a rename moves its
//!   ctime) or it is swapped back and reported as a conflict.
//! - A delete first renames the entry aside (`RENAME_NOREPLACE`), checks it the same way (a
//!   folder: the inode of the descriptor its contents were deleted through), then unlinks or
//!   rmdirs it. If it isn't what was checked, or removing it fails, it is renamed back.
//! - Temp names are fresh and random; a taken one is never touched (another is tried, up to 8).
//!   Only a temp entry this apply made, by its recorded inode, is ever removed.
//! - A file system without these flags gets nothing replaced or deleted; there is no fallback.
//!
//! What is left: a writer that changes a file in place, between its check and its swap, within
//! one mtime tick; and the moment of a swap back or a move back, when the owner's entry briefly
//! sits at the temp name. If putting it back fails, both paths are reported as conflicts, and the
//! report says the original name may hold Blackwall's staged copy. There is no journal: the apply as a
//! whole is not atomic, and a crash part way leaves what was done so far, perhaps with a temp
//! entry. A folder an outside process moves while it is pinned takes its changes with it.
//!
//! Known limits, accepted on purpose: user xattrs exist only on files and folders, so those are
//! the only entries whose overlay metadata is read, and `trusted.*` is visible only to a
//! privileged process. A symlink's target must be relative, stay inside the project lexically,
//! use `..` only at its start, name no protected component and run through no symlink that
//! already exists. A folder is never replaced by a file.

use super::walk::{is_protected, Fingerprint, INSTRUCTION_FILES, MAX_ENTRIES};
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, RenameFlags, ResolveFlags, Stat};
use rustix::io::Errno;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The deepest folder nesting applied or deleted. Each level holds a descriptor open.
const MAX_DEPTH: usize = 256;
/// How many paths each list in the report shows (and keeps).
const LIST_LIMIT: usize = 50;
/// How many characters of each path the report shows; a longer one loses its middle.
const SHOWN_CHARS: usize = 160;
const SET_ID: u32 = 0o6000;
/// How long before `run_started` a ctime already counts as a change during the run (2 s): the
/// kernel stamps ctime from a coarse clock that can lag the one `run_started` was read from.
const CLOCK_MARGIN_NS: i128 = 2_000_000_000;

/// Why a staged change was not applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    ProtectedName,
    InstructionFile,
    LinkOutside,
    SpecialFile,
    /// A set-user-ID or set-group-ID file, or a file capability.
    SetId,
    UnsupportedOverlay,
    /// `openat2` is missing, so nothing can be opened safely.
    Unavailable,
    /// The file system can't replace or delete atomically (no `RENAME_NOREPLACE` or
    /// `RENAME_EXCHANGE`).
    NoSafeReplace,
    /// More staged entries than [`MAX_ENTRIES`].
    TooMany,
    /// The project folder is not the one the command started with.
    ProjectChanged,
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
            Self::Unavailable => "staging unavailable: this system can't open files safely",
            Self::NoSafeReplace => "this file system can't replace files safely",
            Self::TooMany => "too many changes to check safely",
            Self::ProjectChanged => "the project folder changed while the command ran",
            Self::Io => "couldn't be read or written safely",
        }
    }
}

/// One of the report's lists: how many entries it has, and the first [`LIST_LIMIT`] of them
/// (sorted by [`Report::sorted`]). The rest are only counted, so a huge staged tree can't fill
/// memory with paths.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Capped<T> {
    pub total: usize,
    pub first: Vec<T>,
}
impl<T> Default for Capped<T> {
    fn default() -> Self {
        Self {
            total: 0,
            first: Vec::new(),
        }
    }
}
impl<T> FromIterator<T> for Capped<T> {
    fn from_iter<I: IntoIterator<Item = T>>(items: I) -> Self {
        let mut capped = Self::default();
        capped.extend(items);
        capped
    }
}
impl<T> Capped<T> {
    fn push(&mut self, item: T) {
        self.total += 1;
        if !self.full() {
            self.first.push(item);
        }
    }
    fn extend(&mut self, items: impl IntoIterator<Item = T>) {
        for item in items {
            self.push(item);
        }
    }
    /// Add `other`'s entries, the ones it only counted too.
    fn append(&mut self, other: Self) {
        let counted = other.total - other.first.len();
        self.extend(other.first);
        self.total += counted;
    }
    fn full(&self) -> bool {
        self.first.len() >= LIST_LIMIT
    }
    pub fn len(&self) -> usize {
        self.total
    }
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }
}

/// What [`apply`] did, each list sorted by path. An empty path stands for the whole apply.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Report {
    pub applied: Capped<PathBuf>,
    pub refused: Capped<(PathBuf, Refusal)>,
    pub conflicted: Capped<PathBuf>,
    pub deleted: Capped<PathBuf>,
    /// Deletes not done because something else was refused or conflicted.
    pub held_back: Capped<PathBuf>,
    /// A conflicted path is a temp name of Blackwall's (see [`STRANDED`]).
    stranded: bool,
}
impl Report {
    fn refuse(&mut self, path: &Path, why: Refusal) {
        self.refused.push((path.to_path_buf(), why));
    }
    fn conflict(&mut self, path: &Path) {
        self.stranded |= path
            .file_name()
            .is_some_and(|name| name.as_bytes().windows(11).any(|w| w == b".blackwall-"));
        self.conflicted.push(path.to_path_buf());
    }
    fn sorted(mut self) -> Self {
        self.applied.first.sort();
        self.deleted.first.sort();
        self.conflicted.first.sort();
        self.held_back.first.sort();
        self.refused.first.sort_by(|a, b| a.0.cmp(&b.0));
        self
    }
}

const HELD_BACK: &str = "held back (not deleted because other changes were refused or conflicted)";

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
        if !self.held_back.is_empty() {
            write!(f, ", {} {HELD_BACK}", self.held_back.len())?;
        }
        // What the owner may need to act on comes first: a long report is cut at its end.
        list(f, "Refused", &self.refused, |(p, why)| {
            format!("{} ({})", shown(p), why.reason())
        })?;
        list(
            f,
            "Conflicted, so Blackwall kept your version",
            &self.conflicted,
            |p| shown(p),
        )?;
        if self.stranded {
            write!(f, "\n{STRANDED}")?;
        }
        list(f, "Held back", &self.held_back, |p| shown(p))?;
        list(f, "Applied", &self.applied, |p| shown(p))?;
        list(f, "Deleted", &self.deleted, |p| shown(p))
    }
}
const STRANDED: &str =
    "A conflicted name with .blackwall- in it is an entry Blackwall moved aside \
     and couldn't put back. Check it and the entry at its original name, which may be \
     Blackwall's staged copy.";
fn list<T>(
    f: &mut fmt::Formatter<'_>,
    title: &str,
    items: &Capped<T>,
    show: impl Fn(&T) -> String,
) -> fmt::Result {
    if items.is_empty() {
        return Ok(());
    }
    write!(f, "\n{title}:")?;
    for item in &items.first {
        write!(f, "\n  {}", show(item))?;
    }
    if items.total > items.first.len() {
        write!(f, "\n  … and {} more", items.total - items.first.len())?;
    }
    Ok(())
}
/// A path as one line of plain text, at most [`SHOWN_CHARS`] characters (a longer one loses its
/// middle, shown as `…`). Control characters, line and paragraph separators and every format
/// character (bidirectional controls, invisible ones, tags) are escaped as `\u{..}`, bytes that
/// aren't UTF-8 as `\xNN` and a backslash as `\\`, so a file name can't add, hide or disguise a
/// line of the report, and two different names never look the same (unless cut).
fn shown(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        return "(all changes)".into();
    }
    let mut out = String::new();
    for chunk in path.as_os_str().as_bytes().utf8_chunks() {
        for c in chunk.valid().chars() {
            if c == '\\' {
                out.push_str("\\\\");
            } else if c.is_control() || is_format(c) || matches!(c, '\u{2028}' | '\u{2029}') {
                out.extend(c.escape_unicode());
            } else {
                out.push(c);
            }
        }
        for byte in chunk.invalid() {
            out.push_str(&format!("\\x{byte:02x}"));
        }
    }
    let count = out.chars().count();
    if count <= SHOWN_CHARS {
        return out;
    }
    let head = (SHOWN_CHARS - 1) / 2;
    let tail = SHOWN_CHARS - 1 - head;
    let mut clipped: String = out.chars().take(head).collect();
    clipped.push('…');
    clipped.extend(out.chars().skip(count - tail));
    clipped
}
/// Unicode general category Cf, format characters (Unicode 15.1): invisible, and able to
/// reorder or hide the text around them.
fn is_format(c: char) -> bool {
    matches!(
        c,
        '\u{ad}'
            | '\u{600}'..='\u{605}'
            | '\u{61c}'
            | '\u{6dd}'
            | '\u{70f}'
            | '\u{890}'..='\u{891}'
            | '\u{8e2}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

/// Apply the changes staged in `upper` to `project`. Blocking: call it in `spawn_blocking`, and
/// only after the sandbox has exited.
///
/// - `expected_root`: the project folder's (device, inode) from the start walk (`Walk::root`).
/// - `start`: the start walk's fingerprints of the project's regular files (`Walk::files`).
/// - `run_started`: read before the sandbox started; what changed since is the owner's.
/// - `instruction_names`: the owner's configured extra instruction files (relative, `/`-separated,
///   such as `.github/copilot-instructions.md`), refused on top of [`INSTRUCTION_FILES`].
///
/// New files and folders get their staged `0o777` bits less the process umask, read now, as a
/// direct write would.
pub(crate) fn apply(
    upper: &Path,
    project: &Path,
    expected_root: (u64, u64),
    start: &HashMap<PathBuf, Fingerprint>,
    run_started: SystemTime,
    instruction_names: &[String],
) -> Report {
    let rules = Rules {
        start,
        since: to_ns(run_started) - CLOCK_MARGIN_NS,
        names: Names::new(instruction_names),
        limit: MAX_ENTRIES,
        umask: process_umask(),
    };
    apply_within(upper, project, expected_root, &rules)
}

/// What decides each entry, besides the trees themselves.
struct Rules<'a> {
    start: &'a HashMap<PathBuf, Fingerprint>,
    /// The ctime (ns from the epoch) from which an entry counts as changed during the run.
    since: i128,
    names: Names,
    limit: usize,
    /// Taken off a new file's or folder's mode.
    umask: u32,
}

/// The process umask, from `/proc/self/status` (Linux 4.7 and later): `umask(2)` can only read
/// it by changing it, for every thread, for a moment. Unknown counts as `0o077`.
fn process_umask() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| umask_in(&status))
        .unwrap_or(0o077)
}
fn umask_in(status: &str) -> Option<u32> {
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("Umask:"))?;
    u32::from_str_radix(value.trim(), 8)
        .ok()
        .map(|mask| mask & 0o777)
}

fn to_ns(time: SystemTime) -> i128 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i128::try_from(after.as_nanos()).unwrap_or(i128::MAX),
        Err(before) => -i128::try_from(before.duration().as_nanos()).unwrap_or(i128::MAX),
    }
}

fn apply_within(
    upper: &Path,
    project: &Path,
    expected_root: (u64, u64),
    rules: &Rules<'_>,
) -> Report {
    let nothing = |why| Report {
        refused: [(PathBuf::new(), why)].into_iter().collect(),
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
    if (dev(&root), ino(&root)) != expected_root {
        return nothing(Refusal::ProjectChanged);
    }
    // openat2 came in Linux 5.6 and an overlay in a user namespace needs 5.11, so this is not
    // expected; without it nothing here can be done safely.
    if let Err(Errno::NOSYS) = open_dir(&project, OsStr::new(".")) {
        return nothing(Refusal::Unavailable);
    }

    let mut plan = Planner {
        rules,
        seen: 0,
        too_many: false,
        unsupported: false,
        buffer: vec![0; 64 * 1024],
        refused: Capped::default(),
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
        report.refused.append(plan.refused);
        return report.sorted();
    };
    let mut report = Report {
        refused: plan.refused,
        ..Report::default()
    };
    if plan.too_many || plan.unsupported {
        let why = if plan.too_many {
            Refusal::TooMany
        } else {
            Refusal::UnsupportedOverlay
        };
        refuse_all(&entries, Path::new(""), why, &mut report.refused);
        return report.sorted();
    }
    let mut applier = Applier {
        upper: &upper,
        project: &project,
        root_dev: dev(&root),
        rules,
        report,
    };
    let pin = Pin {
        dev: dev(&root),
        ino: ino(&root),
        fresh: false,
    };
    let mut deletes = Vec::new();
    applier.entries(
        project.as_fd(),
        Path::new(""),
        &entries,
        pin,
        false,
        &mut deletes,
    );
    applier.deletes(deletes);
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

/// Refuse every planned entry for `why`: only the paths `out` keeps are made, the rest counted.
fn refuse_all(
    entries: &[Entry],
    parent: &Path,
    why: Refusal,
    out: &mut Capped<(PathBuf, Refusal)>,
) {
    for entry in entries {
        if out.full() {
            out.total += count(entry);
            continue;
        }
        let path = parent.join(&entry.name);
        if let Node::Dir { entries, .. } = &entry.node {
            refuse_all(entries, &path, why, out);
        }
        out.push((path, why));
    }
}
/// `entry` and every entry planned below it.
fn count(entry: &Entry) -> usize {
    match &entry.node {
        Node::Dir { entries, .. } => 1 + entries.iter().map(count).sum::<usize>(),
        _ => 1,
    }
}

/// The instruction-file names, each as its components: [`INSTRUCTION_FILES`] and the owner's
/// configured names. A configured name that is empty, absolute, or has an empty, `.` or `..`
/// component is ignored. Compared ASCII case-insensitively, like protected names.
struct Names(Vec<Vec<OsString>>);
impl Names {
    fn new(configured: &[String]) -> Self {
        let mut names: Vec<Vec<OsString>> = INSTRUCTION_FILES
            .iter()
            .map(|name| vec![OsString::from(name)])
            .collect();
        for name in configured {
            let parts: Vec<&str> = name.split('/').collect();
            if parts
                .iter()
                .any(|p| p.is_empty() || *p == "." || *p == "..")
            {
                continue;
            }
            names.push(parts.into_iter().map(OsString::from).collect());
        }
        Self(names)
    }
    fn ends_with(path: &[&OsStr], name: &[OsString]) -> bool {
        name.len() <= path.len()
            && path[path.len() - name.len()..]
                .iter()
                .zip(name)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
    }
    /// `path` (project-relative) ends with an instruction file's name.
    fn is_instruction(&self, path: &Path) -> bool {
        let parts: Vec<&OsStr> = path.iter().collect();
        self.0.iter().any(|name| Self::ends_with(&parts, name))
    }
    /// `path` ends with the leading folders of a longer name (`.github` for
    /// `.github/copilot-instructions.md`), so a symlink there could stand in for that folder.
    fn leads_to_instruction(&self, path: &Path) -> bool {
        let parts: Vec<&OsStr> = path.iter().collect();
        self.0
            .iter()
            .any(|name| (1..name.len()).any(|k| Self::ends_with(&parts, &name[..k])))
    }
}

fn beneath() -> ResolveFlags {
    ResolveFlags::BENEATH
        | ResolveFlags::NO_SYMLINKS
        | ResolveFlags::NO_XDEV
        | ResolveFlags::NO_MAGICLINKS
}
fn open_dir(dir: impl AsFd, path: impl AsRef<Path>) -> rustix::io::Result<OwnedFd> {
    fs::openat2(
        dir,
        path.as_ref(),
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
fn ino(stat: &Stat) -> u64 {
    stat.st_ino as u64
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
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
fn times(stat: &Stat) -> fs::Timestamps {
    fs::Timestamps {
        last_access: fs::Timespec {
            tv_sec: stat.st_atime as i64,
            tv_nsec: stat.st_atime_nsec as _,
        },
        last_modification: fs::Timespec {
            tv_sec: stat.st_mtime as i64,
            tv_nsec: stat.st_mtime_nsec as _,
        },
    }
}
fn same_inode(a: &Stat, b: &Stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}
/// The entry `b` is the entry `a` was, unchanged, though a rename since may have moved its ctime.
fn same_entry(a: &Stat, b: &Stat) -> bool {
    let (fa, fb) = (fingerprint(a), fingerprint(b));
    same_inode(a, b) && kind(a) == kind(b) && (fa.size, fa.mtime_ns) == (fb.size, fb.mtime_ns)
}
fn permissions(stat: &Stat) -> u32 {
    stat.st_mode & 0o777
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
/// An entry's (device, inode).
type Id = (u64, u64);
fn id(stat: &Stat) -> Id {
    (dev(stat), ino(stat))
}
/// How many fresh temp names to try before giving up on an entry.
const TEMP_TRIES: usize = 8;
/// `.<name>.blackwall-<16 hex>`, with the name cut so the whole stays within `NAME_MAX`.
fn temp_name(name: &OsStr) -> OsString {
    let bytes = name.as_bytes();
    let mut temp = OsString::from(".");
    temp.push(OsStr::from_bytes(&bytes[..bytes.len().min(200)]));
    temp.push(format!(".blackwall-{:016x}", temp_suffix()));
    temp
}
fn temp_suffix() -> u64 {
    #[cfg(test)]
    if let Some(suffix) = TEMP_SUFFIXES.with(|queue| queue.borrow_mut().pop_front()) {
        return suffix;
    }
    rand::random()
}
/// Run `make` with a fresh temp name next to `name` until one isn't taken, at most
/// [`TEMP_TRIES`] times (then `EEXIST`). A name that is taken is never touched.
fn with_temp<T>(
    name: &OsStr,
    mut make: impl FnMut(&OsStr) -> rustix::io::Result<T>,
) -> rustix::io::Result<(OsString, T)> {
    for _ in 0..TEMP_TRIES {
        let temp = temp_name(name);
        match make(&temp) {
            Err(Errno::EXIST) => continue,
            made => return made.map(|made| (temp, made)),
        }
    }
    Err(Errno::EXIST)
}

/// Where an outside writer can race an entry: just before its commit (or its move aside for a
/// delete), just before undoing a swap, and just before removing what was moved aside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Moment {
    Commit,
    Undo,
    Unlink,
}
/// A test's stand-in for an outside writer.
#[cfg(test)]
type Racer = Box<dyn FnMut(&Path, Moment)>;
#[cfg(test)]
thread_local! {
    /// Runs at each [`Moment`], to race apply in tests.
    static BEFORE_COMMIT: std::cell::RefCell<Option<Racer>> =
        const { std::cell::RefCell::new(None) };
    /// Temp-name suffixes to use before random ones, so a test can plant the next name.
    static TEMP_SUFFIXES: std::cell::RefCell<std::collections::VecDeque<u64>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}
fn before_commit(path: &Path, moment: Moment) {
    #[cfg(test)]
    BEFORE_COMMIT.with(|hook| {
        let taken = hook.borrow_mut().take();
        if let Some(mut run) = taken {
            run(path, moment);
            *hook.borrow_mut() = Some(run);
        }
    });
    #[cfg(not(test))]
    let _ = (path, moment);
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
struct Planner<'a> {
    rules: &'a Rules<'a>,
    seen: usize,
    too_many: bool,
    /// Overlay metadata that isn't understood was found: nothing is applied.
    unsupported: bool,
    buffer: Vec<u8>,
    refused: Capped<(PathBuf, Refusal)>,
}
impl Planner<'_> {
    fn refuse(&mut self, path: PathBuf, why: Refusal) {
        self.refused.push((path, why));
    }

    /// What a file's or folder's xattrs say about it. A list longer than the 64 KiB buffer (the
    /// kernel's own limit) can't be read whole, so it is `Unsupported` too.
    fn metadata(&mut self, fd: &OwnedFd) -> rustix::io::Result<Meta> {
        let len = match fs::flistxattr(fd, &mut self.buffer[..]) {
            Ok(len) => len,
            Err(Errno::OPNOTSUPP) => 0,
            Err(Errno::RANGE) => return Ok(Meta::Unsupported),
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
        let budget = self.rules.limit.saturating_sub(self.seen).saturating_add(1);
        let names = names(dir, budget).ok()?;
        let mut entries = Vec::new();
        for name in names.iter().cloned() {
            self.seen += 1;
            if self.seen > self.rules.limit {
                self.too_many = true;
                self.refuse(rel.join(&name), Refusal::TooMany);
                break;
            }
            let path = rel.join(&name);
            match self.entry(dir, &path, &name, depth) {
                Ok(node) => entries.push(Entry { name, node }),
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
    ) -> Result<Node, Refusal> {
        let stat = stat_at(dir, name).map_err(|_| Refusal::Io)?;
        let file_type = kind(&stat);
        let is_dir = file_type == FileType::Directory;
        // Files and folders: pin them and read their xattrs before anything can refuse them.
        let opened = if is_dir || file_type == FileType::RegularFile {
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
            let meta = self.metadata(&fd).map_err(|_| Refusal::Io)?;
            if meta == Meta::Unsupported || (meta == Meta::Opaque && !is_dir) {
                self.unsupported = true;
                return Err(Refusal::UnsupportedOverlay);
            }
            Some((fd, opened, meta))
        } else {
            None
        };
        if is_protected(name) {
            return Err(Refusal::ProtectedName);
        }
        if self.rules.names.is_instruction(path) {
            return Err(Refusal::InstructionFile);
        }
        if file_type == FileType::CharacterDevice && stat.st_rdev == 0 {
            return Ok(Node::Whiteout);
        }
        if file_type == FileType::Symlink {
            if self.rules.names.leads_to_instruction(path) {
                return Err(Refusal::InstructionFile);
            }
            let target = fs::readlinkat(dir, name, Vec::new()).map_err(|_| Refusal::Io)?;
            let target = OsString::from_vec(target.into_bytes());
            return match link_destination(path, &target) {
                Some(_) => Ok(Node::Symlink { target }),
                None => Err(Refusal::LinkOutside),
            };
        }
        let Some((fd, opened, meta)) = opened else {
            return Err(Refusal::SpecialFile);
        };
        if meta == Meta::Capability {
            return Err(Refusal::SetId);
        }
        if !is_dir {
            // A new folder drops its set-id bits instead (only the 0o777 bits are applied).
            if opened.st_mode & SET_ID != 0 {
                return Err(Refusal::SetId);
            }
            return Ok(Node::File {
                source: fingerprint(&opened),
                mode: permissions(&opened),
            });
        }
        if depth >= MAX_DEPTH {
            return Err(Refusal::Io);
        }
        let (names, entries) = self.dir(&fd, path, depth + 1).ok_or(Refusal::Io)?;
        Ok(Node::Dir {
            mode: permissions(&opened),
            opaque: meta == Meta::Opaque,
            names: names.into_iter().collect(),
            entries,
        })
    }
}

/// A project folder as pass 2 found it: its identity, to find it again for the deletes, and
/// whether it or a folder above it (not the root) changed during the run.
#[derive(Clone, Copy)]
struct Pin {
    dev: u64,
    ino: u64,
    fresh: bool,
}
/// An entry's place in the project: its open folder, its project-relative path, and whether that
/// folder or one above it changed during the run.
struct At<'d> {
    dir: BorrowedFd<'d>,
    path: PathBuf,
    fresh: bool,
}
impl At<'_> {
    fn name(&self) -> &OsStr {
        self.path.file_name().unwrap_or_default()
    }
    fn sibling(&self, name: &OsStr) -> PathBuf {
        self.path.with_file_name(name)
    }
}
/// A delete kept for the end of pass 2.
enum Delete<'p> {
    /// A whiteout at `path`, whose folder is `parent`.
    Whiteout { path: PathBuf, parent: Pin },
    /// An opaque folder at `path`: what it holds that `keep` doesn't name goes.
    Clear {
        path: PathBuf,
        dir: Pin,
        keep: &'p HashSet<OsString>,
    },
}
/// What the project has at an entry's path now.
enum Judged {
    /// Nothing, and nothing was there at the start.
    Absent,
    /// A non-folder it may replace or delete: unchanged since the start (this `Stat`).
    Replaceable(Stat),
    /// Anything else: a folder, or something new, changed or gone since the start.
    Changed,
}
enum Outcome {
    Applied,
    Conflict,
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
    rules: &'a Rules<'a>,
    report: Report,
}
impl Applier<'_> {
    fn fresh(&self, stat: &Stat) -> bool {
        fingerprint(stat).ctime_ns >= self.rules.since
    }

    fn entries<'p>(
        &mut self,
        dir: BorrowedFd<'_>,
        rel: &Path,
        entries: &'p [Entry],
        pin: Pin,
        opaque: bool,
        deletes: &mut Vec<Delete<'p>>,
    ) {
        for entry in entries {
            let at = At {
                dir,
                path: rel.join(&entry.name),
                fresh: pin.fresh,
            };
            match &entry.node {
                Node::Dir { .. } => self.dir(&at, entry, opaque, deletes),
                Node::File { source, mode } => self.file(&at, source, *mode),
                Node::Symlink { target } => self.link(&at, target),
                Node::Whiteout => deletes.push(Delete::Whiteout {
                    path: at.path,
                    parent: pin,
                }),
            }
        }
    }

    fn judge(&self, at: &At<'_>) -> rustix::io::Result<Judged> {
        match stat_at(at.dir, at.name()) {
            Err(Errno::NOENT) if self.rules.start.contains_key(&at.path) => Ok(Judged::Changed),
            Err(Errno::NOENT) => Ok(Judged::Absent),
            Err(error) => Err(error),
            Ok(stat) if kind(&stat) == FileType::Directory => Ok(Judged::Changed),
            Ok(stat) => {
                let unchanged = match self.rules.start.get(&at.path) {
                    Some(start) => {
                        kind(&stat) == FileType::RegularFile && *start == fingerprint(&stat)
                    }
                    None => !at.fresh && !self.fresh(&stat),
                };
                Ok(if unchanged {
                    Judged::Replaceable(stat)
                } else {
                    Judged::Changed
                })
            }
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

    /// Put our `temp` entry (in `at.dir`, with identity `mine`) at `at`'s name, given what the
    /// last check found there. Whatever happens, `temp` doesn't stay behind holding ours, and
    /// nothing at `temp` that isn't ours is removed.
    fn commit(
        &mut self,
        at: &At<'_>,
        temp: &OsStr,
        mine: Id,
        before: Judged,
        temp_is_dir: bool,
    ) -> Result<Outcome, Refusal> {
        let failed = |error| match error {
            Errno::INVAL | Errno::NOSYS => Refusal::NoSafeReplace,
            _ => Refusal::Io,
        };
        let seen = match before {
            Judged::Changed => {
                self.discard(at, temp, mine, temp_is_dir);
                return Ok(Outcome::Conflict);
            }
            Judged::Absent => {
                before_commit(&at.path, Moment::Commit);
                let renamed =
                    fs::renameat_with(at.dir, temp, at.dir, at.name(), RenameFlags::NOREPLACE);
                return match renamed {
                    Ok(()) => Ok(Outcome::Applied),
                    Err(error) => {
                        self.discard(at, temp, mine, temp_is_dir);
                        match error {
                            Errno::EXIST => Ok(Outcome::Conflict),
                            _ => Err(failed(error)),
                        }
                    }
                };
            }
            Judged::Replaceable(seen) => seen,
        };
        before_commit(&at.path, Moment::Commit);
        if let Err(error) =
            fs::renameat_with(at.dir, temp, at.dir, at.name(), RenameFlags::EXCHANGE)
        {
            self.discard(at, temp, mine, temp_is_dir);
            return match error {
                Errno::NOENT => Ok(Outcome::Conflict),
                _ => Err(failed(error)),
            };
        }
        // `temp` now names what was at the target. Only what was checked may go.
        if stat_at(at.dir, temp).is_ok_and(|displaced| same_entry(&displaced, &seen)) {
            if fs::unlinkat(at.dir, temp, AtFlags::empty()).is_err() {
                self.report.refuse(&at.sibling(temp), Refusal::Io);
            }
            return Ok(Outcome::Applied);
        }
        // Someone else's: swap back, then remove only what is ours.
        before_commit(&at.path, Moment::Undo);
        match fs::renameat_with(at.dir, temp, at.dir, at.name(), RenameFlags::EXCHANGE) {
            Ok(()) => self.discard(at, temp, mine, temp_is_dir),
            // Ours may still be at the target and theirs at the temp name.
            Err(_) => self.report.conflict(&at.sibling(temp)),
        }
        Ok(Outcome::Conflict)
    }

    /// Remove our temp entry if `temp` still holds it (`mine`). Anything else there is left
    /// alone and reported, as is a removal that fails.
    fn discard(&mut self, at: &At<'_>, temp: &OsStr, mine: Id, is_dir: bool) {
        let flags = if is_dir {
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        match stat_at(at.dir, temp) {
            Err(Errno::NOENT) => {}
            Ok(stat) if id(&stat) == mine => {
                if fs::unlinkat(at.dir, temp, flags).is_err() {
                    self.report.refuse(&at.sibling(temp), Refusal::Io);
                }
            }
            _ => self.report.conflict(&at.sibling(temp)),
        }
    }

    fn record(&mut self, path: &Path, outcome: Result<Outcome, Refusal>) {
        match outcome {
            Ok(Outcome::Applied) => self.report.applied.push(path.to_path_buf()),
            Ok(Outcome::Conflict) => self.report.conflict(path),
            Err(why) => self.report.refuse(path, why),
        }
    }

    fn dir<'p>(
        &mut self,
        at: &At<'_>,
        entry: &'p Entry,
        opaque: bool,
        deletes: &mut Vec<Delete<'p>>,
    ) {
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
        let stat = match stat_at(at.dir, at.name()) {
            Ok(stat) => Some(stat),
            Err(Errno::NOENT) => None,
            Err(_) => return self.report.refuse(&at.path, Refusal::Io),
        };
        match stat {
            Some(stat) if kind(&stat) == FileType::Directory => {
                let fd = match self.open_project_dir(at.dir, at.name(), &stat) {
                    Ok(fd) => fd,
                    Err(why) => return self.report.refuse(&at.path, why),
                };
                let pin = Pin {
                    dev: dev(&stat),
                    ino: ino(&stat),
                    fresh: at.fresh || self.fresh(&stat),
                };
                if opaque {
                    deletes.push(Delete::Clear {
                        path: at.path.clone(),
                        dir: pin,
                        keep: names,
                    });
                }
                self.entries(fd.as_fd(), &at.path, entries, pin, opaque, deletes);
            }
            Some(stat) if kind(&stat) == FileType::Symlink => {
                self.report.refuse(&at.path, Refusal::LinkOutside);
            }
            Some(_) => match self.judge(at) {
                Ok(Judged::Replaceable(_)) => self.replace_with_dir(at, *mode, entries, deletes),
                Ok(_) => self.report.conflict(&at.path),
                Err(_) => self.report.refuse(&at.path, Refusal::Io),
            },
            None if self.rules.start.contains_key(&at.path) => self.report.conflict(&at.path),
            None => self.new_dir(at, *mode, entries, deletes),
        }
    }

    /// Make and open a private folder `name` in `at.dir` (0o700 whatever the umask).
    /// `Err(EXIST)`: the name is taken, and nothing was touched. If it was made but can't be
    /// opened, it is taken away again, only if it is still the folder just made; otherwise it is
    /// left and reported.
    fn make_dir(&mut self, at: &At<'_>, name: &OsStr) -> Result<(OwnedFd, Stat), Errno> {
        fs::mkdirat(at.dir, name, Mode::from_raw_mode(0o700))?;
        let made = stat_at(at.dir, name);
        let opened = made.as_ref().ok().and_then(|stat| {
            let fd = self.open_project_dir(at.dir, name, stat).ok()?;
            fs::fchmod(&fd, Mode::from_raw_mode(0o700)).ok()?;
            Some(fd)
        });
        if let (Some(fd), Ok(stat)) = (opened, &made) {
            return Ok((fd, *stat));
        }
        let path = at.sibling(name);
        let taken_away = made.is_ok_and(|stat| {
            let made = id(&stat);
            let is_it = |moved: &Stat| kind(moved) == FileType::Directory && id(moved) == made;
            matches!(
                self.remove_checked(at.dir, &path, name, true, is_it),
                Removal::Removed
            )
        });
        // The caller reports its own path.
        if !taken_away && path != at.path {
            self.report.refuse(&path, Refusal::Io);
        }
        Err(Errno::IO)
    }

    fn new_dir<'p>(
        &mut self,
        at: &At<'_>,
        mode: u32,
        entries: &'p [Entry],
        deletes: &mut Vec<Delete<'p>>,
    ) {
        before_commit(&at.path, Moment::Commit);
        match self.make_dir(at, at.name()) {
            Ok((fd, stat)) => self.fill(at, fd, &stat, mode, entries, deletes),
            Err(Errno::EXIST) => self.report.conflict(&at.path),
            Err(_) => self.report.refuse(&at.path, Refusal::Io),
        }
    }

    /// An unchanged file the command replaced with a folder: swap a new folder in.
    fn replace_with_dir<'p>(
        &mut self,
        at: &At<'_>,
        mode: u32,
        entries: &'p [Entry],
        deletes: &mut Vec<Delete<'p>>,
    ) {
        let (temp, (fd, stat)) = match with_temp(at.name(), |temp| self.make_dir(at, temp)) {
            Ok(made) => made,
            Err(_) => return self.report.refuse(&at.path, Refusal::Io),
        };
        let before = self.judge(at).unwrap_or(Judged::Changed);
        match self.commit(at, &temp, id(&stat), before, true) {
            Ok(Outcome::Applied) => self.fill(at, fd, &stat, mode, entries, deletes),
            other => self.record(&at.path, other),
        }
    }

    /// Apply a new folder's entries, then give it its staged mode less the umask (no set-id
    /// bits).
    fn fill<'p>(
        &mut self,
        at: &At<'_>,
        fd: OwnedFd,
        stat: &Stat,
        mode: u32,
        entries: &'p [Entry],
        deletes: &mut Vec<Delete<'p>>,
    ) {
        // Ours, so anything found in it that isn't from the plan is someone else's.
        let pin = Pin {
            dev: dev(stat),
            ino: ino(stat),
            fresh: true,
        };
        self.entries(fd.as_fd(), &at.path, entries, pin, false, deletes);
        match fs::fchmod(&fd, Mode::from_raw_mode(mode & 0o777 & !self.rules.umask)) {
            Ok(()) => self.report.applied.push(at.path.clone()),
            Err(_) => self.report.refuse(&at.path, Refusal::Io),
        }
    }

    fn file(&mut self, at: &At<'_>, source: &Fingerprint, mode: u32) {
        let outcome = match self.judge(at) {
            Ok(Judged::Changed) => Ok(Outcome::Conflict),
            // A new file gets what a direct write would; a replaced one keeps its staged mode.
            Ok(Judged::Absent) => self.copy_in(at, source, mode & !self.rules.umask),
            Ok(Judged::Replaceable(_)) => self.copy_in(at, source, mode),
            Err(_) => Err(Refusal::Io),
        };
        self.record(&at.path, outcome);
    }

    /// Copy the staged file into a fresh temp file next to its target, with `mode` and the staged
    /// file's access and modification times (so a build tool's freshness checks still hold; the
    /// conflict checks go by ctime, which can't be set), then commit it.
    fn copy_in(
        &mut self,
        at: &At<'_>,
        source: &Fingerprint,
        mode: u32,
    ) -> Result<Outcome, Refusal> {
        let staged = open_file(self.upper, &at.path).map_err(|_| Refusal::Io)?;
        let stat = fs::fstat(&staged).map_err(|_| Refusal::Io)?;
        if kind(&stat) != FileType::RegularFile || fingerprint(&stat) != *source {
            return Err(Refusal::Io);
        }
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let created = with_temp(at.name(), |temp| {
            fs::openat2(at.dir, temp, flags, Mode::from_raw_mode(0o600), beneath())
        });
        let (temp, out) = created.map_err(|_| Refusal::Io)?;
        let Ok(mine) = fs::fstat(&out).map(|stat| id(&stat)) else {
            // Without its identity it can't be told from anyone else's: leave it, and say so.
            self.report.refuse(&at.sibling(&temp), Refusal::Io);
            return Err(Refusal::Io);
        };
        let filled = (|| -> std::io::Result<()> {
            let mut out = std::fs::File::from(out);
            // Bounded: `io::copy` uses copy_file_range, or a small buffer.
            let copied = std::io::copy(&mut std::fs::File::from(staged), &mut out)?;
            if copied != source.size {
                return Err(std::io::Error::other("the staged file changed size"));
            }
            fs::fchmod(&out, Mode::from_raw_mode(mode & 0o777))?;
            fs::futimens(&out, &times(&stat))?;
            fs::fsync(&out)?;
            Ok(())
        })();
        let before = filled.ok().and_then(|()| self.judge(at).ok());
        let Some(before) = before else {
            self.discard(at, &temp, mine, false);
            return Err(Refusal::Io);
        };
        self.commit(at, &temp, mine, before, false)
    }

    fn link(&mut self, at: &At<'_>, target: &OsStr) {
        let outcome = match self.judge(at) {
            Ok(Judged::Changed) => Ok(Outcome::Conflict),
            Ok(_) if !self.through_real_entries(&at.path, target) => Err(Refusal::LinkOutside),
            Ok(Judged::Absent) => {
                before_commit(&at.path, Moment::Commit);
                // symlinkat never replaces anything.
                match fs::symlinkat(target, at.dir, at.name()) {
                    Ok(()) => Ok(Outcome::Applied),
                    Err(Errno::EXIST) => Ok(Outcome::Conflict),
                    Err(_) => Err(Refusal::Io),
                }
            }
            Ok(Judged::Replaceable(_)) => {
                match with_temp(at.name(), |temp| fs::symlinkat(target, at.dir, temp)) {
                    Ok((temp, ())) => match stat_at(at.dir, &temp) {
                        Ok(made) if kind(&made) == FileType::Symlink => {
                            let before = self.judge(at).unwrap_or(Judged::Changed);
                            self.commit(at, &temp, id(&made), before, false)
                        }
                        // Not provably ours: leave it, and say so.
                        _ => {
                            self.report.refuse(&at.sibling(&temp), Refusal::Io);
                            Err(Refusal::Io)
                        }
                    },
                    Err(_) => Err(Refusal::Io),
                }
            }
            Err(_) => Err(Refusal::Io),
        };
        self.record(&at.path, outcome);
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

    /// Open a folder pinned earlier in pass 2 again, if it is still that folder.
    fn reopen(&self, rel: &Path, pin: &Pin) -> Option<OwnedFd> {
        let path = if rel.as_os_str().is_empty() {
            Path::new(".")
        } else {
            rel
        };
        let fd = open_dir(self.project, path).ok()?;
        let stat = fs::fstat(&fd).ok()?;
        ((dev(&stat), ino(&stat)) == (pin.dev, pin.ino)).then_some(fd)
    }

    /// A project folder's names, or the refusal for a folder that can't be listed or has more
    /// than the entry limit (so it can't be known in full).
    fn listing(&self, fd: &OwnedFd) -> Result<Vec<OsString>, Refusal> {
        let limit = self.rules.limit;
        match names(fd, limit.saturating_add(1)) {
            Ok(names) if names.len() > limit => Err(Refusal::TooMany),
            Ok(names) => Ok(names),
            Err(_) => Err(Refusal::Io),
        }
    }

    /// The whiteouts and opaque clears, after everything else: held back if anything so far was
    /// refused or conflicted.
    fn deletes(&mut self, deletes: Vec<Delete<'_>>) {
        let hold = !self.report.refused.is_empty() || !self.report.conflicted.is_empty();
        for delete in deletes {
            let (folder, pin) = match &delete {
                Delete::Whiteout { path, parent } => {
                    (path.parent().unwrap_or(Path::new("")), *parent)
                }
                Delete::Clear { path, dir, .. } => (path.as_path(), *dir),
            };
            let Some(fd) = self.reopen(folder, &pin) else {
                let path = match &delete {
                    Delete::Whiteout { path, .. } | Delete::Clear { path, .. } => path,
                };
                self.report.conflict(path);
                continue;
            };
            let targets: Vec<OsString> = match &delete {
                Delete::Whiteout { path, .. } => path
                    .file_name()
                    .map(OsStr::to_os_string)
                    .into_iter()
                    .collect(),
                Delete::Clear { keep, .. } => match self.listing(&fd) {
                    Ok(names) => names.into_iter().filter(|n| !keep.contains(n)).collect(),
                    Err(why) => {
                        self.report.refuse(folder, why);
                        continue;
                    }
                },
            };
            for name in targets {
                let path = folder.join(&name);
                if hold {
                    if stat_at(&fd, &name).is_ok() {
                        self.report.held_back.push(path);
                    }
                    continue;
                }
                let mut removed = Vec::new();
                self.remove(fd.as_fd(), &path, &name, pin.fresh, &mut removed);
                self.report.deleted.extend(removed);
            }
        }
    }

    /// Delete `name` in `parent` if it is unchanged since the start (see the module doc), or a
    /// folder on the project's file system that held only such entries. Anything else in it is
    /// kept and reported, and so is the folder. A folder removed whole is listed in `removed`
    /// once; from a folder that stays, what was removed from it is listed instead.
    fn remove(
        &mut self,
        parent: BorrowedFd<'_>,
        path: &Path,
        name: &OsStr,
        fresh_above: bool,
        removed: &mut Vec<PathBuf>,
    ) -> Removal {
        let kept = |report: &mut Report, why: Option<Refusal>| {
            match why {
                Some(why) => report.refuse(path, why),
                None => report.conflict(path),
            }
            Removal::Kept
        };
        if is_protected(name) {
            return kept(&mut self.report, Some(Refusal::ProtectedName));
        }
        if self.rules.names.is_instruction(path) {
            return kept(&mut self.report, Some(Refusal::InstructionFile));
        }
        let stat = match stat_at(parent, name) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Removal::Gone,
            Err(_) => return kept(&mut self.report, Some(Refusal::Io)),
        };
        if kind(&stat) != FileType::Directory {
            let unchanged = match self.rules.start.get(path) {
                Some(start) => kind(&stat) == FileType::RegularFile && *start == fingerprint(&stat),
                None => !fresh_above && !self.fresh(&stat),
            };
            if !unchanged {
                return kept(&mut self.report, None);
            }
            let removal =
                self.remove_checked(parent, path, name, false, |moved| same_entry(moved, &stat));
            if let Removal::Removed = removal {
                removed.push(path.to_path_buf());
            }
            return removal;
        }
        // A folder below one changed during the run is unknown: keep it whole.
        if fresh_above {
            return kept(&mut self.report, None);
        }
        let opened = if path.components().count() > MAX_DEPTH {
            Err(Refusal::Io)
        } else {
            self.open_project_dir(parent, name, &stat)
        };
        let listed = opened.and_then(|fd| self.listing(&fd).map(|names| (fd, names)));
        let (fd, names) = match listed {
            Ok(listed) => listed,
            Err(why) => return kept(&mut self.report, Some(why)),
        };
        let fresh = self.fresh(&stat);
        let mut inside = Vec::new();
        let mut whole = true;
        for child in &names {
            let child_path = path.join(child);
            if let Removal::Kept = self.remove(fd.as_fd(), &child_path, child, fresh, &mut inside) {
                whole = false;
            }
        }
        if !whole {
            removed.extend(inside);
            return Removal::Kept;
        }
        // Changed during the run (an empty folder the owner made, say): it stays.
        if fresh {
            removed.extend(inside);
            return kept(&mut self.report, None);
        }
        // Only the folder just emptied (still held open) may go.
        let Ok(held) = fs::fstat(&fd).map(|held| id(&held)) else {
            removed.extend(inside);
            return kept(&mut self.report, Some(Refusal::Io));
        };
        let is_it = |moved: &Stat| kind(moved) == FileType::Directory && id(moved) == held;
        match self.remove_checked(parent, path, name, true, is_it) {
            Removal::Removed => {
                removed.push(path.to_path_buf());
                Removal::Removed
            }
            other => {
                removed.extend(inside);
                other
            }
        }
    }

    /// Delete `name` in `parent` without check-then-act: rename it aside to a fresh temp name,
    /// check what moved with `is_it`, then unlink (or, for a folder, rmdir) it. If it isn't what
    /// was checked, or removing it fails (a folder no longer empty, say), it is put back and
    /// reported as a conflict; if putting it back fails too, the temp path is reported as well.
    fn remove_checked(
        &mut self,
        parent: BorrowedFd<'_>,
        path: &Path,
        name: &OsStr,
        is_dir: bool,
        is_it: impl Fn(&Stat) -> bool,
    ) -> Removal {
        before_commit(path, Moment::Commit);
        let moved = with_temp(name, |temp| {
            fs::renameat_with(parent, name, parent, temp, RenameFlags::NOREPLACE)
        });
        let temp = match moved {
            Ok((temp, ())) => temp,
            Err(Errno::NOENT) => return Removal::Gone,
            Err(Errno::INVAL | Errno::NOSYS) => {
                self.report.refuse(path, Refusal::NoSafeReplace);
                return Removal::Kept;
            }
            Err(_) => {
                self.report.refuse(path, Refusal::Io);
                return Removal::Kept;
            }
        };
        if stat_at(parent, &temp).is_ok_and(|moved| is_it(&moved)) {
            before_commit(path, Moment::Unlink);
            let flags = if is_dir {
                AtFlags::REMOVEDIR
            } else {
                AtFlags::empty()
            };
            if fs::unlinkat(parent, &temp, flags).is_ok() {
                return Removal::Removed;
            }
        }
        // Not what was checked, or it can't be removed: put it back where it was.
        self.report.conflict(path);
        if fs::renameat_with(parent, &temp, parent, name, RenameFlags::NOREPLACE).is_err() {
            self.report.conflict(&path.with_file_name(&temp));
        }
        Removal::Kept
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
    /// The start of a run: the walk, and the moment from which changes are the owner's.
    struct Run {
        start: HashMap<PathBuf, Fingerprint>,
        root: (u64, u64),
        since: i128,
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
            Self::at(std::fs::canonicalize(root).unwrap())
        }
        /// [`Tree::new`] in the temp folder, never tmpfs.
        fn on_disk() -> Self {
            let name = format!("blackwall-apply-{:016x}", rand::random::<u64>());
            let root = std::env::temp_dir().join(name);
            std::fs::create_dir(&root).unwrap();
            Self::at(std::fs::canonicalize(root).unwrap())
        }
        fn at(root: PathBuf) -> Self {
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
        /// Walk the project and start the run: everything made before is the project's, and
        /// anything made after (past one coarse clock tick) is the owner's, during the run.
        fn run(&self) -> Run {
            let walk = walk_with_files(&self.project, MAX_ENTRIES).unwrap();
            let since = to_ns(SystemTime::now());
            std::thread::sleep(Duration::from_millis(30));
            Run {
                start: walk.files,
                root: walk.root,
                since,
            }
        }
        fn apply_with(&self, run: &Run, names: &[&str], limit: usize) -> Report {
            let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
            let rules = Rules {
                start: &run.start,
                since: run.since,
                names: Names::new(&names),
                limit,
                umask: 0o022,
            };
            apply_within(&self.upper, &self.project, run.root, &rules)
        }
        fn apply(&self, run: &Run) -> Report {
            self.apply_with(run, &[], MAX_ENTRIES)
        }
        fn read(&self, relative: &str) -> String {
            std::fs::read_to_string(self.project.join(relative)).unwrap()
        }
        fn has(&self, relative: &str) -> bool {
            std::fs::symlink_metadata(self.project.join(relative)).is_ok()
        }
        fn listing_of(base: &Path) -> Vec<String> {
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
            visit(base, base, &mut out);
            out.sort();
            out
        }
        /// Every path in the project, sorted.
        fn listing(&self) -> Vec<String> {
            Self::listing_of(&self.project)
        }
        fn no_temp_left(&self) {
            let listing = self.listing();
            assert!(
                listing.iter().all(|p| !p.contains(".blackwall-")),
                "temp left: {listing:?}"
            );
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
    /// Run `hook` with each path just before its commit, for the rest of this thread's test.
    fn race(hook: impl FnMut(&Path, Moment) + 'static) {
        BEFORE_COMMIT.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }
    /// Use these temp-name suffixes next, before random ones.
    fn suffixes(next: &[u64]) {
        TEMP_SUFFIXES.with(|queue| queue.borrow_mut().extend(next));
    }
    /// Clears the racer and planted suffixes when a test ends.
    struct NoRace;
    impl Drop for NoRace {
        fn drop(&mut self) {
            BEFORE_COMMIT.with(|slot| *slot.borrow_mut() = None);
            TEMP_SUFFIXES.with(|queue| queue.borrow_mut().clear());
        }
    }
    fn planted(dir: &Path, name: &str, suffix: u64) -> PathBuf {
        dir.join(format!(".{name}.blackwall-{suffix:016x}"))
    }
    /// The one entry in `dir` that Blackwall moved aside from `name`.
    fn aside(dir: &Path, name: &str) -> PathBuf {
        let prefix = format!(".{name}.blackwall-");
        let found: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with(&prefix)
            })
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
        found[0].clone()
    }
    fn refusals(report: &Report) -> Vec<(&str, Refusal)> {
        assert_eq!(report.refused.total, report.refused.first.len());
        report
            .refused
            .first
            .iter()
            .map(|(path, why)| (path.to_str().unwrap(), *why))
            .collect()
    }
    /// A list's paths, which must all have been kept.
    fn paths(list: &Capped<PathBuf>) -> Vec<&str> {
        assert_eq!(list.total, list.first.len());
        list.first
            .iter()
            .map(|path| path.to_str().unwrap())
            .collect()
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
    /// Replace `path` the way an editor saves: a new inode renamed over it.
    fn save(path: &Path, content: &str) {
        let new = path.with_extension("owner-save");
        std::fs::write(&new, content).unwrap();
        std::fs::rename(&new, path).unwrap();
    }

    #[test]
    fn apply_writes_new_and_changed_files_and_reports_them() {
        let t = Tree::new();
        t.proj("changed.txt", "old");
        t.proj("same.txt", "same");
        t.proj("run.sh", "x");
        let lib = t.proj("lib/a.rs", "a").parent().unwrap().to_path_buf();
        chmod(&lib, 0o700);
        let run = t.run();
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

        let r = t.apply(&run);
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
            (
                r.refused.len(),
                r.conflicted.len(),
                r.deleted.len(),
                r.held_back.len()
            ),
            (0, 0, 0, 0),
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
        t.no_temp_left();
    }

    #[test]
    fn apply_refuses_protected_names_and_instruction_files_at_any_depth() {
        let t = Tree::new();
        t.proj("src/main.rs", "fn main() {}");
        let run = t.run();
        t.up(".git/config", "[core]");
        t.up("a/b/.claude/x", "x");
        t.up("a/b/fine.txt", "fine");
        t.up(".agents/main.rs", "fn main() {}"); // `mv src .agents`: the copy, and ...
        if !whiteout(&t.upper.join("src")) {
            return; // ... the whiteout where it was.
        }
        t.up("AGENTS.md", "obey");
        t.up("docs/claude.md", "obey");
        t.up("docs/ok.md", "ok");

        let r = t.apply(&run);
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
        // The move's source stays: its delete is held back.
        assert_eq!(t.read("src/main.rs"), "fn main() {}");
        assert_eq!(paths(&r.held_back), ["src"]);
        assert!(r.deleted.is_empty());
        assert!(r.to_string().contains(
            "1 held back (not deleted because other changes were refused or conflicted)"
        ));
    }

    #[test]
    fn apply_refuses_case_variants_symlinked_parents_and_links_hard_links_as_copies() {
        let t = Tree::new();
        symlink("../outside", t.project.join("a")).unwrap();
        let run = t.run();
        t.up(".Git/config", "[core]");
        t.up("a/f", "through the link");
        let h1 = t.up("h1", "linked");
        std::fs::hard_link(&h1, t.upper.join("h2")).unwrap();

        let r = t.apply(&run);
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
        symlink("/etc", t.project.join("ext")).unwrap(); // the owner's own link
        let link = |target: &str, name: &str| symlink(target, t.upper.join(name)).unwrap();
        link("/etc/passwd", "abs");
        link("../../x", "up_escape");
        link("lib/../../etc", "sneaky");
        link(".git/config", "to_git");
        link("ext/passwd", "via_owner");
        t.up_dir("d");
        link("../target.txt", "d/ok");
        t.up("target.txt", "t");
        chmod(&t.up("suid", "x"), 0o4755);
        chmod(&t.up("sgid_file", "x"), 0o2755);
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
            ("sgid_file", SetId),
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
        let run = t.run();

        let r = t.apply(&run);
        assert_eq!(refusals(&r), expected);
        assert_eq!(paths(&r.applied), ["d", "d/ok", "target.txt"]);
        assert_eq!(
            std::fs::read_link(t.project.join("d/ok")).unwrap(),
            Path::new("../target.txt")
        );
        assert_eq!(t.listing(), ["d", "d/ok", "ext", "target.txt"]);
        assert!(Tree::listing_of(&t.outside).is_empty());
    }

    #[test]
    fn apply_gives_a_new_folder_its_mode_without_set_id_bits() {
        let t = Tree::new();
        let run = t.run();
        let sgid = t.up("sgid/inner", "x").parent().unwrap().to_path_buf();
        chmod(&sgid, 0o2755);
        let r = t.apply(&run);
        assert!(r.refused.is_empty(), "{r:?}");
        assert_eq!(paths(&r.applied), ["sgid", "sgid/inner"]);
        assert_eq!(mode(&t.project.join("sgid")), 0o755);
    }

    #[test]
    fn apply_keeps_real_file_on_conflict() {
        let t = Tree::new();
        t.proj("f", "mine");
        let run = t.run();
        t.proj("f", "owner edit!");
        t.up("f", "command");
        let r = t.apply(&run);
        assert_eq!(paths(&r.conflicted), ["f"]);
        assert_eq!(t.read("f"), "owner edit!");
        assert!(r.applied.is_empty());
    }

    #[test]
    fn apply_keeps_anything_new_at_a_path_it_would_create() {
        let t = Tree::new();
        let run = t.run();
        // After the start: a file, a folder and a symlink where the command made files.
        t.proj("g", "owner");
        std::fs::create_dir(t.project.join("dir_vs_file")).unwrap();
        symlink("g", t.project.join("lnk")).unwrap();
        for name in ["g", "dir_vs_file", "lnk"] {
            t.up(name, "command");
        }
        symlink("g", t.upper.join("new_link")).unwrap();
        t.proj("new_link", "owner");
        let r = t.apply(&run);
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
        let run = t.run();
        std::fs::write(&f, "bbbb").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(std::fs::metadata(&f).unwrap().modified().unwrap(), mtime);
        t.up("f", "command");
        let r = t.apply(&run);
        assert_eq!(paths(&r.conflicted), ["f"]);
        assert_eq!(t.read("f"), "bbbb");
    }

    #[test]
    fn apply_breaks_project_hard_links_instead_of_writing_through_them() {
        let t = Tree::new();
        let a = t.proj("a", "old");
        std::fs::hard_link(&a, t.project.join("b")).unwrap();
        let run = t.run();
        t.up("a", "new");
        let r = t.apply(&run);
        assert_eq!(paths(&r.applied), ["a"]);
        assert_eq!((t.read("a"), t.read("b")), ("new".into(), "old".into()));
    }

    #[test]
    fn apply_copies_no_xattrs() {
        let t = Tree::new();
        let run = t.run();
        let f = t.up("f", "x");
        if !xattr(&f, "user.blackwall-test", b"1") {
            return;
        }
        let r = t.apply(&run);
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
    fn apply_replaces_an_unchanged_file_with_a_symlink_or_a_folder() {
        let t = Tree::new();
        t.proj("f", "file");
        t.proj("g", "file");
        t.proj("target", "t");
        let run = t.run();
        symlink("target", t.upper.join("f")).unwrap(); // `rm f; ln -s target f`
        t.up("g/inside", "in"); // `rm g; mkdir g`
        if !xattr(&t.upper.join("g"), "user.overlay.opaque", b"y") {
            std::fs::remove_dir_all(t.upper.join("g")).unwrap();
            t.up("g/inside", "in");
        }
        let r = t.apply(&run);
        assert_eq!(paths(&r.applied), ["f", "g", "g/inside"], "{r:?}");
        assert_eq!(
            std::fs::read_link(t.project.join("f")).unwrap(),
            Path::new("target")
        );
        assert_eq!(t.read("g/inside"), "in");
        t.no_temp_left();
    }

    #[test]
    fn apply_whiteouts_delete_only_unchanged_entries() {
        let t = Tree::new();
        t.proj("gone.txt", "g");
        t.proj("edited.txt", "e");
        t.proj("tree/a", "a");
        t.proj("tree/b/c", "c");
        t.proj("tree2/a", "a");
        let run = t.run();
        t.proj("edited.txt", "owner edit");
        t.proj("tree2/new_outside", "owner");
        for name in ["gone.txt", "edited.txt", "tree", "tree2", "never_existed"] {
            if !whiteout(&t.upper.join(name)) {
                return;
            }
        }
        let r = t.apply(&run);
        assert_eq!(paths(&r.deleted), ["gone.txt", "tree", "tree2/a"]);
        assert_eq!(paths(&r.conflicted), ["edited.txt", "tree2/new_outside"]);
        assert!(r.refused.is_empty() && r.applied.is_empty(), "{r:?}");
        assert_eq!(t.listing(), ["edited.txt", "tree2", "tree2/new_outside"]);
        t.no_temp_left();
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
        let run = t.run();
        t.up("d/new", "new");
        t.up("d/keep", "kept by the command");
        if !xattr(&t.upper.join("d"), "user.overlay.opaque", b"y") {
            return;
        }
        let r = t.apply(&run);
        assert_eq!(paths(&r.deleted), ["d/old", "d/sub/gone.txt", "d/whole"]);
        assert_eq!(
            refusals(&r),
            [
                ("d/.git", ProtectedName),
                ("d/AGENTS.md", InstructionFile),
                ("d/sub/.ssh", ProtectedName)
            ]
        );
        assert!(r.conflicted.is_empty(), "{r:?}");
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
                "d/sub",
                "d/sub/.ssh",
                "d/sub/.ssh/key"
            ]
        );
        assert_eq!(t.read("d/keep"), "kept by the command");
    }

    #[test]
    fn apply_opacity_reaches_folders_below_and_keeps_the_owners_new_file() {
        let t = Tree::new();
        t.proj("d/sub/old", "old");
        t.proj("d/sub/keep", "before");
        let run = t.run();
        t.proj("d/sub/outside_new", "owner");
        // `rm -rf d; mkdir -p d/sub; ...`: only d is marked opaque, d/sub is plain.
        t.up("d/sub/keep", "after");
        if !xattr(&t.upper.join("d"), "user.overlay.opaque", b"y") {
            return;
        }
        let r = t.apply(&run);
        assert_eq!(paths(&r.applied), ["d/sub/keep"]);
        assert_eq!(paths(&r.deleted), ["d/sub/old"]);
        assert_eq!(paths(&r.conflicted), ["d/sub/outside_new"]);
        assert_eq!(t.read("d/sub/outside_new"), "owner");
        assert_eq!(t.read("d/sub/keep"), "after");
    }

    #[test]
    fn apply_changes_settled_entries_below_skipped_folders() {
        let t = Tree::new();
        t.proj("node_modules/pkg/index.js", "old");
        t.proj("node_modules/pkg/stale.js", "stale");
        symlink("pkg/index.js", t.project.join("node_modules/main.js")).unwrap();
        let run = t.run();
        assert!(!run
            .start
            .contains_key(Path::new("node_modules/pkg/index.js")));
        t.up("node_modules/pkg/index.js", "new");
        symlink("pkg/stale.js", t.upper.join("node_modules/main.js")).unwrap();
        if !whiteout(&t.upper.join("node_modules/pkg/stale.js")) {
            return;
        }
        let r = t.apply(&run);
        assert_eq!(
            paths(&r.applied),
            ["node_modules/main.js", "node_modules/pkg/index.js"],
            "{r:?}"
        );
        assert_eq!(paths(&r.deleted), ["node_modules/pkg/stale.js"]);
        assert_eq!(t.read("node_modules/pkg/index.js"), "new");
        assert!(r.conflicted.is_empty() && r.refused.is_empty());
    }

    #[test]
    fn apply_counts_an_owner_change_just_before_the_run_as_a_conflict() {
        let t = Tree::new();
        t.proj("node_modules/pkg/index.js", "owner");
        let walk = walk_with_files(&t.project, MAX_ENTRIES).unwrap();
        t.up("node_modules/pkg/index.js", "command");
        // The file was changed a second before the run started: within the 2 s margin.
        let run_started = SystemTime::now() + Duration::from_secs(1);
        let r = apply(
            &t.upper,
            &t.project,
            walk.root,
            &walk.files,
            run_started,
            &[],
        );
        assert_eq!(paths(&r.conflicted), ["node_modules/pkg/index.js"]);
        assert_eq!(t.read("node_modules/pkg/index.js"), "owner");
    }

    #[test]
    fn apply_keeps_a_subtree_the_owner_moved_in_when_its_parent_is_whited_out() {
        let t = Tree::new();
        t.proj("node_modules/a/old.js", "old");
        t.proj("node_modules/b/old.js", "old");
        Tree::write(&t.outside, "pkg/lib.js", "the owner's");
        let run = t.run();
        std::fs::rename(t.outside.join("pkg"), t.project.join("node_modules/a/pkg")).unwrap();
        if !whiteout(&t.upper.join("node_modules")) {
            return;
        }
        let r = t.apply(&run);
        assert_eq!(t.read("node_modules/a/pkg/lib.js"), "the owner's");
        // Below the folder it moved into, everything the walk didn't record is unknown now.
        assert_eq!(paths(&r.deleted), ["node_modules/b"]);
        assert_eq!(
            paths(&r.conflicted),
            ["node_modules/a/old.js", "node_modules/a/pkg"]
        );
    }

    #[test]
    fn apply_keeps_an_empty_folder_the_owner_made_during_the_run() {
        let t = Tree::new();
        t.proj("d/old", "old");
        let run = t.run();
        std::fs::create_dir(t.project.join("d/new_dir")).unwrap();
        if !whiteout(&t.upper.join("d")) {
            return;
        }
        let r = t.apply(&run);
        assert_eq!(paths(&r.deleted), ["d/old"]);
        assert_eq!(paths(&r.conflicted), ["d/new_dir"]);
        assert_eq!(t.listing(), ["d", "d/new_dir"]);
    }

    #[test]
    fn apply_refuses_configured_instruction_files_and_links_standing_in_for_their_folders() {
        let t = Tree::new();
        t.proj("docs/copilot-instructions.md", "the owner's");
        let run = t.run();
        t.up("x/.github/copilot-instructions.md", "obey");
        t.up("x/.github/other.md", "fine");
        t.up("GEMINI.md", "obey");
        symlink("docs", t.upper.join(".GitHub")).unwrap();
        let names = [
            ".github/copilot-instructions.md",
            "gemini.md",
            "",
            "/etc/abs.md",
            "../up.md",
            "a//b.md",
            "./here.md",
        ];
        let r = t.apply_with(&run, &names, MAX_ENTRIES);
        assert_eq!(
            refusals(&r),
            [
                (".GitHub", InstructionFile),
                ("GEMINI.md", InstructionFile),
                ("x/.github/copilot-instructions.md", InstructionFile),
            ]
        );
        assert_eq!(paths(&r.applied), ["x", "x/.github", "x/.github/other.md"]);
        assert!(!t.has(".GitHub") && !t.has("GEMINI.md"));
    }

    #[test]
    fn apply_refuses_everything_when_the_project_folder_is_not_the_one_walked() {
        let t = Tree::new();
        let mut run = t.run();
        run.root.1 += 1;
        t.up("f", "x");
        let r = t.apply(&run);
        assert_eq!(refusals(&r), [("", ProjectChanged)]);
        assert!(t.listing().is_empty());
        assert!(r
            .to_string()
            .contains("(all changes) (the project folder changed"));
    }

    #[test]
    fn apply_keeps_an_owner_save_racing_a_replace() {
        let t = Tree::new();
        t.proj("f", "before");
        let run = t.run();
        t.up("f", "command");
        let _clear = NoRace;
        let target = t.project.join("f");
        race(move |path, moment| {
            if path == Path::new("f") && moment == Moment::Commit {
                save(&target, "owner saved");
            }
        });
        let r = t.apply(&run);
        assert_eq!(paths(&r.conflicted), ["f"], "{r:?}");
        assert!(r.applied.is_empty());
        assert_eq!(t.read("f"), "owner saved");
        t.no_temp_left();
    }

    #[test]
    fn apply_keeps_an_owner_save_racing_a_delete() {
        let t = Tree::new();
        t.proj("f", "before");
        let run = t.run();
        if !whiteout(&t.upper.join("f")) {
            return;
        }
        let _clear = NoRace;
        let target = t.project.join("f");
        race(move |_, moment| {
            if moment == Moment::Commit {
                save(&target, "owner saved");
            }
        });
        let r = t.apply(&run);
        assert_eq!(paths(&r.conflicted), ["f"], "{r:?}");
        assert!(r.deleted.is_empty());
        assert_eq!(t.read("f"), "owner saved");
        t.no_temp_left();
    }

    #[test]
    fn apply_cleans_up_its_temp_file_when_the_final_rename_fails() {
        let t = Tree::new();
        t.proj("f", "before");
        let run = t.run();
        t.up("f", "command");
        t.up("new", "command");
        let _clear = NoRace;
        let project = t.project.clone();
        race(move |path, moment| {
            if moment != Moment::Commit {
                return;
            }
            if path == Path::new("f") {
                std::fs::remove_file(project.join("f")).unwrap(); // the swap finds nothing
            } else {
                std::fs::write(project.join("new"), "owner").unwrap(); // no-replace finds this
            }
        });
        let r = t.apply(&run);
        assert_eq!(paths(&r.conflicted), ["f", "new"], "{r:?}");
        assert!(r.applied.is_empty());
        assert_eq!(t.listing(), ["new"]);
        assert_eq!(t.read("new"), "owner");
    }

    #[test]
    fn apply_leaves_whatever_holds_the_temp_name_after_an_undo() {
        let t = Tree::new();
        t.proj("f", "before");
        let run = t.run();
        t.up("f", "command");
        let _clear = NoRace;
        let target = t.project.join("f");
        race(move |path, moment| match moment {
            _ if path != Path::new("f") => {}
            // The check no longer holds once the swap is made ...
            Moment::Commit => save(&target, "owner saved"),
            // ... and the names change again just before the undo.
            Moment::Undo => save(&target, "owner again"),
            Moment::Unlink => {}
        });
        let r = t.apply(&run);
        // The undo put "owner again" at the temp name. It isn't ours, so it stays.
        assert_eq!(t.read("f"), "owner saved");
        let temp = aside(&t.project, "f");
        assert_eq!(std::fs::read_to_string(&temp).unwrap(), "owner again");
        let temp = temp.file_name().unwrap().to_str().unwrap().to_string();
        assert_eq!(paths(&r.conflicted), [temp.as_str(), "f"]);
        assert!(r.applied.is_empty());
        assert!(r.to_string().contains("moved aside and couldn't put back"));
    }

    #[test]
    fn apply_keeps_a_folder_swapped_in_before_a_folder_delete() {
        let t = Tree::new();
        t.proj("d/old", "old");
        let run = t.run();
        if !whiteout(&t.upper.join("d")) {
            return;
        }
        let _clear = NoRace;
        let project = t.project.clone();
        race(move |path, moment| {
            if path == Path::new("d") && moment == Moment::Commit {
                std::fs::rename(project.join("d"), project.join("d_moved")).unwrap();
                std::fs::create_dir(project.join("d")).unwrap();
            }
        });
        let r = t.apply(&run);
        assert_eq!(paths(&r.deleted), ["d/old"]);
        assert_eq!(paths(&r.conflicted), ["d"]);
        assert_eq!(t.listing(), ["d", "d_moved"]);
    }

    #[test]
    fn apply_puts_a_folder_back_when_it_cannot_be_removed() {
        let t = Tree::new();
        t.proj("d/old", "old");
        let run = t.run();
        if !whiteout(&t.upper.join("d")) {
            return;
        }
        let _clear = NoRace;
        let project = t.project.clone();
        race(move |path, moment| {
            if path == Path::new("d") && moment == Moment::Unlink {
                // Something lands in it after it was moved aside: the rmdir fails.
                std::fs::write(aside(&project, "d").join("late"), "late").unwrap();
            }
        });
        let r = t.apply(&run);
        assert_eq!(paths(&r.conflicted), ["d"]);
        assert_eq!(t.read("d/late"), "late");
        assert_eq!(t.listing(), ["d", "d/late"]);
        t.no_temp_left();
    }

    #[test]
    fn apply_never_touches_a_planted_temp_name() {
        let t = Tree::new();
        t.proj("f", "before");
        t.proj("g", "file");
        t.proj("h", "gone");
        let run = t.run();
        t.up("f", "after");
        t.up("g/inside", "in"); // a folder replaces the file g
        if !whiteout(&t.upper.join("h")) {
            return;
        }
        let _clear = NoRace;
        std::fs::write(planted(&t.project, "f", 1), "planted").unwrap();
        std::fs::create_dir(planted(&t.project, "g", 3)).unwrap(); // an empty owner folder
        std::fs::write(planted(&t.project, "h", 5), "planted").unwrap();
        // f's temp file, g's temp folder, g/inside's temp file, then h's move aside.
        suffixes(&[1, 2, 3, 4, 7, 5, 6]);
        let r = t.apply(&run);
        assert_eq!(paths(&r.applied), ["f", "g", "g/inside"], "{r:?}");
        assert_eq!(paths(&r.deleted), ["h"]);
        assert!(r.refused.is_empty() && r.conflicted.is_empty(), "{r:?}");
        assert_eq!(t.read("f"), "after");
        assert_eq!(
            std::fs::read_to_string(planted(&t.project, "f", 1)).unwrap(),
            "planted"
        );
        assert!(planted(&t.project, "g", 3).is_dir());
        assert_eq!(
            std::fs::read_to_string(planted(&t.project, "h", 5)).unwrap(),
            "planted"
        );
    }

    #[test]
    fn apply_gives_up_after_eight_taken_temp_names() {
        let t = Tree::new();
        t.proj("k", "before");
        let run = t.run();
        t.up("k", "after");
        let _clear = NoRace;
        std::fs::write(planted(&t.project, "k", 7), "planted").unwrap();
        suffixes(&[7; 8]);
        let r = t.apply(&run);
        assert_eq!(refusals(&r), [("k", Io)]);
        assert_eq!(t.read("k"), "before");
        assert_eq!(
            std::fs::read_to_string(planted(&t.project, "k", 7)).unwrap(),
            "planted"
        );
    }

    #[test]
    fn apply_refuses_to_clear_a_folder_it_cannot_list_in_full() {
        let t = Tree::new();
        for n in 0..5 {
            t.proj(&format!("d/f{n}"), "old");
        }
        let run = t.run();
        t.up("d/new", "new");
        if !xattr(&t.upper.join("d"), "user.overlay.opaque", b"y") {
            return;
        }
        // The limit is three; d holds six entries by the time it is cleared.
        let r = t.apply_with(&run, &[], 3);
        assert_eq!(refusals(&r), [("d", TooMany)]);
        assert!(r.deleted.is_empty());
        assert_eq!(t.listing().len(), 7);
    }

    #[test]
    fn apply_changes_nothing_when_the_entry_limit_is_passed_last() {
        let t = Tree::new();
        t.proj("keep", "k");
        let run = t.run();
        for name in ["a", "b", "c", "d"] {
            t.up(name, name);
        }
        t.up("keep", "changed"); // the fifth entry, and the limit is four
        let r = t.apply_with(&run, &[], 4);
        assert!(r.applied.is_empty() && r.deleted.is_empty(), "{r:?}");
        assert!(!r.refused.is_empty() && r.refused.first.iter().all(|(_, why)| *why == TooMany));
        assert_eq!(t.listing(), ["keep"]);
        assert_eq!(t.read("keep"), "k");
    }

    #[test]
    fn apply_changes_nothing_when_overlay_metadata_comes_last() {
        for (name, is_dir, key) in [
            ("zz", true, "user.overlay.redirect"),
            ("zz", false, "user.overlay.metacopy"),
            ("zz", true, "user.overlay.opaque"), // `x` is not an opaque folder
            (".zz_git", true, "user.overlay.redirect"),
        ] {
            let t = Tree::new();
            t.proj("keep", "k");
            let run = t.run();
            t.up("a", "a");
            t.up("keep", "changed");
            // A protected name is still checked for overlay metadata first.
            let (staged, name) = if name == ".zz_git" {
                (t.up_dir("zz/.git"), "zz/.git")
            } else if is_dir {
                (t.up_dir(name), name)
            } else {
                (t.up(name, "z"), name)
            };
            let value: &[u8] = if key.ends_with("opaque") {
                b"x"
            } else {
                b"/elsewhere"
            };
            if !xattr(&staged, key, value) {
                return;
            }
            let r = t.apply(&run);
            assert!(r.applied.is_empty() && r.deleted.is_empty(), "{key}: {r:?}");
            let mut expected = vec![("a", UnsupportedOverlay), ("keep", UnsupportedOverlay)];
            expected.push((name, UnsupportedOverlay));
            if name == "zz/.git" {
                expected.push(("zz", UnsupportedOverlay));
            }
            expected.sort_by_key(|(path, _)| *path);
            assert_eq!(refusals(&r), expected, "{key}");
            assert_eq!(t.listing(), ["keep"], "{key}");
            assert_eq!(t.read("keep"), "k");
        }
    }

    #[test]
    fn report_display_is_capped_and_plain() {
        let mut report = Report {
            applied: (0..250)
                .map(|i| PathBuf::from(format!("f{i:03}")))
                .collect(),
            refused: [
                (PathBuf::from(".git"), ProtectedName),
                (
                    PathBuf::from("evil\nWorkspace changes: 0 applied"),
                    LinkOutside,
                ),
            ]
            .into_iter()
            .collect(),
            held_back: [PathBuf::from("src")].into_iter().collect(),
            ..Report::default()
        };
        report.conflict(Path::new("mine.txt"));
        // Only the first 50 of each list are kept; the rest are counted.
        assert_eq!(
            (report.applied.total, report.applied.first.len()),
            (250, 50)
        );
        let text = report.to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "Workspace changes: 250 applied, 0 deleted, 2 refused, 1 conflicted, 1 held back \
             (not deleted because other changes were refused or conflicted)"
        );
        // What needs the owner comes first, then what was applied.
        assert_eq!(lines[1], "Refused:");
        assert_eq!(lines[2], "  .git (a protected name such as .git or .ssh)");
        // A name can't fake a line of the report.
        assert_eq!(
            lines[3],
            r"  evil\u{a}Workspace changes: 0 applied (a link that leads outside the project, or a path through a link)"
        );
        assert_eq!(
            lines[4..8],
            [
                "Conflicted, so Blackwall kept your version:",
                "  mine.txt",
                "Held back:",
                "  src"
            ]
        );
        assert_eq!(lines[8], "Applied:");
        assert_eq!(lines[9], "  f000");
        assert_eq!(lines[58], "  f049");
        assert_eq!(lines[59], "  … and 200 more");
        assert_eq!(lines.len(), 60);
        assert!(!text.contains("Deleted"));
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.starts_with("Workspace changes:"))
                .count(),
            1
        );
        // Separators, controls and every format character (Cf) are escaped.
        for c in [
            '\u{2028}',
            '\u{2029}',
            '\u{061c}',
            '\u{feff}',
            '\u{2060}',
            '\u{2064}',
            '\u{202a}',
            '\u{202e}',
            '\u{2066}',
            '\u{2069}',
            '\u{200b}',
            '\u{7f}',
            '\u{ad}',
            '\u{600}',
            '\u{6dd}',
            '\u{180e}',
            '\u{206f}',
            '\u{fffb}',
            '\u{110bd}',
            '\u{1bca3}',
            '\u{1d17a}',
            '\u{e0001}',
            '\u{e0041}',
            '\u{e007f}',
        ] {
            let name = PathBuf::from(format!("a{c}b"));
            assert_eq!(shown(&name), format!("a{}b", c.escape_unicode()), "{c:?}");
        }
        // Bytes that aren't UTF-8 are shown one by one, and a backslash is escaped too, so
        // different names never look the same.
        let raw = |bytes: &[u8]| shown(Path::new(OsStr::from_bytes(bytes)));
        assert_eq!(raw(b".g\xffit"), r".g\xffit");
        assert_eq!(raw(b".g\xfe\xffit"), r".g\xfe\xffit");
        assert_eq!(raw(br".g\xffit"), r".g\\xffit");
        assert_eq!(raw("caf\u{e9}/\u{1f600}".as_bytes()), "caf\u{e9}/\u{1f600}");
        // A long path loses its middle, so its start and its name both show.
        let long = format!("{}/{}/name.txt", "a".repeat(300), "b".repeat(300));
        let clipped = shown(Path::new(&long));
        assert_eq!(clipped.chars().count(), SHOWN_CHARS);
        assert!(clipped.starts_with("aaa") && clipped.ends_with("bbb/name.txt"));
        assert!(clipped.contains('…'));
    }

    #[test]
    fn report_keeps_the_stranded_notice_for_a_conflict_it_does_not_list() {
        let mut report = Report::default();
        for i in 0..LIST_LIMIT {
            report.conflict(&PathBuf::from(format!("a{i:02}")));
        }
        report.conflict(Path::new("z/.f.blackwall-0000000000000001"));
        let text = report.sorted().to_string();
        assert!(text.contains("  … and 1 more"));
        assert!(text.contains(STRANDED));
    }

    /// A chain of `depth` folders with 255-byte names in `base`, made one level at a time from
    /// the folder above (the whole path is far longer than PATH_MAX), with `files` empty files in
    /// the deepest.
    fn deep_chain(base: &Path, depth: usize, files: usize) {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let mut dir = fs::open(base, flags, Mode::empty()).unwrap();
        for level in 0..depth {
            let name = format!("{level:03}{}", "d".repeat(252));
            fs::mkdirat(&dir, name.as_str(), Mode::from_raw_mode(0o755)).unwrap();
            dir = fs::openat(&dir, name.as_str(), flags, Mode::empty()).unwrap();
        }
        let create = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
        for n in 0..files {
            fs::openat(
                &dir,
                format!("f{n}").as_str(),
                create,
                Mode::from_raw_mode(0o644),
            )
            .unwrap();
        }
    }

    #[test]
    fn apply_report_stays_small_for_a_deep_tree_of_long_names() {
        let t = Tree::new();
        let run = t.run();
        // About 51 KB a path at the bottom: unclipped, 200 of them would be 5 MB of report.
        deep_chain(&t.upper, 200, 3);
        let r = t.apply(&run);
        assert_eq!((r.applied.total, r.applied.first.len()), (200, LIST_LIMIT));
        let kept: usize = [&r.applied, &r.deleted, &r.conflicted, &r.held_back]
            .iter()
            .map(|list| list.first.len())
            .sum::<usize>()
            + r.refused.first.len();
        assert!(kept <= 5 * LIST_LIMIT);
        let text = r.to_string();
        for line in text.lines().skip(1) {
            let path = line
                .strip_prefix("  ")
                .map(|line| line.split(" (").next().unwrap_or(line));
            assert!(
                path.is_none_or(|path| path.chars().count() <= SHOWN_CHARS),
                "{line}"
            );
        }
        assert!(text.len() < 5 * (LIST_LIMIT + 2) * 300, "{}", text.len());
        assert!(text.contains("  … and 150 more"));
    }

    #[test]
    fn apply_counts_past_the_entry_limit_without_keeping_every_path() {
        // On disk, not tmpfs: half a million inodes.
        let t = Tree::on_disk();
        let run = t.run();
        let upper = fs::open(&t.upper, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap();
        let create = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
        for n in 0..=MAX_ENTRIES {
            fs::openat(
                &upper,
                format!("f{n:06}").as_str(),
                create,
                Mode::from_raw_mode(0o644),
            )
            .unwrap();
        }
        let r = t.apply(&run);
        assert!(r.applied.is_empty() && r.deleted.is_empty());
        assert_eq!(r.refused.total, MAX_ENTRIES + 1);
        assert_eq!(r.refused.first.len(), LIST_LIMIT);
        assert!(r.refused.first.iter().all(|(_, why)| *why == TooMany));
        let text = r.to_string();
        assert_eq!(text.lines().count(), 1 + 1 + LIST_LIMIT + 1);
        assert!(text.ends_with(&format!("  … and {} more", MAX_ENTRIES + 1 - LIST_LIMIT)));
        assert!(text.len() < 4096, "{}", text.len());
        assert!(t.listing().is_empty());
    }

    #[test]
    fn apply_keeps_the_staged_modification_and_access_times() {
        let t = Tree::new();
        t.proj("out.o", "old");
        let run = t.run();
        let at = |secs: u64, nanos: u32| UNIX_EPOCH + Duration::new(secs, nanos);
        // As a build leaves them: the source older than the output built from it.
        let staged = [
            (
                "src.c",
                at(1_000_000_000, 123_456_789),
                at(1_000_000_100, 1),
            ),
            (
                "out.o",
                at(1_000_000_500, 987_654_321),
                at(1_000_000_600, 2),
            ),
            ("new/deep.o", at(1_000_000_700, 5), at(1_000_000_800, 3)),
        ];
        for (name, modified, accessed) in staged {
            let file = std::fs::File::options()
                .write(true)
                .open(t.up(name, name))
                .unwrap();
            let times = std::fs::FileTimes::new()
                .set_modified(modified)
                .set_accessed(accessed);
            file.set_times(times).unwrap();
        }
        let r = t.apply(&run);
        assert_eq!(paths(&r.applied), ["new", "new/deep.o", "out.o", "src.c"]);
        for (name, modified, accessed) in staged {
            let meta = std::fs::symlink_metadata(t.project.join(name)).unwrap();
            assert_eq!(meta.modified().unwrap(), modified, "{name}");
            assert_eq!(meta.accessed().unwrap(), accessed, "{name}");
        }
        // A second build sees the output still newer than its source: nothing to redo.
        let mtime = |name: &str| t.project.join(name).metadata().unwrap().modified().unwrap();
        assert!(mtime("out.o") > mtime("src.c"));
        assert_eq!(t.read("out.o"), "out.o");
    }

    #[test]
    fn apply_gives_new_entries_the_mode_a_direct_write_would() {
        let t = Tree::new();
        t.proj("shared.txt", "old");
        chmod(&t.project.join("shared.txt"), 0o664);
        let run = t.run();
        chmod(&t.up("new.txt", "new"), 0o666);
        chmod(&t.up("tool", "#!/bin/sh"), 0o777);
        t.up("dir/inside", "x");
        chmod(&t.upper.join("dir"), 0o777);
        chmod(&t.up("shared.txt", "changed"), 0o664);
        // `apply_with` uses umask 022.
        let r = t.apply(&run);
        assert_eq!(
            paths(&r.applied),
            ["dir", "dir/inside", "new.txt", "shared.txt", "tool"]
        );
        assert_eq!(mode(&t.project.join("new.txt")), 0o644);
        assert_eq!(mode(&t.project.join("tool")), 0o755);
        assert_eq!(mode(&t.project.join("dir")), 0o755);
        // A file that was there keeps the mode it had (the overlay copied it up).
        assert_eq!(mode(&t.project.join("shared.txt")), 0o664);
    }

    #[test]
    fn umask_is_read_from_the_process_status() {
        let status = "Name:\tblackwall\nUmask:\t0027\nState:\tR (running)\n";
        assert_eq!(umask_in(status), Some(0o027));
        assert_eq!(umask_in("Name:\tx\n"), None);
        assert_eq!(umask_in("Umask:\tzz\n"), None);
        assert!(process_umask() <= 0o777);
    }
}
