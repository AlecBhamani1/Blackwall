//! `bw telemetry`: OTLP export headers and the test span. Header values are read from piped
//! standard input only, so they never appear in shell history or `ps`, and are saved only in the
//! credential store; the settings keep the header names.
use crate::settings::Settings;
use blackwall_core::{storage::LocalStore, telemetry::export};
use std::io::{IsTerminal, Read};

const USAGE: &str = "Use bw telemetry test, bw telemetry header <name> (value piped on standard input), or bw telemetry header <name> --delete.";

pub async fn run(
    store: &LocalStore,
    settings: &mut Settings,
    arguments: &[String],
) -> Result<(), String> {
    match arguments {
        [] => {
            println!("{}", settings.describe());
            println!("{USAGE}");
            Ok(())
        }
        [test] if test == "test" => {
            let status = export::send_test_span(&settings.export, "cli")
                .await
                .map_err(|error| format!("Test span failed ({}): {error}", error.kind()))?;
            println!("Test span sent: HTTP {status}.");
            Ok(())
        }
        [header, name, delete] if header == "header" && delete == "--delete" => {
            let name = export::header_name(name).map_err(|error| error.to_string())?;
            let mut updated = settings.clone();
            updated.export.headers.retain(|saved| saved != &name);
            updated.save(store)?;
            *settings = updated;
            tokio::task::spawn_blocking(move || export::delete_header(&name))
                .await
                .map_err(|_| "The credential store did not respond.")?
                .map_err(|error| error.to_string())?;
            println!("Removed the header.");
            Ok(())
        }
        [header, name] if header == "header" => {
            if std::io::stdin().is_terminal() {
                return Err(
                    "Pipe the header value on standard input, such as from a password manager."
                        .into(),
                );
            }
            let mut value = String::new();
            std::io::stdin()
                .take(16 * 1024)
                .read_to_string(&mut value)
                .map_err(|_| "The header value could not be read.")?;
            let name = export::header_name(name).map_err(|error| error.to_string())?;
            let mut updated = settings.clone();
            if !updated.export.headers.contains(&name) {
                updated.export.headers.push(name.clone());
            }
            updated.validate()?;
            let saved = name.clone();
            tokio::task::spawn_blocking(move || export::save_header(&saved, &value))
                .await
                .map_err(|_| "The credential store did not respond.")?
                .map_err(|error| error.to_string())?;
            updated.save(store)?;
            *settings = updated;
            println!(
                "Saved the {name} header in {}.",
                blackwall_core::secrets::STORE
            );
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}
