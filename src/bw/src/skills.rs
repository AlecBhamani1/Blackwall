//! Owner review of learned skill proposals, and imports from other harnesses. These never call a
//! model.
use blackwall_core::{skill_import, skills::SkillStore};
use std::path::{Path, PathBuf};

const USAGE: &str =
    "Use bw skills pending|approve <id>|reject <id>, or bw skills import [<path>] [--replace].";

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
        "import" => return import(&store, &args[1..]),
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

/// Without a path, list skills found in other harnesses' folders. With one, import every skill
/// there, keeping saved skills of the same name unless `--replace` is given.
fn import(store: &SkillStore, args: &[String]) -> Result<(), String> {
    let replace = args.iter().any(|arg| arg == "--replace");
    let paths: Vec<_> = args.iter().filter(|arg| *arg != "--replace").collect();
    let mut found = match paths.as_slice() {
        [] => {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
                .ok_or("Your home folder could not be found. Use bw skills import <path>.")?;
            skill_import::known_folders(&home)
                .into_iter()
                .flat_map(|(origin, folder)| skill_import::scan(origin, &folder))
                .collect()
        }
        [path] => skill_import::scan("Chosen", Path::new(path)),
        _ => return Err(USAGE.into()),
    };
    skill_import::mark(store, &mut found);
    if found.is_empty() {
        println!(
            "No skills found. A skill is a SKILL.md file, a folder with one, or a folder of them."
        );
        return Ok(());
    }
    let (mut imported, mut skipped) = (0, 0);
    for candidate in &found {
        let Some(skill) = &candidate.skill else {
            println!(
                "{}: {}",
                candidate.source,
                candidate.error.as_deref().unwrap_or("Not a skill.")
            );
            skipped += 1;
            continue;
        };
        if paths.is_empty() {
            let saved = if candidate.exists {
                " (a skill with this name is saved)"
            } else {
                ""
            };
            println!(
                "{} [{}] {}{saved}\n  {}",
                skill.name, candidate.origin, skill.description, candidate.source
            );
        } else {
            match skill_import::import(store, Path::new(&candidate.source), replace) {
                Ok(skill) => {
                    println!("Imported {}.", skill.name);
                    imported += 1;
                }
                Err(blackwall_core::skills::SkillError::Exists) => {
                    println!(
                        "Skipped {}: a skill with this name is saved. Use --replace.",
                        skill.name
                    );
                    skipped += 1;
                    continue;
                }
                Err(error) => {
                    println!("Skipped {}: {error}", skill.name);
                    skipped += 1;
                    continue;
                }
            }
        }
        for note in &candidate.notes {
            println!("  {note}");
        }
    }
    if paths.is_empty() {
        println!("Import one with bw skills import <path>.");
    } else {
        println!("Imported {imported}, skipped {skipped}.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_keeps_saved_skills_unless_replacing() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "blackwall-skill-import-{}-{unique}",
            std::process::id()
        ));
        let source = root.join("source/review");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: review\ndescription: Review changes.\n---\nRead the diff.",
        )
        .unwrap();
        let store = SkillStore::open(&root.join("skills")).unwrap();
        let path = root.join("source").to_string_lossy().into_owned();
        import(&store, std::slice::from_ref(&path)).unwrap();
        assert_eq!(store.read("review").unwrap().body, "Read the diff.");
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: review\ndescription: Review changes.\n---\nRead it twice.",
        )
        .unwrap();
        import(&store, std::slice::from_ref(&path)).unwrap();
        assert_eq!(store.read("review").unwrap().body, "Read the diff.");
        import(&store, &[path, "--replace".into()]).unwrap();
        assert_eq!(store.read("review").unwrap().body, "Read it twice.");
        assert!(import(&store, &["a".into(), "b".into()]).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
