//! Owner review commands. These never call a model and work while learning is disabled.
use blackwall_core::storage::LocalStore;
use std::path::Path;

const USAGE: &str = "Use bw memory list|pending|approve <id> [edited text]|reject <id>|edit <id> <text>|forget <id>.";
fn visible(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}
pub fn run(directory: &Path, args: &[String]) -> Result<(), String> {
    let mut store = LocalStore::open(directory).map_err(|e| e.to_string())?;
    let command = args.first().map(String::as_str).unwrap_or("list");
    let id = || args.get(1).map(String::as_str).ok_or(USAGE);
    match command {
        "list" if args.len() <= 1 => {
            let entries = store.memories("").map_err(|e| e.to_string())?;
            if entries.is_empty() {
                println!("No saved memories.");
            }
            for entry in entries {
                println!(
                    "{} [{}] key={} project={}\n{}",
                    entry.id,
                    entry.scope.label(),
                    visible(&entry.key),
                    visible(&entry.workspace),
                    visible(&entry.content)
                );
                if let Some(source) = entry.source {
                    println!(
                        "Source: session {} / message {}",
                        source.session_id, source.message_id
                    );
                }
            }
        }
        "pending" if args.len() == 1 => {
            let proposals = store.memory_proposals().map_err(|e| e.to_string())?;
            if proposals.is_empty() {
                println!("No pending memory proposals.");
            }
            for proposal in proposals {
                println!("{} [{}] key={} project={}\nSource: session {} / message {}\n--- Saved\n{}\n+++ Proposed\n{}", proposal.id, proposal.entry.scope.label(), visible(&proposal.entry.key), visible(&proposal.entry.workspace), proposal.source.session_id, proposal.source.message_id, visible(proposal.before.as_ref().map_or("(new fact)", |e| e.content.as_str())), visible(&proposal.entry.content));
            }
        }
        "approve" if args.len() >= 2 => {
            let edited = (args.len() > 2).then(|| args[2..].join(" "));
            store
                .approve_memory(id()?, edited.as_deref())
                .map_err(|e| e.to_string())?;
            println!("Memory approved.");
        }
        "reject" if args.len() == 2 => {
            store.reject_memory(id()?).map_err(|e| e.to_string())?;
            println!("Proposal rejected.");
        }
        "forget" if args.len() == 2 => {
            store.delete_memory(id()?).map_err(|e| e.to_string())?;
            println!("Memory forgotten, including pending replacements.");
        }
        "edit" if args.len() >= 3 => {
            let mut entry = store
                .memories("")
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|e| e.id == id().unwrap_or(""))
                .ok_or("Memory not found.")?;
            entry.content = args[2..].join(" ");
            store.save_memory(&entry).map_err(|e| e.to_string())?;
            println!("Memory edited. Older proposals cannot overwrite this edit.");
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}
