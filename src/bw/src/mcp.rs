//! Owner commands for MCP servers. They edit `mcp.json` and never start a server.
use blackwall_core::mcp::{read_config, write_config, ServerConfig};
use std::path::Path;

const USAGE: &str =
    "Use bw mcp list|add <name> <command> [arguments]|enable <name>|disable <name>|remove <name>.";

pub fn run(directory: &Path, args: &[String]) -> Result<(), String> {
    let mut servers = read_config(directory)?;
    let command = args.first().map(String::as_str).unwrap_or("list");
    let name = args.get(1).cloned().unwrap_or_default();
    let position = servers.iter().position(|server| server.name == name);
    let missing = || format!("No MCP server is named {name:?}.");
    match command {
        "list" if args.len() <= 1 => {
            if servers.is_empty() {
                println!("No MCP servers. Add one with bw mcp add <name> <command> [arguments].");
            }
            // JSON arrays show where each argument ends and escape control characters.
            let list = |items: &[String]| serde_json::to_string(items).unwrap_or_default();
            for server in &servers {
                let state = if server.enabled {
                    "enabled"
                } else {
                    "disabled"
                };
                println!("{} [{state}]\n{}", server.name, list(&server.command));
                if let Some(allow) = &server.allow_tools {
                    println!("Allowed tools: {}", list(allow));
                }
                if !server.deny_tools.is_empty() {
                    println!("Denied tools: {}", list(&server.deny_tools));
                }
            }
            return Ok(());
        }
        "add" if args.len() >= 3 => {
            if position.is_some() {
                return Err(format!("An MCP server named {name:?} already exists."));
            }
            servers.push(ServerConfig {
                name: name.clone(),
                command: args[2..].to_vec(),
                enabled: true,
                allow_tools: None,
                deny_tools: vec![],
            });
            write_config(directory, servers)?;
            println!("MCP server {name} added. It starts with the next Agent run.");
        }
        "enable" | "disable" if args.len() == 2 => {
            let index = position.ok_or_else(missing)?;
            servers[index].enabled = command == "enable";
            write_config(directory, servers)?;
            println!("MCP server {name} {command}d.");
        }
        "remove" if args.len() == 2 => {
            servers.remove(position.ok_or_else(missing)?);
            write_config(directory, servers)?;
            println!("MCP server {name} removed.");
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn servers_are_added_toggled_and_removed_without_losing_other_entries() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let directory = std::env::temp_dir().join(format!(
            "blackwall-mcp-cli-{}-{}",
            std::process::id(),
            unique.as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        run(&directory, &args("add files npx -y server-filesystem /tmp")).unwrap();
        run(&directory, &args("add jira jira-mcp")).unwrap();
        assert!(run(&directory, &args("add jira other")).is_err());
        assert!(run(&directory, &args("add Bad! x")).is_err());
        assert!(run(&directory, &args("add batch run.cmd")).is_err());
        run(&directory, &args("disable files")).unwrap();
        assert!(run(&directory, &args("enable missing")).is_err());
        let servers = read_config(&directory).unwrap();
        assert_eq!(servers.len(), 2);
        assert!(!servers[0].enabled);
        assert_eq!(servers[0].command, args("npx -y server-filesystem /tmp"));
        run(&directory, &args("remove files")).unwrap();
        let servers = read_config(&directory).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "jira");
        // An invalid file is reported and never overwritten.
        std::fs::write(directory.join("mcp.json"), b"{\"servers\":[],\"extra\":1}").unwrap();
        assert!(run(&directory, &args("add files npx")).is_err());
        assert_eq!(
            std::fs::read(directory.join("mcp.json")).unwrap(),
            b"{\"servers\":[],\"extra\":1}"
        );
        // A linked file is never replaced by a regular one.
        #[cfg(unix)]
        {
            std::fs::remove_file(directory.join("mcp.json")).unwrap();
            std::os::unix::fs::symlink(directory.join("shared.json"), directory.join("mcp.json"))
                .unwrap();
            assert!(run(&directory, &args("add files npx")).is_err());
            assert!(!directory.join("shared.json").exists());
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
