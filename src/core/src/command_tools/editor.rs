//! What the desktop's Command tools screen needs: list, save and remove `tools.json` entries, and
//! find the coding assistants' command lines on this computer.
use super::*;
use std::ffi::OsStr;

const EDIT_LOCK: &str = ".tools.lock";

/// One tool for the screen. `subagent_problem` says why a subagent cannot run on it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSummary {
    pub name: String,
    pub description: String,
    pub kind: &'static str,
    pub command: Vec<String>,
    pub subagent_problem: Option<&'static str>,
}

const SAVE_FAILED: &str = "The tools file could not be saved.";

/// The tools file's bytes, or `None` when there is no file yet.
fn read_body(directory: &Path) -> Result<Option<Vec<u8>>, &'static str> {
    use std::io::Read;
    let mut body = vec![];
    match std::fs::File::open(directory.join(COMMAND_TOOLS_FILE)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("The tools file could not be read."),
        Ok(file) => file
            .take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut body)
            .map(|_| Some(body))
            .map_err(|_| "The tools file could not be read."),
    }
}

/// The owner's tools for the screen. A file that is not valid is an error, not an empty list.
pub fn summaries(directory: &Path) -> Result<Vec<ToolSummary>, String> {
    let tools = match read_body(directory)? {
        None => vec![],
        Some(body) => parse_file(&body)?,
    };
    Ok(tools
        .iter()
        .map(|tool| ToolSummary {
            name: tool.name.clone(),
            description: tool.description.clone(),
            kind: match tool.kind {
                ToolKind::Command => "command",
                ToolKind::Http => "http",
            },
            command: tool.command.clone(),
            subagent_problem: tool.delegate_problem(),
        })
        .collect())
}

/// Change the file's `tools` array as plain JSON, so entries and fields the screen does not know
/// stay exactly as written, then save only what the loader accepts. Nothing is saved otherwise.
fn edit(
    directory: &Path,
    change: impl FnOnce(&mut Vec<Value>) -> Result<(), String>,
) -> Result<(), String> {
    let _editing = crate::policy::lock_owner_file(directory, EDIT_LOCK).map_err(|_| SAVE_FAILED)?;
    let mut root = match read_body(directory)? {
        None => json!({ "tools": [] }),
        Some(body) => {
            parse_file(&body)?;
            serde_json::from_slice(&body).map_err(|_| SAVE_FAILED)?
        }
    };
    change(
        root.get_mut("tools")
            .and_then(Value::as_array_mut)
            .ok_or(SAVE_FAILED)?,
    )?;
    let mut body = serde_json::to_vec_pretty(&root).map_err(|_| SAVE_FAILED)?;
    body.push(b'\n');
    parse_file(&body)?;
    let path = directory.join(COMMAND_TOOLS_FILE);
    // Like the other owner files, never replace a link.
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("The tools file must not be a symbolic link.".into());
    }
    crate::policy::replace_owner_file(directory, &path, &body).map_err(|_| SAVE_FAILED.into())
}

fn position(tools: &[Value], name: &str) -> Option<usize> {
    tools.iter().position(|tool| tool["name"] == name)
}

fn command_tool_only(tool: &Value) -> Result<(), String> {
    match tool.get("kind").and_then(Value::as_str) {
        None | Some("command") => Ok(()),
        _ => Err(format!(
            "{} is an HTTP tool. Edit it in {COMMAND_TOOLS_FILE}.",
            tool["name"].as_str().unwrap_or_default()
        )),
    }
}

/// Add a command tool, or change the description and command of the one with this name and keep
/// everything else it has. `prompt_on_stdin` makes a new tool take a `prompt` as JSON on standard
/// input, which is what a subagent needs.
pub fn save_tool(
    directory: &Path,
    name: &str,
    description: &str,
    command: Vec<String>,
    prompt_on_stdin: bool,
) -> Result<(), String> {
    if !valid_name(name) {
        return Err("A tool name uses lowercase letters, digits and underscores.".into());
    }
    edit(directory, |tools| {
        match position(tools, name) {
            Some(index) => {
                command_tool_only(&tools[index])?;
                tools[index]["description"] = json!(description.trim());
                tools[index]["command"] = json!(command);
            }
            None => {
                let mut tool =
                    json!({"name":name,"description":description.trim(),"command":command});
                if prompt_on_stdin {
                    tool["stdin"] = json!("json");
                    tool["parameters"] = json!({"type":"object","properties":{"prompt":{"type":"string"}},"required":["prompt"]});
                }
                tools.push(tool);
            }
        }
        Ok(())
    })
}

/// Remove a command tool. HTTP tools have saved secrets and stay in the file.
pub fn remove_tool(directory: &Path, name: &str) -> Result<(), String> {
    edit(directory, |tools| {
        let index = position(tools, name)
            .ok_or_else(|| format!("There is no command tool named {name}."))?;
        command_tool_only(&tools[index])?;
        tools.remove(index);
        Ok(())
    })
}

/// A command line that can be installed and signed in, as far as this computer can tell without
/// running it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliStatus {
    pub name: &'static str,
    pub installed: bool,
    pub signed_in: bool,
}

/// Whether `claude`, `codex` and `copilot` are on the owner's `PATH`, and whether each one's
/// sign-in file exists.
pub fn detect_clis() -> Vec<CliStatus> {
    let appdir = std::env::var_os("APPDIR").map(PathBuf::from);
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    detect_in(
        crate::tools::search_path().as_deref(),
        crate::child_env::valid_appdir(appdir.as_deref()),
        std::env::home_dir().as_deref(),
        codex_home.as_deref(),
    )
}

/// A file only counts when it is a non-empty regular file; its contents are never read. Where
/// a program keeps its sign-in in the system keychain there is no file, and that reads as not
/// detected. Copilot's location is unknown, so it is never detected.
fn detect_in(
    search_path: Option<&OsStr>,
    appdir: Option<&Path>,
    home: Option<&Path>,
    codex_home: Option<&Path>,
) -> Vec<CliStatus> {
    let sign_in = |name: &str| match name {
        "claude" => home.map(|home| home.join(".claude").join(".credentials.json")),
        "codex" => codex_home
            .map(Path::to_path_buf)
            .or_else(|| home.map(|home| home.join(".codex")))
            .map(|folder| folder.join("auth.json")),
        _ => None,
    };
    ["claude", "codex", "copilot"]
        .into_iter()
        .map(|name| CliStatus {
            name,
            installed: resolve_in(name, search_path, appdir, Path::new("")).is_ok(),
            signed_in: sign_in(name)
                .and_then(|file| std::fs::metadata(file).ok())
                .is_some_and(|meta| meta.is_file() && meta.len() > 0),
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn folder() -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("blackwall-tools-edit-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn read(directory: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(directory.join(COMMAND_TOOLS_FILE)).unwrap()).unwrap()
    }

    const HTTP: &str = r#"{"name":"issues","description":"Issues","kind":"http","url":"https://example.com/{id}","headers":{"Authorization":"{secret:TOKEN}"}}"#;
    const LOCAL: &str = r#"{"name":"lint","description":"Lint","command":["eslint","{file}"],"env":["HOME"],"parameters":{"type":"object","properties":{"file":{"type":"string","description":"The file"}},"required":["file"]}}"#;

    #[test]
    fn a_missing_file_has_no_tools_and_a_save_creates_it() {
        let directory = folder();
        assert!(summaries(&directory).unwrap().is_empty());
        save_tool(
            &directory,
            "claude",
            "Ask Claude",
            vec!["claude".into(), "-p".into()],
            true,
        )
        .unwrap();
        let tools = summaries(&directory).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "claude");
        assert_eq!(tools[0].command, ["claude", "-p"]);
        assert_eq!(tools[0].subagent_problem, None);
        let saved = read(&directory);
        assert_eq!(saved["tools"][0]["stdin"], "json");
        assert_eq!(
            saved["tools"][0]["parameters"]["required"],
            json!(["prompt"])
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_tool_without_a_prompt_on_stdin_is_not_a_subagent_candidate() {
        let directory = folder();
        save_tool(
            &directory,
            "copilot",
            "Copilot",
            vec!["copilot".into(), "-p".into(), "{prompt}".into()],
            false,
        )
        .unwrap();
        let tools = summaries(&directory).unwrap();
        assert!(tools[0]
            .subagent_problem
            .is_some_and(|problem| problem.contains("stdin")));
        assert!(read(&directory)["tools"][0].get("stdin").is_none());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn other_entries_and_their_fields_survive_a_save_and_a_remove() {
        let directory = folder();
        let body = format!(r#"{{"tools":[{HTTP},{LOCAL}]}}"#);
        std::fs::write(directory.join(COMMAND_TOOLS_FILE), &body).unwrap();
        let before = read(&directory);
        save_tool(&directory, "claude", "Ask", vec!["claude".into()], true).unwrap();
        let after = read(&directory);
        assert_eq!(after["tools"][0], before["tools"][0]);
        assert_eq!(after["tools"][1], before["tools"][1]);
        // Editing an existing tool changes only its description and command.
        save_tool(
            &directory,
            "lint",
            "Lint files",
            vec!["biome".into(), "{file}".into()],
            true,
        )
        .unwrap();
        let edited = read(&directory)["tools"][1].clone();
        assert_eq!(edited["description"], "Lint files");
        assert_eq!(edited["command"], json!(["biome", "{file}"]));
        assert_eq!(edited["env"], before["tools"][1]["env"]);
        assert_eq!(edited["parameters"], before["tools"][1]["parameters"]);
        assert!(edited.get("stdin").is_none());
        remove_tool(&directory, "claude").unwrap();
        let removed = read(&directory);
        assert_eq!(removed["tools"].as_array().unwrap().len(), 2);
        assert_eq!(removed["tools"][0], before["tools"][0]);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn http_tools_are_listed_but_not_changed_or_removed() {
        let directory = folder();
        std::fs::write(
            directory.join(COMMAND_TOOLS_FILE),
            format!(r#"{{"tools":[{HTTP}]}}"#),
        )
        .unwrap();
        let tools = summaries(&directory).unwrap();
        assert_eq!(tools[0].kind, "http");
        assert!(tools[0].subagent_problem.is_some());
        let before = std::fs::read(directory.join(COMMAND_TOOLS_FILE)).unwrap();
        assert!(save_tool(&directory, "issues", "x", vec!["ls".into()], false).is_err());
        assert!(remove_tool(&directory, "issues").is_err());
        assert_eq!(
            std::fs::read(directory.join(COMMAND_TOOLS_FILE)).unwrap(),
            before
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn an_invalid_save_leaves_the_file_as_it_was() {
        let directory = folder();
        let body = format!(r#"{{"tools":[{LOCAL}]}}"#);
        std::fs::write(directory.join(COMMAND_TOOLS_FILE), &body).unwrap();
        for (name, command) in [
            ("Bad Name", vec!["ls".to_owned()]),
            ("shell", vec!["ls".into()]),
            ("empty", vec![]),
            ("batch", vec!["run.cmd".into()]),
            ("local", vec!["./run".into()]),
        ] {
            assert!(
                save_tool(&directory, name, "d", command, true).is_err(),
                "{name}"
            );
        }
        assert!(save_tool(&directory, "ok", "  ", vec!["ls".into()], true).is_err());
        assert!(remove_tool(&directory, "absent").is_err());
        assert_eq!(
            std::fs::read_to_string(directory.join(COMMAND_TOOLS_FILE)).unwrap(),
            body
        );
        // An unreadable file is reported, not replaced.
        std::fs::write(directory.join(COMMAND_TOOLS_FILE), "not json").unwrap();
        assert!(summaries(&directory).is_err());
        assert!(save_tool(&directory, "ok", "d", vec!["ls".into()], true).is_err());
        assert_eq!(
            std::fs::read_to_string(directory.join(COMMAND_TOOLS_FILE)).unwrap(),
            "not json"
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_not_replaced() {
        let directory = folder();
        let target = directory.join("elsewhere.json");
        std::fs::write(&target, r#"{"tools":[]}"#).unwrap();
        std::os::unix::fs::symlink(&target, directory.join(COMMAND_TOOLS_FILE)).unwrap();
        let error = save_tool(&directory, "ok", "d", vec!["ls".into()], true).unwrap_err();
        assert!(error.contains("symbolic link"), "{error}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), r#"{"tools":[]}"#);
        assert!(
            std::fs::symlink_metadata(directory.join(COMMAND_TOOLS_FILE))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn each_reason_a_subagent_cannot_use_a_tool_is_listed() {
        let directory = folder();
        let tools = [
            r#"{"name":"good","description":"d","command":["x"],"stdin":"json","parameters":{"type":"object","properties":{"prompt":{"type":"string"}},"required":["prompt"]}}"#,
            r#"{"name":"nostdin","description":"d","command":["x"],"parameters":{"type":"object","properties":{"prompt":{"type":"string"}},"required":["prompt"]}}"#,
            r#"{"name":"noprompt","description":"d","command":["x"],"stdin":"json"}"#,
            r#"{"name":"placeholder","description":"d","command":["x","{prompt}"],"stdin":"json"}"#,
            HTTP,
        ];
        std::fs::write(
            directory.join(COMMAND_TOOLS_FILE),
            format!(r#"{{"tools":[{}]}}"#, tools.join(",")),
        )
        .unwrap();
        let listed = summaries(&directory).unwrap();
        let problems: Vec<_> = listed
            .iter()
            .map(|tool| tool.subagent_problem.is_some())
            .collect();
        assert_eq!(problems, [false, true, true, true, true]);
        // The list agrees with what a run accepts.
        for tool in &listed {
            assert_eq!(
                check_delegate(&directory, &tool.name).is_ok(),
                tool.subagent_problem.is_none()
            );
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn detection_checks_programs_and_sign_in_files_without_reading_them() {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("blackwall-detect-{}", rand::random::<u64>()));
        let bin = root.join("bin");
        let home = root.join("home");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        for program in ["claude", "codex"] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(bin.join(program), "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(bin.join(program), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let path = std::env::join_paths([&bin]).unwrap();
        let find = |home: &Path, codex_home: Option<&Path>| {
            detect_in(Some(&path), None, Some(home), codex_home)
                .into_iter()
                .map(|cli| (cli.name, cli.installed, cli.signed_in))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            find(&home, None),
            [
                ("claude", true, false),
                ("codex", true, false),
                ("copilot", false, false)
            ]
        );
        // A directory or an empty file is not a sign-in.
        std::fs::create_dir_all(home.join(".claude/.credentials.json")).unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex/auth.json"), "").unwrap();
        assert_eq!(
            find(&home, None),
            [
                ("claude", true, false),
                ("codex", true, false),
                ("copilot", false, false)
            ]
        );
        std::fs::remove_dir(home.join(".claude/.credentials.json")).unwrap();
        std::fs::write(home.join(".claude/.credentials.json"), "x").unwrap();
        std::fs::write(home.join(".codex/auth.json"), "x").unwrap();
        assert_eq!(
            find(&home, None),
            [
                ("claude", true, true),
                ("codex", true, true),
                ("copilot", false, false)
            ]
        );
        // CODEX_HOME moves the codex file.
        let other = root.join("codex-home");
        std::fs::create_dir_all(&other).unwrap();
        assert!(!find(&home, Some(&other))[1].2);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
