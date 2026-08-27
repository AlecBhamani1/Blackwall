use std::{env, io, process::ExitCode};

use blackwall_core::{ChatMessage, ChatRequest, MessageRole};
use thiserror::Error;

const DEFAULT_MODEL: &str = "llama3.2";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bw: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), CliError> {
    let mut arguments = env::args().skip(1);
    let Some(command) = arguments.next() else {
        print_usage();
        return Ok(());
    };

    match command.as_str() {
        "request" | "run" => {
            let prompt = arguments.collect::<Vec<_>>().join(" ");
            if prompt.trim().is_empty() {
                return Err(CliError::MissingPrompt);
            }
            let model = env::var("BLACKWALL_MODEL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_MODEL.to_owned());
            let request = ChatRequest::new(
                "cli-preview",
                model,
                vec![ChatMessage::new(MessageRole::User, prompt)],
            );
            request.validate()?;
            serde_json::to_writer_pretty(io::stdout().lock(), &request)?;
            println!();
            if command == "run" {
                eprintln!(
                    "note: this M0 CLI links blackwall-core and prints the headless request; model transport lands in the next core milestone"
                );
            }
            Ok(())
        }
        "--help" | "-h" | "help" => {
            print_usage();
            Ok(())
        }
        unknown => Err(CliError::UnknownCommand(unknown.to_owned())),
    }
}

fn print_usage() {
    println!(
        "bw — headless Blackwall client\n\nUSAGE:\n  bw request <prompt>\n  bw run <prompt>\n\nENVIRONMENT:\n  BLACKWALL_MODEL  model id (default: {DEFAULT_MODEL})"
    );
}

#[derive(Debug, Error)]
enum CliError {
    #[error("a prompt is required")]
    MissingPrompt,
    #[error("unknown command {0:?}; run `bw --help`")]
    UnknownCommand(String),
    #[error(transparent)]
    Protocol(#[from] blackwall_core::ProtocolError),
    #[error("could not write the request: {0}")]
    Serialize(#[from] serde_json::Error),
}
