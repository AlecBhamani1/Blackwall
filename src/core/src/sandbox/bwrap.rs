//! The bubblewrap command line for a sandboxed Linux command, built as plain data.
//!
//! The command is split in two:
//! - [`args`] gives the bubblewrap options: namespaces, seccomp, every mount and `--chdir`. They
//!   go into a NUL-separated file ([`args_file`]) that bubblewrap reads with `--args 4`, so there
//!   is no length limit and no quoting.
//! - [`launcher`] puts `--args 4 -- <program> <program args>` on the real command line. The
//!   program can't go inside the `--args` file: bubblewrap 0.13.1 only takes it from its own argv.
//!
//! What the sandbox sees: the system roots read-only (merged-/usr links recreated as symlinks), a
//! short list of `/etc` entries, a private `/dev`, `/proc` and `/tmp`, empty parent folders down
//! to the project, and the project itself read-only with every protected name masked. Locally
//! managed configuration under the system roots (`/usr/local/etc`, Linuxbrew's `etc`) is hidden
//! behind an empty read-only tmpfs. The project
//! is mounted from a descriptor (`--ro-bind-fd`) for the folder that was checked, not by its path,
//! so swapping the folder at that path after the check changes nothing. No folder
//! under `/run` or `/var/run` is ever mounted, because host pathname sockets live there (the user
//! session bus, docker.sock); the one thing from there that can appear is a single regular `/etc`
//! file reached through a link, such as `resolv.conf` (see [`etc_at`]). The home folder, the data
//! folder and the host root are not mounted.
//!
//! Masks: a directory becomes an empty read-only tmpfs; a file becomes a read-only bind of
//! `/dev/null`. bubblewrap mounts binds `nodev`, so opening a masked file fails with "permission
//! denied" (EACCES) rather than reading as empty. Either way its content can't be read.

use super::walk::{Mask, MaskKind};
use std::ffi::{OsStr, OsString};
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub(crate) const SYSTEM_ROOTS: [&str; 6] = [
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/home/linuxbrew/.linuxbrew",
];
pub(crate) const ETC_FILES: [&str; 12] = [
    "ld.so.cache",
    "ld.so.conf",
    "ld.so.conf.d",
    "ssl",
    "ca-certificates",
    "resolv.conf",
    "hosts",
    "nsswitch.conf",
    "passwd",
    "group",
    "localtime",
    "alternatives",
];
/// Locally managed configuration under the system roots, rather than the distribution's own files.
/// Each one that exists is hidden behind an empty read-only tmpfs.
pub(crate) const SYSTEM_MASKS: [&str; 2] = ["/usr/local/etc", "/home/linuxbrew/.linuxbrew/etc"];
/// Where host pathname sockets live. Nothing that resolves under these is ever mounted, with
/// one exception: a single regular file (see [`etc_at`]).
const SOCKET_DIRS: [&str; 2] = ["run", "var/run"];

/// Every profile mounts the project read-only for now, the strictest choice.
pub(crate) struct MountPlan<'a> {
    /// Where the project appears inside the sandbox: its own (canonical) path.
    pub project: &'a Path,
    /// What is mounted there: a descriptor for the checked folder, open in bubblewrap at this
    /// number (see `linux::open_project` and `linux::spawn_with`).
    pub project_fd: RawFd,
    pub network: bool,
    pub roots: Vec<Root>,
    /// Absolute `/etc/<name>` paths that exist on the host.
    pub etc: Vec<PathBuf>,
    /// Absolute paths from [`SYSTEM_MASKS`] to hide.
    pub system_masks: Vec<PathBuf>,
    /// Relative to `project`.
    pub masks: &'a [Mask],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Root {
    Bind(PathBuf),
    /// A merged-/usr link such as `/bin -> usr/bin`, recreated inside the sandbox.
    Symlink {
        target: PathBuf,
        link: PathBuf,
    },
}

/// The system roots on this host.
pub(crate) fn host_roots() -> Vec<Root> {
    roots_at(Path::new("/"))
}

/// The `/etc` entries on this host.
pub(crate) fn host_etc() -> Vec<PathBuf> {
    etc_at(Path::new("/"))
}

/// The [`SYSTEM_MASKS`] on this host, for `roots`.
pub(crate) fn host_system_masks(roots: &[Root]) -> Vec<PathBuf> {
    system_masks_at(Path::new("/"), roots)
}

/// Each of [`SYSTEM_MASKS`] that is a real directory under `base`, inside one of the bound
/// `roots`. A symlink in its place is left alone, as is anything under a root that is a symlink
/// (and so not mounted).
pub(crate) fn system_masks_at(base: &Path, roots: &[Root]) -> Vec<PathBuf> {
    SYSTEM_MASKS
        .iter()
        .map(PathBuf::from)
        .filter(|mask| {
            roots
                .iter()
                .any(|root| matches!(root, Root::Bind(bound) if mask.starts_with(bound)))
                && std::fs::symlink_metadata(base.join(relative(&mask.to_string_lossy())))
                    .is_ok_and(|meta| meta.is_dir())
        })
        .collect()
}

/// Each of [`SYSTEM_ROOTS`] under `base`, as the sandbox should see it at the same absolute path:
/// a symlink is recreated with its own target (never followed), a directory is bound, and a
/// missing or other entry, or a directory that resolves under `/run`, is skipped.
pub(crate) fn roots_at(base: &Path) -> Vec<Root> {
    let socket_dirs = socket_dirs(base);
    let mut roots = Vec::new();
    for root in SYSTEM_ROOTS {
        let host = base.join(relative(root));
        let Ok(meta) = std::fs::symlink_metadata(&host) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            if let Ok(target) = std::fs::read_link(&host) {
                roots.push(Root::Symlink {
                    target,
                    link: root.into(),
                });
            }
        } else if meta.is_dir() && !resolves_under(&host, &socket_dirs) {
            roots.push(Root::Bind(root.into()));
        }
    }
    roots
}

/// Each of [`ETC_FILES`] under `base/etc` that is a regular file or a directory (following
/// symlinks, as the bind will). A directory that resolves under `/run` or `/var/run` is skipped,
/// and so is anything else (a socket, a FIFO, a device, a broken link). A regular file is kept
/// even there: `/etc/resolv.conf` is usually a link to `/run/systemd/resolve/stub-resolv.conf`,
/// and one regular file exposes only its own content, never a socket.
pub(crate) fn etc_at(base: &Path) -> Vec<PathBuf> {
    let socket_dirs = socket_dirs(base);
    ETC_FILES
        .iter()
        .filter(|name| {
            let host = base.join("etc").join(name);
            match std::fs::metadata(&host) {
                Ok(meta) => {
                    meta.is_file() || (meta.is_dir() && !resolves_under(&host, &socket_dirs))
                }
                Err(_) => false,
            }
        })
        .map(|name| Path::new("/etc").join(name))
        .collect()
}

fn relative(path: &str) -> &str {
    path.trim_start_matches('/')
}

/// `base/run` and `base/var/run`, resolved (on most systems `/var/run` is a link to `/run`).
fn socket_dirs(base: &Path) -> Vec<PathBuf> {
    SOCKET_DIRS
        .iter()
        .map(|dir| {
            let path = base.join(dir);
            std::fs::canonicalize(&path).unwrap_or(path)
        })
        .collect()
}

/// True when `path` resolves inside one of `dirs`, or can't be resolved (fail closed).
fn resolves_under(path: &Path, dirs: &[PathBuf]) -> bool {
    match std::fs::canonicalize(path) {
        Ok(real) => dirs.iter().any(|dir| real.starts_with(dir)),
        Err(_) => true,
    }
}

/// The bubblewrap options for `plan`, in the pinned order. The program is not included; see
/// [`launcher`].
pub(crate) fn args(plan: &MountPlan) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |items: &[&OsStr]| a.extend(items.iter().map(|s| s.to_os_string()));
    let s = |s: &'static str| OsStr::new(s);

    push(&[s("--unshare-all")]);
    if plan.network {
        push(&[s("--share-net")]);
    }
    push(&[
        s("--die-with-parent"),
        s("--new-session"),
        // No user namespaces inside, so no nested sandbox, mount or capability games. The
        // AppImage has no AppArmor child profile to forbid them. --disable-userns needs an
        // explicit --unshare-user: --unshare-all only tries one, and skips it when run as root.
        s("--unshare-user"),
        s("--disable-userns"),
        s("--cap-drop"),
        s("ALL"),
        s("--seccomp"),
        s("3"),
    ]);
    for root in &plan.roots {
        match root {
            Root::Bind(path) => push(&[s("--ro-bind"), path.as_os_str(), path.as_os_str()]),
            Root::Symlink { target, link } => {
                push(&[s("--symlink"), target.as_os_str(), link.as_os_str()]);
            }
        }
    }
    for mask in &plan.system_masks {
        let mask = mask.as_os_str();
        push(&[s("--tmpfs"), mask, s("--remount-ro"), mask]);
    }
    push(&[s("--dir"), s("/etc")]);
    for etc in &plan.etc {
        push(&[s("--ro-bind"), etc.as_os_str(), etc.as_os_str()]);
    }
    push(&[
        s("--dev"),
        s("/dev"),
        s("--proc"),
        s("/proc"),
        s("--tmpfs"),
        s("/tmp"),
    ]);
    let bound: Vec<&Path> = plan
        .roots
        .iter()
        .filter_map(|root| match root {
            Root::Bind(path) => Some(path.as_path()),
            Root::Symlink { .. } => None,
        })
        .collect();
    let mut ancestors: Vec<&Path> = plan
        .project
        .ancestors()
        .skip(1)
        .filter(|dir| dir.parent().is_some()) // not "/"
        .filter(|dir| !bound.iter().any(|root| dir.starts_with(root)))
        .collect();
    ancestors.reverse();
    for dir in ancestors {
        push(&[s("--dir"), dir.as_os_str()]);
    }
    let project = plan.project.as_os_str();
    let project_fd = OsString::from(plan.project_fd.to_string());
    push(&[s("--ro-bind-fd"), &project_fd, project]);
    for mask in plan.masks {
        let path = plan.project.join(&mask.path);
        let path = path.as_os_str();
        match mask.kind {
            MaskKind::Dir => push(&[s("--tmpfs"), path, s("--remount-ro"), path]),
            MaskKind::File => push(&[s("--ro-bind"), s("/dev/null"), path]),
        }
    }
    push(&[s("--remount-ro"), s("/"), s("--chdir"), project]);
    a
}

/// The `--args` file content: each argument followed by a NUL. `None` if an argument itself
/// contains a NUL, which would split it into two options.
pub(crate) fn args_file(args: &[OsString]) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    for arg in args {
        let arg = arg.as_bytes();
        if arg.contains(&0) {
            return None;
        }
        bytes.extend_from_slice(arg);
        bytes.push(0);
    }
    Some(bytes)
}

/// `None` unless `bwrap`, `seccomp_file` and `args_file` are all absolute: bubblewrap is never
/// found through PATH, and the shell runs in `/`.
///
/// `/bin/sh` opens the seccomp file on fd 3 and the args file on fd 4, then execs bubblewrap:
/// `/bin/sh -c 'exec 3<"$1" 4<"$2" && shift 2 && exec "$@"' blackwall-sandbox <seccomp_file>
/// <args_file> <bwrap> --args 4 -- <program> <program args>`.
///
/// The environment starts empty, so bubblewrap (which is pid 1 in the sandbox, and whose
/// `/proc/1/environ` the command can read) never carries Blackwall's own variables. The caller
/// sets the allow-listed `ChildEnv` on the returned command. The shell runs in `/`, because it
/// exports `PWD` (and bash also `SHLVL` and `_`) to bubblewrap and so to the command; from `/`
/// that reveals nothing about the host. The command itself starts in the project (`--chdir`).
pub(crate) fn launcher(
    bwrap: &Path,
    seccomp_file: &Path,
    args_file: &Path,
    program: &OsStr,
    program_args: &[OsString],
) -> Option<tokio::process::Command> {
    if ![bwrap, seccomp_file, args_file]
        .iter()
        .all(|path| path.is_absolute())
    {
        return None;
    }
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .env_clear()
        .current_dir("/")
        .arg("-c")
        .arg(r#"exec 3<"$1" 4<"$2" && shift 2 && exec "$@""#)
        .arg("blackwall-sandbox")
        .arg(seccomp_file)
        .arg(args_file)
        .arg(bwrap)
        .args(["--args", "4", "--"])
        .arg(program)
        .args(program_args);
    Some(command)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("blackwall-bwrap-{}", rand::random::<u64>()));
            std::fs::create_dir(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mask(path: &str, kind: MaskKind) -> Mask {
        Mask {
            path: path.into(),
            kind,
        }
    }
    fn plan<'a>(project: &'a Path, network: bool, masks: &'a [Mask]) -> MountPlan<'a> {
        MountPlan {
            project,
            project_fd: 10,
            network,
            roots: vec![
                Root::Bind("/usr".into()),
                Root::Symlink {
                    target: "usr/bin".into(),
                    link: "/bin".into(),
                },
                Root::Bind("/home/linuxbrew/.linuxbrew".into()),
            ],
            etc: vec!["/etc/hosts".into(), "/etc/ssl".into()],
            system_masks: Vec::new(),
            masks,
        }
    }
    fn strs(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_str().unwrap().to_owned())
            .collect()
    }
    /// Index of the space-separated run `needle` in `args`.
    fn pos(args: &[String], needle: &str) -> usize {
        let needle: Vec<&str> = needle.split(' ').collect();
        args.windows(needle.len())
            .position(|w| w == needle.as_slice())
            .unwrap_or_else(|| panic!("{needle:?} not in {args:?}"))
    }

    #[test]
    fn read_only_args_have_no_host_root_no_home_and_mask_after_project() {
        let masks = [mask(".git", MaskKind::Dir), mask("x/.ssh", MaskKind::File)];
        let project = Path::new("/home/u/p");
        let a = strs(&args(&plan(project, false, &masks)));
        assert!(!a.windows(3).any(|w| w == ["--ro-bind", "/", "/"]));
        assert!(!a
            .iter()
            .any(|x| x == "--share-net" || x == "--bind" || x == "--dev-bind"));
        assert!(!a.windows(2).any(|w| w[1] == "/home/u" && w[0] != "--dir"));
        assert_eq!(
            a[..10],
            [
                "--unshare-all",
                "--die-with-parent",
                "--new-session",
                "--unshare-user",
                "--disable-userns",
                "--cap-drop",
                "ALL",
                "--seccomp",
                "3",
                "--ro-bind"
            ]
        );
        assert!(pos(&a, "--ro-bind /usr /usr") < pos(&a, "--symlink usr/bin /bin"));
        assert!(pos(&a, "--symlink usr/bin /bin") < pos(&a, "--dir /etc"));
        assert!(pos(&a, "--dir /etc") < pos(&a, "--ro-bind /etc/hosts /etc/hosts"));
        assert!(
            pos(&a, "--ro-bind /etc/ssl /etc/ssl")
                < pos(&a, "--dev /dev --proc /proc --tmpfs /tmp")
        );
        assert!(pos(&a, "--tmpfs /tmp") < pos(&a, "--dir /home/u"));
        // /home sits above the linuxbrew root but isn't under it, so it still gets a --dir.
        assert!(pos(&a, "--dir /home") < pos(&a, "--dir /home/u"));
        assert!(pos(&a, "--dir /home/u") < pos(&a, "--ro-bind-fd 10 /home/u/p"));
        assert!(pos(&a, "--ro-bind-fd 10 /home/u/p") < pos(&a, "--tmpfs /home/u/p/.git"));
        pos(&a, "--tmpfs /home/u/p/.git --remount-ro /home/u/p/.git");
        assert!(
            pos(&a, "--remount-ro /home/u/p/.git")
                < pos(&a, "--ro-bind /dev/null /home/u/p/x/.ssh")
        );
        assert!(a.ends_with(&[
            "--remount-ro".into(),
            "/".into(),
            "--chdir".into(),
            "/home/u/p".into()
        ]));
        // The only bare "/" is the final remount.
        assert_eq!(a.iter().filter(|x| *x == "/").count(), 1);
    }

    #[test]
    fn project_ancestors_inside_a_bound_root_get_no_dir() {
        let a = strs(&args(&plan(Path::new("/usr/local/src/p"), false, &[])));
        for dir in ["/usr", "/usr/local", "/usr/local/src"] {
            assert!(!a.windows(2).any(|w| w == ["--dir", dir]), "{dir}");
        }
        pos(&a, "--tmpfs /tmp --ro-bind-fd 10 /usr/local/src/p");
    }

    #[test]
    fn the_project_is_mounted_from_its_descriptor_never_its_path() {
        let a = strs(&args(&plan(Path::new("/home/u/p"), false, &[])));
        assert_eq!(
            a.windows(3)
                .filter(|w| w[0].starts_with("--ro-bind") && w[2] == "/home/u/p")
                .collect::<Vec<_>>(),
            [["--ro-bind-fd", "10", "/home/u/p"]]
        );
        assert!(!a
            .windows(2)
            .any(|w| w[0] == "--ro-bind" && w[1] == "/home/u/p"));
    }

    #[test]
    fn local_configuration_is_masked_after_the_roots_and_before_the_root_remount() {
        let mut masked = plan(Path::new("/home/u/p"), false, &[]);
        masked.system_masks = vec![
            "/usr/local/etc".into(),
            "/home/linuxbrew/.linuxbrew/etc".into(),
        ];
        let a = strs(&args(&masked));
        let usr = pos(&a, "--tmpfs /usr/local/etc --remount-ro /usr/local/etc");
        let brew = pos(
            &a,
            "--tmpfs /home/linuxbrew/.linuxbrew/etc --remount-ro /home/linuxbrew/.linuxbrew/etc",
        );
        assert!(pos(&a, "--ro-bind /usr /usr") < usr);
        assert!(
            pos(
                &a,
                "--ro-bind /home/linuxbrew/.linuxbrew /home/linuxbrew/.linuxbrew"
            ) < usr
        );
        assert!(usr < brew);
        assert!(brew < pos(&a, "--dir /etc"));
        assert!(brew < pos(&a, "--remount-ro / --chdir"));
    }

    #[test]
    fn only_real_configuration_folders_under_bound_roots_are_masked() {
        let base = Temp::new();
        let b = &base.0;
        std::fs::create_dir_all(b.join("usr/local/etc")).unwrap();
        std::fs::create_dir_all(b.join("brew/etc")).unwrap();
        std::fs::create_dir_all(b.join("home/linuxbrew")).unwrap();
        // Linuxbrew through a link isn't bound, so there is nothing of it to mask.
        symlink("../../brew", b.join("home/linuxbrew/.linuxbrew")).unwrap();
        assert_eq!(
            system_masks_at(b, &roots_at(b)),
            [PathBuf::from("/usr/local/etc")]
        );
        std::fs::remove_file(b.join("home/linuxbrew/.linuxbrew")).unwrap();
        std::fs::create_dir_all(b.join("home/linuxbrew/.linuxbrew/etc")).unwrap();
        assert_eq!(
            system_masks_at(b, &roots_at(b)),
            ["/usr/local/etc", "/home/linuxbrew/.linuxbrew/etc"].map(PathBuf::from)
        );
        // A missing folder, or a link in its place, is left alone.
        std::fs::remove_dir(b.join("usr/local/etc")).unwrap();
        symlink("../share", b.join("usr/local/etc")).unwrap();
        std::fs::remove_dir(b.join("home/linuxbrew/.linuxbrew/etc")).unwrap();
        assert!(system_masks_at(b, &roots_at(b)).is_empty());
    }

    #[test]
    fn network_toggle_only_adds_share_net() {
        let masks = [mask(".git", MaskKind::Dir)];
        let project = Path::new("/home/u/p");
        let off = strs(&args(&plan(project, false, &masks)));
        let on = strs(&args(&plan(project, true, &masks)));
        assert_eq!(on.len(), off.len() + 1);
        assert_eq!(on[1], "--share-net");
        let mut without = on.clone();
        without.remove(1);
        assert_eq!(without, off);
    }

    #[test]
    fn merged_usr_roots_become_symlinks_and_missing_roots_are_skipped() {
        let base = Temp::new();
        let b = &base.0;
        std::fs::create_dir_all(b.join("usr/bin")).unwrap();
        std::fs::create_dir_all(b.join("usr/lib")).unwrap();
        std::fs::create_dir_all(b.join("sbin")).unwrap();
        symlink("usr/bin", b.join("bin")).unwrap();
        symlink("usr/lib", b.join("lib")).unwrap();
        // lib64 is missing; linuxbrew is a plain file, not a directory.
        std::fs::create_dir_all(b.join("home/linuxbrew")).unwrap();
        std::fs::write(b.join("home/linuxbrew/.linuxbrew"), "").unwrap();
        assert_eq!(
            roots_at(b),
            [
                Root::Bind("/usr".into()),
                Root::Symlink {
                    target: "usr/bin".into(),
                    link: "/bin".into()
                },
                Root::Bind("/sbin".into()),
                Root::Symlink {
                    target: "usr/lib".into(),
                    link: "/lib".into()
                },
            ]
        );
        // A symlink is recreated even when it dangles (it reveals nothing), but a directory
        // reached through a link into /run is never bound.
        std::fs::create_dir_all(b.join("run/brew")).unwrap();
        std::fs::remove_file(b.join("home/linuxbrew/.linuxbrew")).unwrap();
        std::fs::remove_dir(b.join("home/linuxbrew")).unwrap();
        symlink("../run", b.join("home/linuxbrew")).unwrap();
        std::fs::create_dir(b.join("run/.linuxbrew")).unwrap();
        assert!(!roots_at(b).contains(&Root::Bind("/home/linuxbrew/.linuxbrew".into())));
        symlink("missing/lib64", b.join("lib64")).unwrap();
        assert!(roots_at(b).contains(&Root::Symlink {
            target: "missing/lib64".into(),
            link: "/lib64".into()
        }));
    }

    #[test]
    fn etc_keeps_files_and_dirs_but_nothing_else_from_run() {
        let base = Temp::new();
        let b = &base.0;
        std::fs::create_dir_all(b.join("etc/ssl")).unwrap();
        std::fs::create_dir_all(b.join("run/resolve")).unwrap();
        std::fs::create_dir_all(b.join("run/dir")).unwrap();
        std::fs::create_dir_all(b.join("var")).unwrap();
        symlink("../run", b.join("var/run")).unwrap();
        std::fs::write(
            b.join("run/resolve/stub-resolv.conf"),
            "nameserver 127.0.0.53\n",
        )
        .unwrap();
        std::fs::write(b.join("etc/hosts"), "").unwrap();
        // resolv.conf → a regular file under /run: kept. alternatives → a directory under
        // /var/run: skipped. passwd → missing: skipped. group → a socket: skipped.
        symlink("../run/resolve/stub-resolv.conf", b.join("etc/resolv.conf")).unwrap();
        symlink("../var/run/dir", b.join("etc/alternatives")).unwrap();
        symlink("../nowhere", b.join("etc/passwd")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(b.join("etc/group")).unwrap();
        assert_eq!(
            etc_at(b),
            ["/etc/ssl", "/etc/resolv.conf", "/etc/hosts"].map(PathBuf::from)
        );
    }

    #[test]
    fn no_argv_mounts_anything_under_run() {
        for list in [&SYSTEM_ROOTS[..], &ETC_FILES[..]] {
            for entry in list {
                assert!(!entry.contains("run"), "{entry}");
            }
        }
        let project = Path::new("/home/u/p");
        let masks = [mask(".git", MaskKind::Dir)];
        let host = MountPlan {
            roots: host_roots(),
            etc: host_etc(),
            system_masks: host_system_masks(&host_roots()),
            ..plan(project, true, &masks)
        };
        let a = args(&host);
        for arg in &a {
            let path = Path::new(arg);
            assert!(
                !path.starts_with("/run") && !path.starts_with("/var/run"),
                "{arg:?} in {a:?}"
            );
        }
        // The real host has /usr at least, and every root is either a symlink or a directory
        // that doesn't resolve under /run.
        assert!(host.roots.contains(&Root::Bind("/usr".into())));
        for root in &host.roots {
            if let Root::Bind(path) = root {
                let real = std::fs::canonicalize(path).unwrap();
                assert!(!real.starts_with("/run"), "{path:?} → {real:?}");
            }
        }
    }

    #[test]
    fn launcher_puts_the_program_on_the_real_command_line_with_an_empty_environment() {
        let command = launcher(
            Path::new("/opt/bwrap"),
            Path::new("/d/seccomp.bpf"),
            Path::new("/d/args"),
            OsStr::new("/bin/sh"),
            &["-c".into(), "echo hi".into()],
        )
        .unwrap();
        let std = command.as_std();
        assert_eq!(std.get_program(), "/bin/sh");
        let argv: Vec<&OsStr> = std.get_args().collect();
        assert_eq!(
            argv,
            [
                "-c",
                r#"exec 3<"$1" 4<"$2" && shift 2 && exec "$@""#,
                "blackwall-sandbox",
                "/d/seccomp.bpf",
                "/d/args",
                "/opt/bwrap",
                "--args",
                "4",
                "--",
                "/bin/sh",
                "-c",
                "echo hi"
            ]
            .map(OsStr::new)
        );
        // env_clear: nothing is inherited and nothing is set.
        assert_eq!(std.get_envs().count(), 0);
        // std's Debug output is the only public view of env_clear.
        assert!(
            format!("{std:?}").contains(" env -i \"/bin/sh\""),
            "{std:?}"
        );
        assert_eq!(std.get_current_dir(), Some(Path::new("/")));
    }

    #[test]
    fn launcher_refuses_relative_paths() {
        let abs = Path::new("/a");
        let rel = Path::new("bwrap");
        for (bwrap, seccomp, args) in [(rel, abs, abs), (abs, rel, abs), (abs, abs, rel)] {
            assert!(launcher(bwrap, seccomp, args, OsStr::new("/bin/true"), &[]).is_none());
        }
        assert!(launcher(Path::new(""), abs, abs, OsStr::new("/bin/true"), &[]).is_none());
    }

    #[test]
    fn args_file_is_nul_terminated_and_refuses_an_embedded_nul() {
        let args: Vec<OsString> = vec!["--dir".into(), "/a b".into(), "".into()];
        assert_eq!(args_file(&args).unwrap(), b"--dir\0/a b\0\0");
        use std::os::unix::ffi::OsStringExt;
        let sneaky = OsString::from_vec(b"/p/x\0--bind\0/\0/".to_vec());
        assert_eq!(args_file(&[sneaky]), None);
    }

    // ---- with the real bubblewrap ------------------------------------------------------

    struct Sandbox {
        temp: Temp,
        bwrap: PathBuf,
        project: PathBuf,
        masks: Vec<Mask>,
    }
    struct Output {
        code: Option<i32>,
        stdout: String,
        stderr: String,
    }
    impl Sandbox {
        fn new() -> Self {
            let bwrap = PathBuf::from(
                std::env::var_os("BLACKWALL_TEST_BWRAP")
                    .expect("set BLACKWALL_TEST_BWRAP to a bubblewrap binary"),
            );
            let temp = Temp::new();
            let project = temp.0.join("p");
            std::fs::create_dir_all(project.join(".git")).unwrap();
            std::fs::create_dir_all(project.join("x")).unwrap();
            std::fs::write(project.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
            std::fs::write(project.join("x/.ssh"), "secret key\n").unwrap();
            std::fs::write(project.join("README"), "hello\n").unwrap();
            let masks = super::super::walk::walk(&project, 1000).unwrap().masks;
            assert_eq!(
                masks,
                [mask(".git", MaskKind::Dir), mask("x/.ssh", MaskKind::File)]
            );
            Self {
                temp,
                bwrap,
                project,
                masks,
            }
        }

        async fn run(&self, network: bool, script: &str) -> Output {
            let run = self.temp.0.join(format!("run-{}", rand::random::<u32>()));
            std::fs::create_dir(&run).unwrap();
            std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
            let project_fd = super::super::linux::open_directory(&self.project).unwrap();
            let plan = MountPlan {
                project: &self.project,
                project_fd: std::os::fd::AsRawFd::as_raw_fd(&project_fd),
                network,
                roots: host_roots(),
                etc: host_etc(),
                system_masks: host_system_masks(&host_roots()),
                masks: &self.masks,
            };
            let seccomp = run.join("seccomp.bpf");
            let args_path = run.join("args");
            let program = super::super::seccomp::program().unwrap();
            std::fs::write(&seccomp, super::super::seccomp::to_bytes(&program)).unwrap();
            std::fs::write(&args_path, args_file(&args(&plan)).unwrap()).unwrap();
            let mut command = launcher(
                &self.bwrap,
                &seccomp,
                &args_path,
                OsStr::new("/bin/sh"),
                &["-c".into(), script.into()],
            )
            .unwrap();
            command
                .env("PATH", "/usr/bin:/bin")
                .env("LANG", "C.UTF-8")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let child = super::super::linux::spawn_with(project_fd, || command.spawn()).unwrap();
            let out = child.wait_with_output().await.unwrap();
            Output {
                code: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }
        }
    }

    const AF_UNIX: &str = "import socket; socket.socket(socket.AF_UNIX)";

    #[tokio::test]
    #[ignore = "needs bubblewrap; run by the linux-sandbox CI job"]
    async fn linux_sandbox_seccomp_masks_and_read_only_hold_under_the_real_bwrap() {
        let sb = Sandbox::new();

        // The sandbox runs, and sees the project.
        let ok = sb.run(false, "cat README && pwd").await;
        assert_eq!(ok.code, Some(0), "{}", ok.stderr);
        assert_eq!(ok.stdout, format!("hello\n{}\n", sb.project.display()));

        // AF_UNIX socket() fails with EPERM, with the network off and on.
        for network in [false, true] {
            let out = sb.run(network, &format!("python3 -c '{AF_UNIX}'")).await;
            assert_eq!(out.code, Some(1), "network {network}: {}", out.stderr);
            assert!(
                out.stderr
                    .contains("PermissionError: [Errno 1] Operation not permitted"),
                "{}",
                out.stderr
            );
        }

        // AF_INET works with --share-net and reaches a listener on the host.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let connect = format!(
            "python3 -c 'import socket; s=socket.create_connection((\"127.0.0.1\", {port}), 2); print(\"connected\")'"
        );
        let on = sb.run(true, &connect).await;
        assert_eq!(
            (on.code, on.stdout.as_str()),
            (Some(0), "connected\n"),
            "{}",
            on.stderr
        );
        let off = sb.run(false, &connect).await;
        assert_ne!(off.code, Some(0), "connected with the network off");

        // Only a stream socketpair is allowed; datagram, raw (which becomes datagram) and
        // seqpacket pairs fail with EPERM.
        for ty in ["SOCK_DGRAM", "SOCK_RAW", "SOCK_SEQPACKET"] {
            let out = sb
                .run(
                    true,
                    &format!("python3 -c 'import socket; socket.socketpair(socket.AF_UNIX, socket.{ty})'"),
                )
                .await;
            assert_eq!(out.code, Some(1), "{ty}: {}", out.stderr);
            assert!(
                out.stderr
                    .contains("PermissionError: [Errno 1] Operation not permitted"),
                "{ty}: {}",
                out.stderr
            );
        }
        // No new user namespace inside (--disable-userns), so no nested sandbox or mount.
        let userns = sb.run(false, "unshare -U true").await;
        assert_ne!(userns.code, Some(0), "unshare -U succeeded");

        let pair = sb
            .run(false, "python3 -c 'import socket; a, b = socket.socketpair(); a.send(b\"x\"); print(b.recv(1).decode())'")
            .await;
        assert_eq!(
            (pair.code, pair.stdout.as_str()),
            (Some(0), "x\n"),
            "{}",
            pair.stderr
        );

        // io_uring_setup fails with ENOSYS inside (checked against the host when it has io_uring).
        let uring = "import ctypes, os; libc = ctypes.CDLL(None, use_errno=True); \
            params = ctypes.create_string_buffer(120); \
            fd = libc.syscall(425, 1, params); \
            print(fd if fd >= 0 else os.strerror(ctypes.get_errno()))";
        let inside = sb.run(false, &format!("python3 -c '{uring}'")).await;
        assert_eq!(
            inside.stdout, "Function not implemented\n",
            "{}",
            inside.stderr
        );

        // Masked names: a directory is empty, a file can't be opened, and neither can be written.
        let git = sb.run(false, "cat .git/HEAD").await;
        assert_ne!(git.code, Some(0));
        assert!(
            git.stderr.contains("No such file or directory"),
            "{}",
            git.stderr
        );
        let listing = sb.run(false, "ls -A .git").await;
        assert_eq!((listing.code, listing.stdout.as_str()), (Some(0), ""));
        let ssh = sb.run(false, "cat x/.ssh").await;
        assert_ne!(ssh.code, Some(0));
        assert!(ssh.stderr.contains("Permission denied"), "{}", ssh.stderr);
        assert!(!ssh.stdout.contains("secret"));

        // Read-only: no write to the project, a masked folder or the root; /tmp is private.
        for target in ["out", ".git/new", "x/.ssh", "/new"] {
            let w = sb.run(false, &format!("printf x > {target}")).await;
            assert_ne!(w.code, Some(0), "wrote {target}");
        }
        assert!(!sb.project.join("out").exists());
        let tmp = sb.run(false, "printf x > /tmp/t && cat /tmp/t").await;
        assert_eq!(
            (tmp.code, tmp.stdout.as_str()),
            (Some(0), "x"),
            "{}",
            tmp.stderr
        );

        // Local configuration under /usr is hidden.
        if Path::new("/usr/local/etc").is_dir() {
            let local = sb.run(false, "ls -A /usr/local/etc").await;
            assert_eq!(
                (local.code, local.stdout.as_str()),
                (Some(0), ""),
                "{}",
                local.stderr
            );
        }

        // Nothing from /run, the host home or the run folder is visible, and bubblewrap (pid 1)
        // carries only the environment the launcher was given, not the test's.
        let hidden = sb
            .run(
                true,
                &format!(
                    "test ! -e /run && test ! -e /var/run && ! ls /root 2>/dev/null \
                     && ls -A {} && tr '\\0' '\\n' < /proc/1/environ",
                    sb.temp.0.display()
                ),
            )
            .await;
        assert_eq!(hidden.code, Some(0), "{}", hidden.stderr);
        let lines: Vec<&str> = hidden.stdout.lines().collect();
        assert_eq!(lines.first(), Some(&"p"), "the run folder is visible");
        let environ = &lines[1..];
        assert!(environ.contains(&"PATH=/usr/bin:/bin") && environ.contains(&"LANG=C.UTF-8"));
        // Anything else comes from /bin/sh itself and says nothing about the host.
        for var in environ {
            let name = var.split('=').next().unwrap();
            assert!(
                matches!(name, "PATH" | "LANG" | "SHLVL" | "_") || *var == "PWD=/",
                "{var} leaked into bubblewrap's environment"
            );
        }
    }
}
