//! Owner commands for HTTP command tools' header secrets. Tools themselves are edited in
//! `tools.json`; values are saved in the credential store for the tool URL's origin.
use blackwall_core::command_tools;
use std::{
    io::{IsTerminal, Read},
    path::Path,
};

const USAGE: &str = "Use bw tools secret <tool> <NAME> [--delete], with the value piped on stdin.";

pub fn run(directory: &Path, args: &[String]) -> Result<(), String> {
    match args {
        [command, tool, key, flag] if command == "secret" && flag == "--delete" => {
            let origin = command_tools::delete_secret(directory, tool, key)?;
            println!("Removed {key} for {origin}.");
        }
        [command, tool, key] if command == "secret" => {
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
            let origin = command_tools::save_secret(
                directory,
                tool,
                key,
                value.trim_end_matches(['\r', '\n']),
            )?;
            println!(
                "Saved {key} for {origin} in {}. Only requests to that origin receive it.",
                blackwall_core::secrets::STORE
            );
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
    fn secrets_need_an_http_tool_and_a_valid_name() {
        let directory = std::env::temp_dir().join(format!(
            "blackwall-tools-cli-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        assert!(run(&directory, &args("list"))
            .unwrap_err()
            .contains("bw tools secret"));
        assert!(run(&directory, &args("secret gitlab TOKEN --delete"))
            .unwrap_err()
            .contains("could not be read"));
        std::fs::write(
            directory.join(command_tools::COMMAND_TOOLS_FILE),
            r#"{"tools":[{"name":"status","description":"Git status","command":["git","status"]}]}"#,
        )
        .unwrap();
        let error = run(&directory, &args("secret status TOKEN --delete")).unwrap_err();
        assert!(error.contains("No HTTP tool"), "{error}");
        let error = run(&directory, &args("secret status lower --delete")).unwrap_err();
        assert!(error.contains("uppercase"), "{error}");
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
