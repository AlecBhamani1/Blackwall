//! The environment every child program starts with: shell commands, owner command tools and MCP
//! servers. It is built from an allow-list, so the AppImage runtime's variables (`PYTHONHOME`,
//! `LD_LIBRARY_PATH`, ...) and the owner's unrelated credentials never reach a child.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Who the child runs for. Sandboxed children get the minimal set; owner children (command tools,
/// MCP servers) also get the owner's locale, identity and any explicitly passed-through names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Sandboxed,
    Owner,
}

/// The variables a child starts with, plus the names (never values) that were left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChildEnv {
    vars: Vec<(OsString, OsString)>,
    dropped: Vec<String>,
}

/// Names an owner child inherits, besides every `LC_*` variable.
pub(crate) const OWNER_ALLOW: [&str; 9] = [
    "HOME",
    "LANG",
    "TERM",
    "TMPDIR",
    "USER",
    "LOGNAME",
    "SHELL",
    "TZ",
    "XDG_RUNTIME_DIR",
];
/// Names a sandboxed child inherits, besides `PATH` and `TMPDIR`.
pub(crate) const SANDBOX_ALLOW: [&str; 4] = ["HOME", "LANG", "LC_ALL", "TERM"];
/// Variables that make a program load attacker-chosen code. Besides these, the `LD_` and `DYLD_`
/// prefixes are refused.
pub(crate) const INJECTION: &[&str] = &[
    "PYTHONHOME",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "PYTHONUSERBASE",
    "NODE_OPTIONS",
    "NODE_PATH",
    "PERL5LIB",
    "PERLLIB",
    "PERL5OPT",
    "RUBYOPT",
    "RUBYLIB",
    "GTK_PATH",
    "GIO_MODULE_DIR",
    "QT_PLUGIN_PATH",
    "GCONV_PATH",
    "LOCPATH",
    "NLSPATH",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
    "JDK_JAVA_OPTIONS",
    "BASH_ENV",
    "ENV",
    "PROMPT_COMMAND",
    "GIT_SSH_COMMAND",
    "GIT_ASKPASS",
];
/// What Windows programs need to start at all. Owner and sandboxed children keep these on Windows.
pub(crate) const WINDOWS_ALLOW: [&str; 10] = [
    "SYSTEMROOT",
    "WINDIR",
    "USERPROFILE",
    "TEMP",
    "TMP",
    "COMSPEC",
    "PATHEXT",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
];
/// Blackwall's model and relay secrets, which child programs never inherit.
pub(crate) const SECRET_ENV: [&str; 4] = [
    "BLACKWALL_MODEL_API_KEY",
    "BLACKWALL_RELAY_TOKEN",
    "TAURI_SIGNING_PRIVATE_KEY",
    "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
];
const MAX_DROPPED: usize = 20;

/// Whether two variable names are the same: Windows ignores case, other systems do not.
fn same_name(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Builds the child environment from `source` (normally the process environment).
///
/// `PATH` comes from `search_path` (the owner's login-shell one) when set, otherwise from `source`,
/// and always goes through [`validated_path`]. `pass_through` names are added for the owner scope
/// only, and only when [`valid_pass_through`] accepts them. `project` should be the canonical
/// absolute project path, so `PATH` entries inside it are recognised.
pub(crate) fn build(
    source: impl IntoIterator<Item = (OsString, OsString)>,
    scope: Scope,
    search_path: Option<&OsStr>,
    project: &Path,
    pass_through: &[String],
) -> ChildEnv {
    let source: Vec<(OsString, OsString)> = source.into_iter().collect();
    let lookup = |name: &str| {
        source
            .iter()
            .find(|(k, _)| k.to_str().is_some_and(|k| same_name(k, name)))
            .map(|(_, v)| v)
    };
    let appdir = lookup("APPDIR")
        .map(Path::new)
        .filter(|dir| dir.is_absolute() && dir.parent().is_some());
    let mut vars: Vec<(OsString, OsString)> = Vec::new();
    for (key, value) in &source {
        let Some(name) = key.to_str() else { continue };
        let listed = |names: &[&str]| names.iter().any(|allowed| same_name(allowed, name));
        let on_windows = cfg!(windows) && listed(&WINDOWS_ALLOW);
        let allowed = on_windows
            || match scope {
                Scope::Sandboxed => listed(&SANDBOX_ALLOW),
                Scope::Owner => {
                    listed(&OWNER_ALLOW)
                        || name.starts_with("LC_")
                        || pass_through.iter().any(|wanted| {
                            same_name(wanted, name) && valid_pass_through(name).is_ok()
                        })
                }
            };
        if allowed {
            vars.push((key.clone(), value.clone()));
        }
    }
    if scope == Scope::Sandboxed {
        // The sandbox only exposes /tmp on Linux; on macOS the host temporary directory is kept.
        if cfg!(target_os = "linux") {
            vars.push(("TMPDIR".into(), "/tmp".into()));
        } else if let Some(tmp) = lookup("TMPDIR") {
            vars.push(("TMPDIR".into(), tmp.clone()));
        }
    }
    let path = search_path.or_else(|| lookup("PATH").map(OsString::as_os_str));
    if let Some(path) = path.and_then(|p| validated_path(p, appdir, project)) {
        vars.push(("PATH".into(), path));
    }
    vars.sort();
    vars.dedup_by(|a, b| a.0 == b.0);
    let mut dropped: Vec<String> = source
        .iter()
        .filter(|(key, _)| !vars.iter().any(|(kept, _)| kept == key))
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect();
    dropped.sort();
    dropped.dedup();
    dropped.truncate(MAX_DROPPED);
    ChildEnv { vars, dropped }
}

/// [`build`] from this process's environment and the login-shell `PATH`.
pub(crate) fn from_process(scope: Scope, project: &Path, pass_through: &[String]) -> ChildEnv {
    build(
        std::env::vars_os(),
        scope,
        crate::tools::search_path().as_deref(),
        project,
        pass_through,
    )
}

/// Keeps only the `PATH` entries a child may search: absolute ones outside the AppImage mount and
/// the project (pass the project's canonical absolute path, which is what symlink-free `PATH`
/// entries are compared with). Empty and relative entries would resolve to the current directory, and a project
/// entry would let a repository plant programs that the owner's tools then run.
pub(crate) fn validated_path(
    path: &OsStr,
    appdir: Option<&Path>,
    project: &Path,
) -> Option<OsString> {
    let inside = |entry: &Path, root: Option<&Path>| {
        root.is_some_and(|root| !root.as_os_str().is_empty() && entry.starts_with(root))
    };
    let kept: Vec<PathBuf> = std::env::split_paths(path)
        .filter(|entry| {
            entry.is_absolute()
                && !entry
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
                && !inside(entry, appdir)
                && !inside(entry, Some(project))
        })
        .collect();
    if kept.is_empty() {
        return None;
    }
    std::env::join_paths(kept).ok()
}

/// Checks a name the owner asked to pass through to children.
pub(crate) fn valid_pass_through(name: &str) -> Result<(), &'static str> {
    let mut chars = name.chars();
    let well_formed = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !well_formed {
        return Err("Environment variable names use letters, digits and underscores.");
    }
    let upper = name.to_ascii_uppercase();
    if upper == "PATH" {
        Err("PATH is set by Blackwall and cannot be passed through.")
    } else if SECRET_ENV.contains(&upper.as_str()) {
        Err("Blackwall's own secrets are never passed to child programs.")
    } else if INJECTION.contains(&upper.as_str())
        || upper.starts_with("LD_")
        || upper.starts_with("DYLD_")
    {
        Err("Variables that load code into programs cannot be passed through.")
    } else {
        Ok(())
    }
}

/// Checks the `env` list of an MCP server or command tool: at most 32 distinct valid names.
pub(crate) fn valid_pass_through_list(names: &[String]) -> Result<(), &'static str> {
    const TOO_MANY: &str = "List at most 32 distinct env names.";
    if names.len() > 32
        || names
            .iter()
            .enumerate()
            .any(|(i, name)| names[..i].contains(name))
    {
        return Err(TOO_MANY);
    }
    names.iter().try_for_each(|name| valid_pass_through(name))
}

impl ChildEnv {
    /// Replaces the command's whole environment with this one.
    pub(crate) fn apply(&self, command: &mut tokio::process::Command) {
        command.env_clear();
        command.envs(self.vars.iter().map(|(k, v)| (k, v)));
    }

    /// Names (never values) that were not passed, sorted, at most 20.
    pub(crate) fn dropped(&self) -> &[String] {
        &self.dropped
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::path::Path;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs.iter().map(|(k, v)| (k.into(), v.into())).collect()
    }
    fn names(env: &ChildEnv) -> Vec<String> {
        env.vars
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect()
    }
    fn get<'a>(env: &'a ChildEnv, name: &str) -> Option<&'a str> {
        env.vars
            .iter()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| v.to_str())
    }

    #[test]
    fn owner_scope_keeps_only_allow_list_lc_and_pass_through() {
        let env = build(
            vars(&[
                ("HOME", "/h"),
                ("LC_TIME", "C"),
                ("AWS_SECRET_ACCESS_KEY", "x"),
                ("GITHUB_TOKEN", "t"),
                ("SSH_AUTH_SOCK", "/s"),
            ]),
            Scope::Owner,
            None,
            Path::new("/p"),
            &["GITHUB_TOKEN".into()],
        );
        assert_eq!(names(&env), ["GITHUB_TOKEN", "HOME", "LC_TIME"]);
        assert_eq!(env.dropped(), ["AWS_SECRET_ACCESS_KEY", "SSH_AUTH_SOCK"]);
    }

    #[test]
    fn injection_and_secret_variables_never_pass_even_by_name() {
        let mut names = vec![
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "BLACKWALL_MODEL_API_KEY",
        ];
        names.extend(INJECTION);
        for name in [
            "BASH_ENV",
            "ENV",
            "NODE_PATH",
            "GCONV_PATH",
            "LOCPATH",
            "NLSPATH",
            "JAVA_TOOL_OPTIONS",
            "_JAVA_OPTIONS",
            "JDK_JAVA_OPTIONS",
            "PYTHONUSERBASE",
            "PROMPT_COMMAND",
            "GIT_SSH_COMMAND",
            "GIT_ASKPASS",
        ] {
            assert!(INJECTION.contains(&name), "{name}");
        }
        for name in names {
            assert!(valid_pass_through(name).is_err(), "{name}");
            assert!(
                valid_pass_through(&name.to_ascii_lowercase()).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn appimage_entries_are_stripped_from_path_and_not_passed() {
        let env = build(
            vars(&[
                ("APPDIR", "/tmp/.mount_x"),
                ("XDG_DATA_DIRS", "/tmp/.mount_x/usr/share"),
                ("PATH", "/tmp/.mount_x/usr/bin:/usr/bin"),
                ("PYTHONHOME", "/tmp/.mount_x/usr"),
            ]),
            Scope::Owner,
            None,
            Path::new("/p"),
            &[],
        );
        assert_eq!(get(&env, "PATH"), Some("/usr/bin"));
        assert_eq!(get(&env, "PYTHONHOME"), None);
        assert_eq!(get(&env, "APPDIR"), None);
        assert_eq!(get(&env, "XDG_DATA_DIRS"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sandboxed_scope_uses_tmp_on_linux_whatever_the_host_sets() {
        for host in [vars(&[("TMPDIR", "/var/tmp/x")]), vars(&[])] {
            let env = build(host, Scope::Sandboxed, None, Path::new("/p"), &[]);
            assert_eq!(get(&env, "TMPDIR"), Some("/tmp"));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandboxed_scope_keeps_the_host_tmpdir_on_macos() {
        let env = build(
            vars(&[("TMPDIR", "/var/folders/x/T/")]),
            Scope::Sandboxed,
            None,
            Path::new("/p"),
            &[],
        );
        assert_eq!(get(&env, "TMPDIR"), Some("/var/folders/x/T/"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_children_keep_system_variables_and_find_path_in_any_case() {
        for scope in [Scope::Owner, Scope::Sandboxed] {
            let env = build(
                vars(&[
                    ("SystemRoot", "C:\\Windows"),
                    ("ComSpec", "C:\\Windows\\System32\\cmd.exe"),
                    ("PATHEXT", ".EXE"),
                    ("Path", "C:\\Windows\\System32"),
                    ("AWS_SECRET_ACCESS_KEY", "x"),
                ]),
                scope,
                None,
                Path::new("C:\\p"),
                &[],
            );
            assert_eq!(get(&env, "SystemRoot"), Some("C:\\Windows"));
            assert_eq!(get(&env, "PATH"), Some("C:\\Windows\\System32"));
            assert_eq!(get(&env, "AWS_SECRET_ACCESS_KEY"), None);
        }
    }

    #[test]
    fn validated_path_drops_empty_relative_appdir_and_project_entries() {
        assert_eq!(
            validated_path(
                OsStr::new(":/usr/bin:.:bin:/p/node_modules/.bin:/a/usr/bin:/opt/x/bin"),
                Some(Path::new("/a")),
                Path::new("/p")
            ),
            Some("/usr/bin:/opt/x/bin".into())
        );
        assert_eq!(
            validated_path(OsStr::new("::"), None, Path::new("/p")),
            None
        );
    }

    #[test]
    fn validated_path_drops_entries_that_climb_out_of_a_blocked_directory() {
        assert_eq!(
            validated_path(OsStr::new("/usr/../p/bin:/usr/bin"), None, Path::new("/p")),
            Some("/usr/bin".into())
        );
    }

    #[test]
    fn search_path_replaces_the_source_path_and_is_validated() {
        let env = build(
            vars(&[("PATH", "/usr/bin")]),
            Scope::Owner,
            Some(OsStr::new("/opt/homebrew/bin:.:/p/bin")),
            Path::new("/p"),
            &[],
        );
        assert_eq!(get(&env, "PATH"), Some("/opt/homebrew/bin"));
    }

    #[test]
    fn sandboxed_scope_ignores_pass_through() {
        let env = build(
            vars(&[("GITHUB_TOKEN", "t")]),
            Scope::Sandboxed,
            None,
            Path::new("/p"),
            &["GITHUB_TOKEN".into()],
        );
        assert_eq!(get(&env, "GITHUB_TOKEN"), None);
        assert_eq!(env.dropped(), ["GITHUB_TOKEN"]);
    }

    #[test]
    fn invalid_pass_through_is_not_added_and_valid_names_are_accepted() {
        let env = build(
            vars(&[
                ("LD_PRELOAD", "/x.so"),
                ("PATH", "/usr/bin"),
                ("OK_NAME", "v"),
            ]),
            Scope::Owner,
            None,
            Path::new("/p"),
            &["LD_PRELOAD".into(), "OK_NAME".into()],
        );
        assert_eq!(names(&env), ["OK_NAME", "PATH"]);
        assert_eq!(env.dropped(), ["LD_PRELOAD"]);
        assert!(valid_pass_through("GITHUB_TOKEN").is_ok());
        for bad in ["", "A=B", "1A", "has space", "path", "ld_preload"] {
            assert!(valid_pass_through(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn dropped_holds_sorted_names_capped_at_twenty() {
        let pairs: Vec<(String, String)> = (0..30)
            .map(|i| (format!("SECRET_{i:02}"), "value".into()))
            .collect();
        let env = build(
            pairs.iter().map(|(k, v)| (k.into(), v.into())),
            Scope::Owner,
            None,
            Path::new("/p"),
            &[],
        );
        assert_eq!(env.dropped().len(), 20);
        assert_eq!(env.dropped()[0], "SECRET_00");
        assert!(env.dropped().iter().all(|name| !name.contains("value")));
    }

    #[test]
    fn apply_clears_the_inherited_environment() {
        let env = build(
            vars(&[("HOME", "/h")]),
            Scope::Owner,
            None,
            Path::new("/p"),
            &[],
        );
        let mut command = tokio::process::Command::new("true");
        command.env("LEAK", "1");
        env.apply(&mut command);
        let set: Vec<_> = command.as_std().get_envs().collect();
        assert_eq!(set, [(OsStr::new("HOME"), Some(OsStr::new("/h")))]);
    }

    #[test]
    fn sandboxed_scope_matches_todays_macos_set() {
        let env = build(
            vars(&[
                ("HOME", "/h"),
                ("LANG", "C"),
                ("LC_ALL", "C"),
                ("TERM", "x"),
                ("TMPDIR", "/t"),
                ("USER", "u"),
            ]),
            Scope::Sandboxed,
            Some(OsStr::new("/usr/bin")),
            Path::new("/p"),
            &[],
        );
        assert_eq!(
            names(&env),
            ["HOME", "LANG", "LC_ALL", "PATH", "TERM", "TMPDIR"]
        );
    }
}
