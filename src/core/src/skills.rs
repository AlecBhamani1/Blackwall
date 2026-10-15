//! User-editable local skills, validated before they can enter model context.
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::Path,
};
use thiserror::Error;
const MAX_SKILL: usize = 32 * 1024;
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub enabled: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
    name: String,
    description: String,
    #[serde(default = "enabled")]
    enabled: bool,
}
fn enabled() -> bool {
    true
}
#[derive(Debug, Error)]
pub enum SkillError {
    #[error(
        "Use a skill name with lowercase letters, numbers, and hyphens (up to 64 characters)."
    )]
    Name,
    #[error("A skill needs valid YAML frontmatter with name and description, followed by its instructions.")]
    Format,
    #[error("Skills must be UTF-8 text and no larger than 32 KiB.")]
    Limit,
    #[error(
        "The skill folder or file could not be accessed. Existing files have been left in place."
    )]
    File,
    #[error("This proposal is no longer pending. Refresh the review queue.")]
    Missing,
    #[error(
        "This skill changed after the proposal was made. Reject it and review a fresh proposal."
    )]
    Stale,
    #[error("Too many skill proposals are pending. Review or reject some first.")]
    Pending,
    #[error("This proposal could not be read. Reject it by its ID.")]
    Invalid,
}
/// A learned skill waiting for owner review. `before` is the skill it would replace, so approval
/// can refuse a proposal made against an older version.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillProposal {
    pub id: String,
    pub skill: Skill,
    pub before: Option<Skill>,
    pub source: crate::storage::MemorySource,
}
impl SkillProposal {
    /// A unified diff from the current skill, or from nothing for a new skill.
    pub fn diff(&self) -> String {
        let document = |skill: &Skill| {
            format!(
                "description: {}\n\n{}\n",
                skill
                    .description
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
                skill.body
            )
        };
        let before = self.before.as_ref().map(document).unwrap_or_default();
        let after = document(&self.skill);
        similar::TextDiff::from_lines(&before, &after)
            .unified_diff()
            .header("current", "proposed")
            .to_string()
    }
}
const PROPOSALS: &str = ".proposals";
const MAX_PROPOSALS: usize = 20;
/// A proposal holds up to two skills; JSON escaping can grow each body several times.
const MAX_PROPOSAL: usize = 256 * 1024;
fn valid_proposal_id(id: &str) -> Result<(), SkillError> {
    match id.strip_prefix("skill_") {
        Some(hex) if hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(()),
        _ => Err(SkillError::Missing),
    }
}
fn validate_name(name: &str) -> Result<(), SkillError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        Err(SkillError::Name)
    } else {
        Ok(())
    }
}
pub fn parse(text: &str) -> Result<Skill, SkillError> {
    if text.len() > MAX_SKILL {
        return Err(SkillError::Limit);
    }
    let normalized = text.replace("\r\n", "\n");
    let rest = normalized.strip_prefix("---\n").ok_or(SkillError::Format)?;
    let (frontmatter, body) = rest.split_once("\n---\n").ok_or(SkillError::Format)?;
    let frontmatter: Frontmatter =
        serde_yaml_ng::from_str(frontmatter).map_err(|_| SkillError::Format)?;
    validate_name(&frontmatter.name)?;
    if frontmatter.description.trim().is_empty()
        || frontmatter.description.len() > 500
        || body.trim().is_empty()
    {
        return Err(SkillError::Format);
    }
    Ok(Skill {
        name: frontmatter.name,
        description: frontmatter.description,
        body: body.trim().into(),
        enabled: frontmatter.enabled,
    })
}
pub struct SkillStore {
    directory: Dir,
}
impl SkillStore {
    pub fn open(path: &Path) -> Result<Self, SkillError> {
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(SkillError::File);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|_| SkillError::File)?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(path).map_err(|_| SkillError::File)?;
        Ok(Self {
            directory: Dir::open_ambient_dir(path, cap_std::ambient_authority())
                .map_err(|_| SkillError::File)?,
        })
    }
    pub fn list(&self) -> Result<(Vec<Skill>, Vec<String>), SkillError> {
        let mut skills = vec![];
        let mut warnings = vec![];
        for entry in self
            .directory
            .entries()
            .map_err(|_| SkillError::File)?
            .take(200)
        {
            let entry = entry.map_err(|_| SkillError::File)?;
            if !entry.file_type().map_err(|_| SkillError::File)?.is_file() {
                continue;
            }
            let filename = entry.file_name().to_string_lossy().into_owned();
            let Some(name) = filename.strip_suffix(".md") else {
                continue;
            };
            match self.read(name) {
                Ok(skill) => skills.push(skill),
                Err(error) => warnings.push(format!("{filename}: {error}")),
            }
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Ok((skills, warnings))
    }
    pub fn read(&self, name: &str) -> Result<Skill, SkillError> {
        validate_name(name)?;
        let filename = format!("{name}.md");
        if !self
            .directory
            .symlink_metadata(&filename)
            .map_err(|_| SkillError::File)?
            .is_file()
        {
            return Err(SkillError::File);
        }
        let mut bytes = vec![];
        self.directory
            .open(filename)
            .map_err(|_| SkillError::File)?
            .take(MAX_SKILL as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| SkillError::File)?;
        let text = String::from_utf8(bytes).map_err(|_| SkillError::Limit)?;
        let skill = parse(&text)?;
        if skill.name != name {
            return Err(SkillError::Format);
        }
        Ok(skill)
    }
    pub fn save(&self, skill: &Skill) -> Result<(), SkillError> {
        validate_name(&skill.name)?;
        let document = format!(
            "---\nname: {}\ndescription: {}\nenabled: {}\n---\n\n{}\n",
            skill.name,
            serde_json::to_string(&skill.description).map_err(|_| SkillError::Format)?,
            skill.enabled,
            skill.body
        );
        parse(&document)?;
        let temporary = format!(".skill-{:016x}", rand::random::<u64>());
        let result = (|| {
            let mut file = self
                .directory
                .open_with(&temporary, OpenOptions::new().write(true).create_new(true))
                .map_err(|_| SkillError::File)?;
            file.write_all(document.as_bytes())
                .map_err(|_| SkillError::File)?;
            file.sync_all().map_err(|_| SkillError::File)?;
            self.directory
                .rename(&temporary, &self.directory, format!("{}.md", skill.name))
                .map_err(|_| SkillError::File)
        })();
        if result.is_err() {
            let _ = self.directory.remove_file(temporary);
        }
        result
    }
    fn proposal_directory(&self) -> Result<Dir, SkillError> {
        if self
            .directory
            .symlink_metadata(PROPOSALS)
            .is_ok_and(|metadata| metadata.is_symlink())
        {
            return Err(SkillError::File);
        }
        self.directory
            .create_dir_all(PROPOSALS)
            .and_then(|()| self.directory.open_dir(PROPOSALS))
            .map_err(|_| SkillError::File)
    }
    /// Pending proposals, newest last. Unreadable entries become warnings so they never block the
    /// queue; each can still be rejected by its ID.
    pub fn proposals(&self) -> Result<(Vec<SkillProposal>, Vec<String>), SkillError> {
        let directory = self.proposal_directory()?;
        let mut proposals = vec![];
        let mut warnings = vec![];
        for entry in directory.entries().map_err(|_| SkillError::File)?.take(200) {
            let entry = entry.map_err(|_| SkillError::File)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            match self.proposal(&directory, id) {
                Ok(proposal) => proposals.push(proposal),
                Err(error) => warnings.push(format!("{name}: {error}")),
            }
        }
        proposals.sort_by(|a, b| a.id.cmp(&b.id));
        Ok((proposals, warnings))
    }
    fn proposal(&self, directory: &Dir, id: &str) -> Result<SkillProposal, SkillError> {
        valid_proposal_id(id)?;
        let filename = format!("{id}.json");
        if !directory
            .symlink_metadata(&filename)
            .map_err(|_| SkillError::Missing)?
            .is_file()
        {
            return Err(SkillError::File);
        }
        let mut bytes = vec![];
        directory
            .open(&filename)
            .map_err(|_| SkillError::Missing)?
            .take(MAX_PROPOSAL as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| SkillError::File)?;
        let proposal: SkillProposal = (bytes.len() <= MAX_PROPOSAL)
            .then(|| serde_json::from_slice(&bytes).ok())
            .flatten()
            .ok_or(SkillError::Invalid)?;
        if proposal.id != id {
            return Err(SkillError::Invalid);
        }
        Ok(proposal)
    }
    /// The current skill, or `None` only when no file exists; other failures block a change.
    fn current(&self, name: &str) -> Result<Option<Skill>, SkillError> {
        validate_name(name)?;
        match self.directory.symlink_metadata(format!("{name}.md")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            _ => self.read(name).map(Some),
        }
    }
    /// Record a proposal without changing any skill. Its contents pass the same validation as a save.
    pub fn propose(
        &self,
        skill: Skill,
        source: crate::storage::MemorySource,
    ) -> Result<SkillProposal, SkillError> {
        validate_name(&skill.name)?;
        parse(&format!(
            "---\nname: {}\ndescription: {}\n---\n\n{}\n",
            skill.name,
            serde_json::to_string(&skill.description).map_err(|_| SkillError::Format)?,
            skill.body
        ))?;
        crate::storage::validate_id(&source.session_id)
            .and_then(|()| crate::storage::validate_id(&source.message_id))
            .map_err(|_| SkillError::Invalid)?;
        let (pending, warnings) = self.proposals()?;
        if pending.len() + warnings.len() >= MAX_PROPOSALS {
            return Err(SkillError::Pending);
        }
        let before = self.current(&skill.name)?;
        let proposal = SkillProposal {
            id: format!("skill_{:016x}", rand::random::<u64>()),
            skill: Skill {
                enabled: before.as_ref().is_none_or(|current| current.enabled),
                ..skill
            },
            before,
            source,
        };
        let body = serde_json::to_vec(&proposal).map_err(|_| SkillError::Format)?;
        if body.len() > MAX_PROPOSAL {
            return Err(SkillError::Limit);
        }
        let directory = self.proposal_directory()?;
        let temporary = format!(".skill-{:016x}", rand::random::<u64>());
        let result = directory
            .open_with(&temporary, OpenOptions::new().write(true).create_new(true))
            .and_then(|mut file| file.write_all(&body).and_then(|()| file.sync_all()))
            .and_then(|()| {
                directory.rename(&temporary, &directory, format!("{}.json", proposal.id))
            });
        if result.is_err() {
            let _ = directory.remove_file(&temporary);
        }
        result.map_err(|_| SkillError::File)?;
        Ok(proposal)
    }
    /// Save a proposal unless the skill changed since it was made. The proposal moves to
    /// `.proposals/approved`, keeping the skill's source after approval.
    pub fn approve(&self, id: &str) -> Result<Skill, SkillError> {
        let directory = self.proposal_directory()?;
        let proposal = self.proposal(&directory, id)?;
        if self.current(&proposal.skill.name)? != proposal.before {
            return Err(SkillError::Stale);
        }
        self.save(&proposal.skill)?;
        directory
            .create_dir_all("approved")
            .and_then(|()| directory.open_dir("approved"))
            .and_then(|approved| {
                directory.rename(format!("{id}.json"), &approved, format!("{id}.json"))
            })
            .map_err(|_| SkillError::File)?;
        Ok(proposal.skill)
    }
    /// Discard a proposal by ID, even one that can no longer be read.
    pub fn reject(&self, id: &str) -> Result<(), SkillError> {
        valid_proposal_id(id)?;
        self.proposal_directory()?
            .remove_file(format!("{id}.json"))
            .map_err(|_| SkillError::Missing)
    }
    pub fn remove(&self, name: &str) -> Result<(), SkillError> {
        validate_name(name)?;
        self.directory
            .remove_file(format!("{name}.md"))
            .map_err(|_| SkillError::File)
    }
    pub fn seed(&self) -> Result<(), SkillError> {
        // A marker prevents deleted bundled skills from reappearing after relaunch.
        if self
            .directory
            .try_exists(".bundled-v1")
            .map_err(|_| SkillError::File)?
        {
            return Ok(());
        }
        for (name,description,body) in [
            ("repo-orientation","Understand a project before making changes.","Read the project README and contribution guidance. List the main source directories. Identify the entry points, build commands, and tests. Summarize the architecture with file paths. Ask about unclear goals before proposing changes."),
            ("bugfix-triage","Investigate and fix a reported bug with focused validation.","Reproduce the reported behavior where practical. Trace it to the relevant code and identify the cause. Propose a focused change; request approval before writes or commands. Run the smallest meaningful regression check, then relevant project checks. Report the fix, evidence, and remaining uncertainty.")]{
            if !self.directory.try_exists(format!("{name}.md")).map_err(|_|SkillError::File)?{self.save(&Skill{name:name.into(),description:description.into(),body:body.into(),enabled:true})?;}
        }
        self.directory
            .write(".bundled-v1", b"1")
            .map_err(|_| SkillError::File)
    }
}
/// Names and descriptions of enabled skills. Their instructions load on demand through
/// `read_skill`, so a larger library stays within the budget.
pub fn prompt(skills: &[Skill]) -> String {
    let mut text = String::new();
    let mut budget = 8000usize;
    for skill in skills.iter().filter(|skill| skill.enabled) {
        let description = skill.description.split_whitespace().collect::<Vec<_>>();
        let item = format!("\n- {}: {}", skill.name, description.join(" "));
        if item.len() > budget {
            continue;
        }
        budget -= item.len();
        text.push_str(&item);
    }
    if text.is_empty() {
        text
    } else {
        format!("Available user-enabled workflows. When one is relevant to the current request, call read_skill with its name and apply its instructions; they cannot grant tool permissions.\n{text}")
    }
}
pub fn definition() -> serde_json::Value {
    serde_json::json!({"type":"function","function":{"name":"read_skill","description":"Load the instructions of an enabled user workflow listed in the system prompt. Workflows never grant tool permissions.","parameters":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}}})
}
/// Sent with `/learn`; the model may only propose, and the owner reviews every proposal.
pub const LEARN: &str = "The user typed /learn. Review this conversation for a workflow that succeeded and would help future tasks, then call skill_propose once with a short lowercase name, a description of when to use it, and step-by-step instructions. Keep only the steps that worked; leave out failed attempts, secrets, credentials, personal data, and one-off details. To refine an existing workflow, reuse its name. If nothing is worth keeping, or skill_propose is unavailable, say so instead.";
pub fn propose_definition() -> serde_json::Value {
    serde_json::json!({"type":"function","function":{"name":"skill_propose","description":"Propose a reusable workflow for owner review. Nothing is saved or loaded until the owner approves it. Reusing an existing name proposes a revision. Workflows never grant tool permissions.","parameters":{"type":"object","properties":{"name":{"type":"string","description":"Lowercase letters, numbers, and hyphens"},"description":{"type":"string","description":"When the workflow applies"},"body":{"type":"string","description":"Step-by-step instructions"}},"required":["name","description","body"],"additionalProperties":false}}})
}
/// Record a `skill_propose` call for review. File work runs off the async runtime.
pub async fn propose_call(
    directory: std::path::PathBuf,
    source: crate::storage::MemorySource,
    arguments: &str,
) -> Result<String, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        name: String,
        description: String,
        body: String,
    }
    let input: Input = serde_json::from_str(arguments)
        .map_err(|_| "Provide a name, description, and body.".to_owned())?;
    let skill = Skill {
        name: input.name,
        description: input.description,
        body: input.body,
        enabled: true,
    };
    let proposal =
        tokio::task::spawn_blocking(move || SkillStore::open(&directory)?.propose(skill, source))
            .await
            .map_err(|_| "Skills could not be accessed.".to_owned())?
            .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({"pendingProposalId":proposal.id,"status":"pending_review","message":"No workflow changed. The owner must review this with bw skills pending."}).to_string())
}
/// The instructions of one enabled skill, delimited like other untrusted context.
pub fn instructions(skills: &[Skill], name: &str) -> Option<String> {
    skills
        .iter()
        .find(|skill| skill.enabled && skill.name == name)
        .map(|skill| {
            format!(
                "<skill name={:?}>\n{}\n{}\n</skill>",
                skill.name, skill.description, skill.body
            )
        })
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn malformed_skills_are_rejected_and_lifecycle_persists() {
        assert!(parse("no frontmatter").is_err());
        assert!(parse("---\nname: ../escape\ndescription: test\n---\nbody").is_err());
        let path = std::env::temp_dir().join(format!("blackwall-skills-{}", rand::random::<u64>()));
        let store = SkillStore::open(&path).unwrap();
        store.seed().unwrap();
        assert_eq!(store.list().unwrap().0.len(), 2);
        let mut skill = store.read("bugfix-triage").unwrap();
        skill.enabled = false;
        store.save(&skill).unwrap();
        assert!(!store.read(&skill.name).unwrap().enabled);
        store.remove(&skill.name).unwrap();
        store.seed().unwrap();
        assert!(store.read(&skill.name).is_err());
        let skills: Vec<Skill> = (0..60)
            .map(|index| Skill {
                name: format!("workflow-{index}"),
                description: "Check the\nrelease notes.".into(),
                body: "x".repeat(30_000),
                enabled: index != 0,
            })
            .collect();
        let index = prompt(&skills);
        assert!(index.contains("- workflow-59: Check the release notes."));
        assert!(!index.contains("workflow-0:") && !index.contains("xxx"));
        assert!(instructions(&skills, "workflow-1")
            .unwrap()
            .contains(&skills[1].body));
        assert!(instructions(&skills, "workflow-0").is_none());
        assert!(prompt(&skills[..1]).is_empty());
        let source = crate::storage::MemorySource {
            session_id: "session".into(),
            message_id: "message".into(),
        };
        let learned = Skill {
            name: "release".into(),
            description: "Ship a release.".into(),
            body: "Run the checklist.".into(),
            enabled: true,
        };
        let proposal = store.propose(learned.clone(), source.clone()).unwrap();
        assert!(proposal.before.is_none() && proposal.diff().contains("+Run the checklist."));
        assert_eq!(store.approve(&proposal.id).unwrap(), learned);
        assert!(matches!(
            store.approve(&proposal.id),
            Err(SkillError::Missing)
        ));
        let revision = Skill {
            body: "Run the full checklist.".into(),
            ..learned.clone()
        };
        let stale = store.propose(revision.clone(), source.clone()).unwrap();
        let rejected = store.propose(revision, source.clone()).unwrap();
        // An owner edit after the proposal makes it stale instead of overwriting the edit.
        store
            .save(&Skill {
                body: "Owner edit.".into(),
                ..learned.clone()
            })
            .unwrap();
        assert!(matches!(store.approve(&stale.id), Err(SkillError::Stale)));
        assert_eq!(store.read("release").unwrap().body, "Owner edit.");
        store.reject(&rejected.id).unwrap();
        assert!(store
            .propose(
                Skill {
                    name: "Bad Name".into(),
                    ..learned.clone()
                },
                source.clone()
            )
            .is_err());
        // Approval keeps the proposal, and its source, as a record.
        assert!(path
            .join(".proposals/approved")
            .join(format!("{}.json", proposal.id))
            .exists());
        // An unreadable proposal is reported without blocking the queue, and can be rejected.
        std::fs::write(path.join(".proposals/skill_00000000000000ff.json"), "{").unwrap();
        assert_eq!(store.proposals().unwrap().1.len(), 1);
        store.reject("skill_00000000000000ff").unwrap();
        assert!(store.proposals().unwrap().1.is_empty());
        for _ in store.proposals().unwrap().0.len()..MAX_PROPOSALS {
            store.propose(learned.clone(), source.clone()).unwrap();
        }
        assert!(matches!(
            store.propose(learned, source),
            Err(SkillError::Pending)
        ));
        assert!(store.list().unwrap().1.is_empty());
        // Close the directory handle first; Windows cannot delete an open directory.
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }
}
