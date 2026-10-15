//! Owner review of learned skill proposals. These never call a model.
use blackwall_core::skills::SkillStore;
use std::path::Path;

const USAGE: &str = "Use bw skills pending|approve <id>|reject <id>.";

pub fn run(directory: &Path, args: &[String]) -> Result<(), String> {
    let store = SkillStore::open(&directory.join("skills")).map_err(|e| e.to_string())?;
    let id = || args.get(1).map(String::as_str).unwrap_or_default();
    match args.first().map(String::as_str).unwrap_or("pending") {
        "pending" if args.len() <= 1 => {
            let (proposals, warnings) = store.proposals().map_err(|e| e.to_string())?;
            for warning in warnings {
                eprintln!("Skill proposal: {warning}");
            }
            if proposals.is_empty() {
                println!("No pending skill proposals. Type /learn after a task to propose one.");
            }
            for proposal in proposals {
                let kind = if proposal.before.is_some() {
                    "revision"
                } else {
                    "new skill"
                };
                println!(
                    "{} [{kind}] {}\nSource: session {} / message {}\n{}",
                    proposal.id,
                    proposal.skill.name,
                    proposal.source.session_id,
                    proposal.source.message_id,
                    crate::memory::visible(&proposal.diff())
                );
            }
        }
        "approve" if args.len() == 2 => {
            let skill = store.approve(id()).map_err(|e| e.to_string())?;
            println!("Skill {} saved.", skill.name);
        }
        "reject" if args.len() == 2 => {
            store.reject(id()).map_err(|e| e.to_string())?;
            println!("Proposal rejected.");
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}
