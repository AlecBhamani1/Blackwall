//! Local owner search; no model requests and no replay of historical actions.
use blackwall_core::{
    memory::MemoryClient,
    storage::{LocalStore, RecallQuery},
};
const USAGE: &str = "Use bw sessions search <words> [--project <path>] [--after <unix-ms>] [--before <unix-ms>] [--limit <1-10>].";

pub fn parse(args: &[String]) -> Result<RecallQuery, String> {
    let mut query = RecallQuery::default();
    let mut words = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--project" => query.project = Some(args.next().ok_or(USAGE)?.clone()),
            "--after" => query.after = Some(args.next().ok_or(USAGE)?.parse().map_err(|_| USAGE)?),
            "--before" => {
                query.before = Some(args.next().ok_or(USAGE)?.parse().map_err(|_| USAGE)?)
            }
            "--limit" => query.limit = Some(args.next().ok_or(USAGE)?.parse().map_err(|_| USAGE)?),
            flag if flag.starts_with("--") => return Err(USAGE.into()),
            word => words.push(word),
        }
    }
    query.query = words.join(" ");
    if query.query.is_empty() {
        return Err(USAGE.into());
    }
    Ok(query)
}
/// Minimal quoted arguments for interactive project paths; never invokes a shell.
pub fn line(value: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    for c in value.chars() {
        if quote == Some(c) {
            quote = None;
        } else if quote.is_some() {
            word.push(c);
        } else if matches!(c, '\'' | '"') {
            quote = Some(c);
        } else if c.is_whitespace() {
            if !word.is_empty() {
                args.push(std::mem::take(&mut word));
            }
        } else {
            word.push(c);
        }
    }
    if quote.is_some() {
        return Err("Close the quoted search argument.".into());
    }
    if !word.is_empty() {
        args.push(word);
    }
    Ok(args)
}
fn terminal(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}
pub fn search(store: &LocalStore, args: &[String]) -> Result<(), String> {
    let results = store
        .search_messages(MemoryClient::Cli, &parse(args)?)
        .map_err(|e| e.to_string())?;
    if results.results.is_empty() {
        println!("No matching saved messages.");
    }
    for hit in results.results {
        println!(
            "{} / {} (message {}) [{}] {} — {}\n{}\n{}",
            terminal(&hit.session_id),
            terminal(&hit.message_id),
            hit.message_index + 1,
            terminal(&hit.mode),
            terminal(&hit.title),
            hit.updated_at,
            terminal(&hit.excerpt),
            if hit.mode == "agent" {
                format!(
                    "Resume: bw resume {} (from its saved project)",
                    terminal(&hit.session_id)
                )
            } else {
                "Open this Chat conversation in desktop history.".into()
            }
        );
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_filters_and_rejects_missing_values() {
        let args = [
            "saved",
            "words",
            "--project",
            "/tmp/my project",
            "--after",
            "123",
            "--before",
            "456",
            "--limit",
            "2",
        ]
        .map(str::to_owned);
        let query = parse(&args).unwrap();
        assert_eq!(query.query, "saved words");
        assert_eq!(query.project.as_deref(), Some("/tmp/my project"));
        assert_eq!(
            (query.after, query.before, query.limit),
            (Some(123), Some(456), Some(2))
        );
        for args in [
            vec!["--after"],
            vec!["--limit", "wrong"],
            vec!["--unknown"],
            vec![],
        ] {
            assert!(parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
        }
        let query = parse(&line("saved words --project '/tmp/my project'").unwrap()).unwrap();
        assert_eq!(query.project.as_deref(), Some("/tmp/my project"));
        assert!(line("words --project 'unfinished").is_err());
        assert_eq!(terminal("\u{1b}[31mhello\n"), "[31mhello");
    }
}
