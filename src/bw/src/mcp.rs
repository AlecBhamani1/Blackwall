//! Owner commands for MCP servers. They edit `mcp.json` and never start a server.
use blackwall_core::mcp;
use std::{
    io::{IsTerminal, Read},
    path::Path,
};

const USAGE: &str = "Use bw mcp list|add <name> <command> [arguments]|add <name> --url <url> [--header <Header>=<SECRET_NAME>]...|enable <name>|disable <name>|remove <name>|secret <name> <NAME> [--delete].";

pub fn run(directory: &Path, args: &[String]) -> Result<(), String> {
    let command = args.first().map(String::as_str).unwrap_or("list");
    let name = args.get(1).map(String::as_str).unwrap_or_default();
    match command {
        "list" if args.len() <= 1 => {
            let servers = mcp::read_config(directory)?;
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
                match &server.url {
                    Some(url) => println!("{} [{state}]\n{url}", server.name),
                    None => println!("{} [{state}]\n{}", server.name, list(&server.command)),
                }
                for (header, value) in &server.headers {
                    println!("Header: {header}: {value}");
                }
                if let Some(allow) = &server.allow_tools {
                    println!("Allowed tools: {}", list(allow));
                }
                if !server.deny_tools.is_empty() {
                    println!("Denied tools: {}", list(&server.deny_tools));
                }
                if !server.secrets.is_empty() {
                    println!("Secrets: {}", list(&server.secrets));
                }
                for (key, saved) in server.secrets_saved_elsewhere() {
                    println!("{key} was saved for {saved}; save it again to use it here.");
                }
            }
        }
        "add" if name.starts_with("--") => return Err(USAGE.into()),
        "add" if args.get(2).is_some_and(|flag| flag == "--url") => {
            let (url, headers) = url_arguments(&args[3..]).ok_or(USAGE)?;
            let secrets: Vec<String> = headers.iter().map(|(_, secret)| secret.clone()).collect();
            let name = mcp::add_http_server(directory, name, url, headers)?;
            println!("MCP server {name} added. It connects with the next Agent run.");
            for secret in secrets {
                println!("Save {secret} with: bw mcp secret {name} {secret}");
            }
        }
        "add" if args.len() >= 3 => {
            let name = mcp::add_server(directory, name, args[2..].to_vec())?;
            println!("MCP server {name} added. It starts with the next Agent run.");
        }
        "enable" | "disable" if args.len() == 2 => {
            mcp::set_enabled(directory, name, command == "enable")?;
            println!("MCP server {name} {command}d.");
        }
        "remove" if args.len() == 2 => {
            mcp::remove_server(directory, name)?;
            println!("MCP server {name} removed.");
        }
        "secret" if args.len() == 4 && args[3] == "--delete" => {
            mcp::delete_secret(directory, name, &args[2])?;
            println!("Removed {} from MCP server {name}.", args[2]);
        }
        "secret" if args.len() == 3 => {
            if std::io::stdin().is_terminal() {
                return Err(
                    "Pipe the value into this command, for example from the clipboard, so it is never shown."
                        .into(),
                );
            }
            let mut value = String::new();
            std::io::stdin()
                .take(16 * 1024 + 1)
                .read_to_string(&mut value)
                .map_err(|_| "The value could not be read.")?;
            mcp::save_secret(
                directory,
                name,
                &args[2],
                value.trim_end_matches(['\r', '\n']),
            )?;
            println!(
                "Saved {} for MCP server {name} in {}.",
                args[2],
                blackwall_core::secrets::STORE
            );
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

/// `<url> [--header <Header>=<SECRET_NAME>]...`
fn url_arguments(args: &[String]) -> Option<(&str, Vec<(String, String)>)> {
    let (url, rest) = args.split_first()?;
    if url.starts_with("--") || rest.len() % 2 != 0 {
        return None;
    }
    let headers = rest
        .chunks(2)
        .map(|pair| {
            let (header, secret) = pair[1].split_once('=')?;
            (pair[0] == "--header").then(|| (header.to_owned(), secret.to_owned()))
        })
        .collect::<Option<_>>()?;
    Some((url, headers))
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
        let invalid = run(&directory, &args("add Bad! x")).unwrap_err();
        assert!(invalid.contains("lowercase letters"), "{invalid}");
        run(&directory, &args("add Context7 npx")).unwrap();
        assert!(mcp::read_config(&directory)
            .unwrap()
            .iter()
            .any(|server| server.name == "context7"));
        run(&directory, &args("disable Context7")).unwrap();
        run(&directory, &args("remove Context7")).unwrap();
        assert!(run(&directory, &args("add batch run.cmd")).is_err());
        run(&directory, &args("disable files")).unwrap();
        assert!(run(&directory, &args("enable missing")).is_err());
        let servers = mcp::read_config(&directory).unwrap();
        assert_eq!(servers.len(), 2);
        assert!(!servers[0].enabled);
        assert_eq!(servers[0].command, args("npx -y server-filesystem /tmp"));
        run(&directory, &args("remove files")).unwrap();
        let servers = mcp::read_config(&directory).unwrap();
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

    #[test]
    fn url_servers_are_added_with_secret_headers() {
        let directory = std::env::temp_dir().join(format!(
            "blackwall-mcp-cli-url-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        run(
            &directory,
            &args("add Docs --url https://mcp.example.com/mcp --header Authorization=DOCS_TOKEN"),
        )
        .unwrap();
        let servers = mcp::read_config(&directory).unwrap();
        assert_eq!(servers[0].name, "docs");
        assert_eq!(
            servers[0].url.as_deref(),
            Some("https://mcp.example.com/mcp")
        );
        assert!(servers[0].command.is_empty());
        assert_eq!(servers[0].headers["Authorization"], "{secret:DOCS_TOKEN}");
        for invalid in [
            "add a --url",
            "add a --url --header X=Y",
            "add a --url https://mcp.example.com/mcp --header",
            "add a --url https://mcp.example.com/mcp --header NoEquals",
            "add a --url https://mcp.example.com/mcp --other X=Y",
            "add a --url http://mcp.example.com/mcp",
            "add a --url https://mcp.example.com/mcp --header Authorization=lower",
            "add p-docs --url https://mcp.example.com/mcp",
            "add --url https://mcp.example.com/mcp",
            "add --url",
        ] {
            assert!(run(&directory, &args(invalid)).is_err(), "{invalid}");
        }
        assert_eq!(mcp::read_config(&directory).unwrap().len(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
