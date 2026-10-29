//! Model servers found on this computer, listed by `bw setup`.
use blackwall_core::connection::LocalService;

/// Text a local server sent, without control characters that could drive the terminal.
fn printable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

fn names(models: &[String]) -> String {
    let shown: Vec<_> = models.iter().take(5).map(|name| printable(name)).collect();
    match models.len() {
        0 => "none installed".into(),
        n if n > shown.len() => format!("{} (+{} more)", shown.join(", "), n - shown.len()),
        _ => shown.join(", "),
    }
}

/// A numbered list of `services`, or a note that none answered.
pub fn describe(services: &[LocalService]) -> String {
    if services.is_empty() {
        return "No model servers found on this computer.".into();
    }
    let mut text = String::from("Found on this computer:");
    for (number, service) in services.iter().enumerate() {
        text.push_str(&format!(
            "\n  {}. {}  {}\n     models: {}",
            number + 1,
            printable(&service.name),
            service.endpoint,
            names(&service.models)
        ));
        if !service.loaded.is_empty() {
            text.push_str(&format!("\n     loaded: {}", names(&service.loaded)));
            if let Some(window) = service.context_window {
                text.push_str(&format!(" · {window}-token context"));
            }
        }
    }
    text
}

/// The service a typed list number names.
pub fn chosen<'a>(services: &'a [LocalService], value: &str) -> Option<&'a LocalService> {
    let number: usize = value.trim().parse().ok()?;
    services.get(number.checked_sub(1)?)
}

/// The model to offer after choosing `service`: the current one if it has it, else what it
/// has loaded, else its first model.
pub fn offered_model(service: &LocalService, current: &str) -> String {
    if service.models.iter().any(|model| model == current) {
        return current.into();
    }
    service
        .loaded
        .first()
        .or(service.models.first())
        .cloned()
        .unwrap_or_else(|| current.into())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use blackwall_core::connection::ServerKind;

    fn service(name: &str, port: u16, models: &[&str], loaded: &[&str]) -> LocalService {
        LocalService {
            name: name.into(),
            endpoint: format!("http://127.0.0.1:{port}/v1"),
            available: true,
            models: models.iter().map(|m| (*m).to_owned()).collect(),
            supports_download: false,
            kind: ServerKind::Unknown,
            loaded: loaded.iter().map(|m| (*m).to_owned()).collect(),
            context_window: None,
        }
    }

    #[test]
    fn lists_every_server_with_models_and_what_is_loaded() {
        let mut ollama = service(
            "Ollama",
            11434,
            &["qwen3.6:35b-a3b-nvfp4", "a", "b", "c", "d", "e"],
            &["qwen3.6:35b-a3b-nvfp4"],
        );
        ollama.context_window = Some(65_536);
        let llama = service(
            "llama.cpp\u{1b}[2J",
            18040,
            &["qwen3.8-flash-next-iq2_xs"],
            &["qwen3.8-flash-next-iq2_xs"],
        );
        let text = describe(&[ollama, llama]);
        assert_eq!(
            text,
            "Found on this computer:
  1. Ollama  http://127.0.0.1:11434/v1
     models: qwen3.6:35b-a3b-nvfp4, a, b, c, d (+1 more)
     loaded: qwen3.6:35b-a3b-nvfp4 · 65536-token context
  2. llama.cpp[2J  http://127.0.0.1:18040/v1
     models: qwen3.8-flash-next-iq2_xs
     loaded: qwen3.8-flash-next-iq2_xs"
        );
        assert_eq!(describe(&[]), "No model servers found on this computer.");
    }

    #[test]
    fn a_number_picks_a_server_and_its_model() {
        let services = [
            service("Ollama", 11434, &["a", "b"], &["b"]),
            service("vLLM", 8000, &["m"], &[]),
        ];
        assert_eq!(
            chosen(&services, " 2 ").unwrap().endpoint,
            "http://127.0.0.1:8000/v1"
        );
        for other in ["0", "3", "http://127.0.0.1:1/v1", ""] {
            assert!(chosen(&services, other).is_none(), "{other}");
        }
        assert_eq!(offered_model(&services[0], "a"), "a");
        assert_eq!(offered_model(&services[0], "missing"), "b");
        assert_eq!(offered_model(&services[1], "missing"), "m");
        assert_eq!(
            offered_model(&service("x", 1, &[], &[]), "current"),
            "current"
        );
    }
}
