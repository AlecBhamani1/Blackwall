//! `bw telemetry`: OTLP export headers and the test span. Header values are read from piped
//! standard input only, so they never appear in shell history or `ps`, and are saved only in the
//! credential store under the endpoint's origin; the settings keep each header's name and origin.
use crate::settings::Settings;
use blackwall_core::{storage::LocalStore, telemetry::export};
use std::io::{IsTerminal, Read};

const USAGE: &str = "Use bw telemetry test, bw telemetry header <name> (value piped on standard input), or bw telemetry header <name> --delete.";

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, export::ExportError> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| "The credential store did not respond.".to_owned())?
        .map_err(|error| error.to_string())
}

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
            let (removed, kept) = updated
                .export
                .headers
                .drain(..)
                .partition(|saved| saved.name == name);
            updated.export.headers = kept;
            let removed: Vec<export::SavedHeader> = removed;
            blocking(move || removed.iter().try_for_each(export::delete_header)).await?;
            updated.save(store)?;
            *settings = updated;
            crate::session::reload_telemetry(settings);
            println!("Removed the header.");
            Ok(())
        }
        [header, name] if header == "header" => {
            let endpoint = settings
                .export
                .endpoint
                .clone()
                .ok_or("Set bw config set otlp-endpoint <url> first; a header is saved for that endpoint only.")?;
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
            let saved = blocking(move || export::save_header(&endpoint, &name, &value)).await?;
            // Entering a header again replaces any copy saved for another endpoint.
            let mut updated = settings.clone();
            let stale: Vec<export::SavedHeader> = updated
                .export
                .headers
                .iter()
                .filter(|header| header.name == saved.name && **header != saved)
                .cloned()
                .collect();
            updated
                .export
                .headers
                .retain(|header| header.name != saved.name);
            updated.export.headers.push(saved.clone());
            updated.validate()?;
            updated.save(store)?;
            *settings = updated;
            blocking(move || stale.iter().try_for_each(export::delete_header)).await?;
            crate::session::reload_telemetry(settings);
            println!(
                "Saved the {} header for {} in {}.",
                saved.name,
                saved.origin,
                blackwall_core::secrets::STORE
            );
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}
