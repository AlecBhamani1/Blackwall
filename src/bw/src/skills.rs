//! Owner review of learned skill proposals, and imports from other harnesses. These never call a
//! model.
use crate::project::safe;
use blackwall_core::{
    skill_import, skill_suggest,
    skills::{SkillError, SkillStore},
};
use std::path::{Path, PathBuf};

const USAGE: &str = "Use bw skills list|remove <name>|pending|approve <id>|reject <id>, bw skills import [<path>], bw skills suggested, or bw skills get <name>. Add --replace to overwrite a saved skill and --allow-anyway to import one with scan warnings.";

pub async fn run(directory: &Path, args: &[String]) -> Result<(), String> {
    let local = blackwall_core::storage::LocalStore::open(directory).map_err(|e| e.to_string())?;
    let max_skill_kib = match crate::settings::Settings::load(&local) {
        Ok(settings) => settings
            .instructions
            .effective_skill_kib(settings.budget.context_window),
        Err(error) => {
            eprintln!("Warning: {error} Using the default skill size limit.");
            blackwall_core::instructions::InstructionSettings::default()
                .effective_skill_kib_or_default_window()
        }
    };
    let store = SkillStore::open_with_limit(&directory.join("skills"), max_skill_kib)
        .map_err(|e| e.to_string())?;
    let id = || args.get(1).map(String::as_str).unwrap_or_default();
    match args.first().map(String::as_str).unwrap_or("pending") {
        "list" if args.len() == 1 => {
            let (skills, warnings) = store.list().map_err(|e| e.to_string())?;
            if skills.is_empty() {
                println!("No saved skills.");
            }
            for entry in skills {
                if let Some(warning) = entry.warning {
                    println!("{} — {warning}", entry.skill.name);
                } else {
                    println!("{} — {}", entry.skill.name, safe(&entry.skill.description));
                }
            }
            for warning in warnings {
                eprintln!("{}", warning_line("Skill", &warning));
            }
        }
        "remove" if args.len() == 2 => {
            store.remove(id()).map_err(|e| e.to_string())?;
            println!("Skill {} removed.", id());
        }
        "pending" if args.len() <= 1 => {
            let (proposals, warnings) = store.proposals().map_err(|e| e.to_string())?;
            for warning in warnings {
                eprintln!("{}", warning_line("Skill proposal", &warning));
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
                    "{} [{kind}] {}\nSource: session {} / message {}\n{}\n{}",
                    proposal.id,
                    proposal.skill.name,
                    proposal.source.session_id,
                    proposal.source.message_id,
                    scan_summary(&proposal.scan()),
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
        "suggested" if args.len() == 1 => suggested(&store),
        "get" => return get(&store, &args[1..]).await,
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

/// A store warning for stderr. Warnings can quote file names from the skills folder.
fn warning_line(label: &str, warning: &str) -> String {
    format!("{label}: {}", safe(warning))
}

/// A candidate with its license, bundled files, scan result, and conversion notes. Skill text
/// comes from third parties, so it is filtered before it reaches the terminal.
fn describe(candidate: &skill_import::ImportCandidate) -> String {
    let Some(skill) = &candidate.skill else {
        return safe(&format!(
            "{}: {}",
            candidate.source,
            candidate.error.as_deref().unwrap_or("Not a skill.")
        ));
    };
    let saved = if candidate.exists {
        " (a skill with this name is saved)"
    } else {
        ""
    };
    let mut text = format!(
        "{} [{}] {}{saved}\n  {}",
        skill.name, candidate.origin, skill.description, candidate.source
    );
    if let Some(license) = &candidate.license {
        text.push_str(&format!("\n  License: {license}"));
    }
    if !candidate.files.is_empty() {
        text.push_str(&format!(
            "\n  Bundled files ({}): {}",
            candidate.files.len(),
            candidate
                .files
                .iter()
                .take(8)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let mut text = safe(&text);
    text.push('\n');
    text.push_str(&scan_summary(&candidate.findings));
    for note in &candidate.notes {
        text.push_str(&format!("\n  {}", safe(note)));
    }
    text
}

/// Import one candidate, printing the outcome. Returns whether it was imported.
fn import_one(
    store: &SkillStore,
    candidate: &skill_import::ImportCandidate,
    replace: bool,
    allow: bool,
) -> bool {
    let Some(skill) = &candidate.skill else {
        return false;
    };
    match skill_import::import(store, Path::new(&candidate.source), replace, allow) {
        Ok(skill) => {
            println!("Imported {}.", skill.name);
            true
        }
        Err(SkillError::Exists) => {
            println!(
                "Skipped {}: a skill with this name is saved. Use --replace.",
                skill.name
            );
            false
        }
        Err(SkillError::Flagged) => {
            println!(
                "Blocked {}: the safety scan found warnings. Review them, then use --allow-anyway to import it.",
                skill.name
            );
            false
        }
        Err(error) => {
            println!("Skipped {}: {error}", skill.name);
            false
        }
    }
}

/// Without a path, list skills found in other harnesses' folders. With one, import every skill
/// there. Saved skills of the same name are kept unless `--replace` is given, and skills with scan
/// findings are blocked unless `--allow-anyway` is given.
/// The scan result as printed for an import candidate or a proposal: a clean line, or each finding.
fn scan_summary(findings: &[blackwall_core::skill_scan::Finding]) -> String {
    if findings.is_empty() {
        return "  Scan: no issues found".into();
    }
    let mut text = format!("  Scan: {} warning(s)", findings.len());
    for finding in findings {
        text.push_str(&format!(
            "\n    [{}] {}:{} {}\n      {}",
            serde_json::to_value(finding.severity)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default(),
            safe(&finding.file),
            finding.line,
            finding.reason,
            crate::memory::visible(&finding.excerpt)
        ));
    }
    text
}

fn import(store: &SkillStore, args: &[String]) -> Result<(), String> {
    let replace = args.iter().any(|arg| arg == "--replace");
    let allow = args.iter().any(|arg| arg == "--allow-anyway");
    let paths: Vec<_> = args
        .iter()
        .filter(|arg| !matches!(arg.as_str(), "--replace" | "--allow-anyway"))
        .collect();
    let mut found = match paths.as_slice() {
        [] => {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
                .ok_or("Your home folder could not be found. Use bw skills import <path>.")?;
            skill_import::known_folders(&home)
                .into_iter()
                .flat_map(|(origin, folder)| {
                    skill_import::scan_with_limit(origin, &folder, store.max_skill_kib())
                })
                .collect()
        }
        [path] => skill_import::scan_with_limit("Chosen", Path::new(path), store.max_skill_kib()),
        _ => return Err(USAGE.into()),
    };
    skill_import::mark(store, &mut found);
    if found.is_empty() {
        println!(
            "No skills found. A skill is a SKILL.md file, a folder with one, or a folder of them."
        );
        return Ok(());
    }
    let mut imported = 0;
    for candidate in &found {
        println!("{}", describe(candidate));
        if !paths.is_empty() && import_one(store, candidate, replace, allow) {
            imported += 1;
        }
    }
    if paths.is_empty() {
        println!("Import with bw skills import <path>, or see bw skills suggested.");
    } else {
        println!("Imported {imported} of {}.", found.len());
    }
    Ok(())
}

/// List the suggested skills, marking ones already saved.
fn suggested(store: &SkillStore) {
    for suggested in skill_suggest::SUGGESTED {
        let saved = if store.contains(suggested.name) {
            " (saved)"
        } else {
            ""
        };
        println!(
            "{}{saved}\n  {}\n  {} · {}",
            suggested.name, suggested.description, suggested.repository, suggested.license
        );
    }
    println!("Download, scan, and import one with bw skills get <name>.");
}

/// Download a suggested skill at its reviewed version, show the scan, and import it.
async fn get(store: &SkillStore, args: &[String]) -> Result<(), String> {
    let replace = args.iter().any(|arg| arg == "--replace");
    let allow = args.iter().any(|arg| arg == "--allow-anyway");
    let names: Vec<_> = args
        .iter()
        .filter(|arg| !matches!(arg.as_str(), "--replace" | "--allow-anyway"))
        .collect();
    let [name] = names.as_slice() else {
        return Err(USAGE.into());
    };
    let suggested = skill_suggest::find(name)
        .ok_or("No suggested skill has that name. See bw skills suggested.")?;
    println!(
        "Downloading {} from {} at its reviewed version…",
        suggested.name, suggested.repository
    );
    let source = skill_suggest::download(suggested, &std::env::temp_dir()).await?;
    let mut found =
        skill_import::scan_with_limit(suggested.repository, &source, store.max_skill_kib());
    skill_import::mark(store, &mut found);
    let candidate = found.first();
    if let Some(candidate) = candidate {
        println!("{}", describe(candidate));
        import_one(store, candidate, replace, allow);
    }
    skill_suggest::discard(&source);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_strips_terminal_escapes_from_skill_text() {
        let hidden = "\u{1b}[8m";
        let candidate = skill_import::ImportCandidate {
            origin: "Chosen".into(),
            source: format!("/tmp/{hidden}x/SKILL.md"),
            skill: Some(blackwall_core::skills::Skill {
                name: "review-diff".into(),
                description: format!("Review changes.{hidden}"),
                body: "Read the diff.".into(),
                enabled: true,
                user_only: false,
            }),
            error: None,
            notes: vec![format!("Converted.{hidden}")],
            exists: false,
            files: vec![format!("{hidden}notes.md")],
            license: Some(format!("MIT{hidden}")),
            findings: Vec::new(),
        };
        let text = describe(&candidate);
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(text.contains("Review changes.[8m") && text.contains("Scan: no issues found"));
    }

    #[test]
    // Windows file names cannot contain control characters, so this attack needs Unix.
    #[cfg(unix)]
    fn store_warnings_about_escaped_file_names_are_filtered() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "blackwall-skill-warnings-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join(".proposals")).unwrap();
        std::fs::write(root.join("\u{1b}[8m.md"), "hidden").unwrap();
        std::fs::write(root.join(".proposals/\u{1b}[8m.json"), "{}").unwrap();
        let store = SkillStore::open_with_limit(&root, 32).unwrap();
        let (_, listed) = store.list().unwrap();
        let (_, pending) = store.proposals().unwrap();
        assert!(listed[0].contains('\u{1b}') && pending[0].contains('\u{1b}'));
        for warning in listed.iter().chain(&pending) {
            let line = warning_line("Skill", warning);
            assert!(!line.contains('\u{1b}') && line.contains("[8m"), "{line:?}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

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
        let source = root.join("source/review-diff");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: review-diff\ndescription: Review changes.\n---\nRead the diff.",
        )
        .unwrap();
        let store = SkillStore::open_with_limit(&root.join("skills"), 32).unwrap();
        let path = root.join("source").to_string_lossy().into_owned();
        import(&store, std::slice::from_ref(&path)).unwrap();
        assert_eq!(store.read("review-diff").unwrap().body, "Read the diff.");
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: review-diff\ndescription: Review changes.\n---\nRead it twice.",
        )
        .unwrap();
        import(&store, std::slice::from_ref(&path)).unwrap();
        assert_eq!(store.read("review-diff").unwrap().body, "Read the diff.");
        import(&store, &[path, "--replace".into()]).unwrap();
        assert_eq!(store.read("review-diff").unwrap().body, "Read it twice.");
        assert!(import(&store, &["a".into(), "b".into()]).is_err());
        drop(store); // Windows cannot remove a folder the store still holds open.
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn remove_works_with_invalid_unrelated_settings() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "blackwall-skill-remove-invalid-settings-{}-{unique}",
            std::process::id()
        ));
        let local = blackwall_core::storage::LocalStore::open(&root).unwrap();
        local
            .save_setting("cli-settings", r#"{"webEnabled":"not-a-boolean"}"#)
            .unwrap();
        let store = SkillStore::open_with_limit(&root.join("skills"), 32).unwrap();
        store
            .save(&blackwall_core::skills::Skill {
                name: "removable".into(),
                description: "Remove me.".into(),
                body: "Body".into(),
                enabled: true,
                user_only: false,
            })
            .unwrap();
        drop(store);
        run(&root, &["remove".into(), "removable".into()])
            .await
            .unwrap();
        assert!(!root.join("skills/removable.md").exists());
        drop(local);
        std::fs::remove_dir_all(root).unwrap();
    }
}
