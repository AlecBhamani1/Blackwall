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
    /// Runs only when the user types `/name`; hidden from the model's list and `read_skill`.
    /// Saved as `disable-model-invocation: true`, the key other harnesses use.
    #[serde(default)]
    pub user_only: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
    name: String,
    description: String,
    #[serde(default = "enabled")]
    enabled: bool,
    #[serde(default, rename = "disable-model-invocation")]
    user_only: bool,
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
    #[error("A skill with this name already exists. Choose to replace it, or rename one of them.")]
    Exists,
    #[error("A skill folder can hold at most 200 files and 5 MB.")]
    Folder,
    #[error("The safety scan found something in this skill. Review the findings, then allow or block it.")]
    Flagged,
    #[error("This name is a built-in command, so typing it could not run the skill. Give the skill another name.")]
    Reserved,
}
/// Built-in commands of the CLI and desktop app. A skill cannot take one of these names, because
/// typing `/name` must run exactly one thing.
pub const COMMANDS: &[&str] = &[
    "commands",
    "compact",
    "context",
    "endpoint",
    "exit",
    "help",
    "init",
    "learn",
    "memory",
    "model",
    "new",
    "quit",
    "recall",
    "resume",
    "review",
    "sessions",
    "settings",
    "web",
    "workspace",
];
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
static APPROVALS: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Saving, installing, replacing, and removing skills run one at a time in a process.
static WRITES: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// A hidden folder name for staged or replaced skills, with the time it was made, so a sweep can
/// tell its age even when a moved folder keeps an older modification time.
fn leftover(prefix: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    format!("{prefix}{now}-{:016x}", rand::random::<u64>())
}
fn writing() -> std::sync::MutexGuard<'static, ()> {
    WRITES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
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
        user_only: frontmatter.user_only,
    })
}
/// The saved file for a skill, after the same checks as reading one back.
pub(crate) fn validate(skill: &Skill) -> Result<String, SkillError> {
    validate_name(&skill.name)?;
    if COMMANDS.contains(&skill.name.as_str()) {
        return Err(SkillError::Reserved);
    }
    let document = format!(
        "---\nname: {}\ndescription: {}\nenabled: {}\n{}---\n\n{}\n",
        skill.name,
        serde_json::to_string(&skill.description).map_err(|_| SkillError::Format)?,
        skill.enabled,
        if skill.user_only {
            "disable-model-invocation: true\n"
        } else {
            ""
        },
        skill.body
    );
    parse(&document)?;
    Ok(document)
}
/// A skill is `<name>.md`, or a folder `<name>/SKILL.md` with bundled files such as scripts and
/// references, as imported from other harnesses.
pub struct SkillStore {
    directory: Dir,
    path: std::path::PathBuf,
}
const SKILL_FILE: &str = "SKILL.md";
/// Bundled files a folder skill may hold, together.
pub const MAX_FOLDER_FILES: usize = 200;
pub const MAX_FOLDER_BYTES: u64 = 5 * 1024 * 1024;
/// One bundled file the agent can read through `read_skill`.
const MAX_BUNDLED_READ: u64 = 64 * 1024;
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
        let store = Self {
            directory: Dir::open_ambient_dir(path, cap_std::ambient_authority())
                .map_err(|_| SkillError::File)?,
            path: path.to_owned(),
        };
        store.sweep();
        Ok(store)
    }
    /// Remove staged or replaced folders an interrupted import left behind. Recent ones may belong
    /// to an import still running in another process, so only those over an hour old go.
    fn sweep(&self) {
        let Ok(entries) = self.directory.entries() else {
            return;
        };
        for entry in entries.filter_map(Result::ok).take(1000) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let made = name
                .strip_prefix(".import-")
                .or_else(|| name.strip_prefix(".replaced-"))
                .and_then(|rest| rest.split_once('-'))
                .and_then(|(seconds, _)| seconds.parse::<u64>().ok());
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs());
            let stale = made.is_some_and(|made| now.saturating_sub(made) > 3600);
            if stale && (name.starts_with(".import-") || name.starts_with(".replaced-")) {
                let _ = self
                    .directory
                    .remove_dir_all(&name)
                    .or_else(|_| self.directory.remove_file(&name));
            }
        }
    }
    /// The skill's file, relative to the store: `<name>.md`, or `<name>/SKILL.md` for a real
    /// folder (not a link).
    fn file_of(&self, name: &str) -> Option<String> {
        let single = format!("{name}.md");
        if self.directory.symlink_metadata(&single).is_ok() {
            return Some(single);
        }
        self.directory
            .symlink_metadata(name)
            .is_ok_and(|metadata| metadata.is_dir())
            .then(|| format!("{name}/{SKILL_FILE}"))
    }
    /// The folder of a folder skill, for running its bundled scripts.
    pub fn folder(&self, name: &str) -> Option<std::path::PathBuf> {
        validate_name(name).ok()?;
        self.file_of(name)
            .filter(|file| file.ends_with(SKILL_FILE))
            .map(|_| self.path.join(name))
    }
    /// Bundled files of a folder skill, as `/`-separated relative paths, sorted and bounded.
    pub fn files(&self, name: &str) -> Vec<String> {
        fn walk(directory: &Dir, prefix: &str, depth: usize, out: &mut Vec<String>) {
            let Ok(entries) = directory.entries() else {
                return;
            };
            let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                if out.len() >= MAX_FOLDER_FILES {
                    return;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let path = format!("{prefix}{name}");
                if kind.is_file() && path != SKILL_FILE {
                    out.push(path);
                } else if kind.is_dir() && depth < 8 {
                    if let Ok(child) = entry.open_dir() {
                        walk(&child, &format!("{path}/"), depth + 1, out);
                    }
                }
            }
        }
        let mut out = vec![];
        if self.folder(name).is_some() {
            if let Ok(folder) = self.directory.open_dir(name) {
                walk(&folder, "", 0, &mut out);
            }
        }
        out
    }
    /// One bundled text file of a folder skill, bounded. Paths stay inside the folder.
    pub fn read_file(&self, name: &str, file: &str) -> Result<String, SkillError> {
        let relative = Path::new(file);
        if self.folder(name).is_none()
            || file.is_empty()
            || relative
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(SkillError::File);
        }
        let folder = self
            .directory
            .open_dir(name)
            .map_err(|_| SkillError::File)?;
        if !folder
            .symlink_metadata(relative)
            .map_err(|_| SkillError::File)?
            .is_file()
        {
            return Err(SkillError::File);
        }
        let mut bytes = vec![];
        folder
            .open(relative)
            .map_err(|_| SkillError::File)?
            .take(MAX_BUNDLED_READ + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| SkillError::File)?;
        if bytes.len() as u64 > MAX_BUNDLED_READ {
            return Err(SkillError::Limit);
        }
        String::from_utf8(bytes).map_err(|_| SkillError::Limit)
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
            let kind = entry.file_type().map_err(|_| SkillError::File)?;
            let filename = entry.file_name().to_string_lossy().into_owned();
            let name = if kind.is_file() {
                match filename.strip_suffix(".md") {
                    Some(name) => name,
                    None => continue,
                }
            } else if kind.is_dir()
                && !filename.starts_with('.')
                && self
                    .directory
                    .try_exists(format!("{filename}/{SKILL_FILE}"))
                    .unwrap_or(false)
            {
                filename.as_str()
            } else {
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
        let filename = self.file_of(name).ok_or(SkillError::File)?;
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
    /// Whether a skill file or folder with this name exists, even one that cannot be read.
    pub fn contains(&self, name: &str) -> bool {
        validate_name(name).is_ok()
            && (self
                .directory
                .symlink_metadata(format!("{name}.md"))
                .is_ok()
                || self.directory.symlink_metadata(name).is_ok())
    }
    /// Save a new skill, refusing to replace any existing file with that name, even one written
    /// at the same moment.
    pub fn create(&self, skill: &Skill) -> Result<(), SkillError> {
        let _writing = writing();
        let document = validate(skill)?;
        if self.contains(&skill.name) {
            return Err(SkillError::Exists);
        }
        let temporary = self.write_temporary(&document)?;
        let linked =
            self.directory
                .hard_link(&temporary, &self.directory, format!("{}.md", skill.name));
        let _ = self.directory.remove_file(&temporary);
        linked.map_err(|error| match error.kind() {
            std::io::ErrorKind::AlreadyExists => SkillError::Exists,
            _ => SkillError::File,
        })
    }
    fn write_temporary(&self, document: &str) -> Result<String, SkillError> {
        Self::write_temporary_in(&self.directory, document)
    }
    fn write_temporary_in(directory: &Dir, document: &str) -> Result<String, SkillError> {
        let temporary = format!(".skill-{:016x}", rand::random::<u64>());
        let result = (|| {
            let mut file = directory
                .open_with(&temporary, OpenOptions::new().write(true).create_new(true))
                .map_err(|_| SkillError::File)?;
            file.write_all(document.as_bytes())
                .map_err(|_| SkillError::File)?;
            file.sync_all().map_err(|_| SkillError::File)
        })();
        if result.is_err() {
            let _ = directory.remove_file(&temporary);
        }
        result.map(|()| temporary)
    }
    pub fn save(&self, skill: &Skill) -> Result<(), SkillError> {
        let _writing = writing();
        let document = validate(skill)?;
        // A folder skill keeps its bundled files; only its SKILL.md changes.
        if self.folder(&skill.name).is_some() {
            let folder = self
                .directory
                .open_dir(&skill.name)
                .map_err(|_| SkillError::File)?;
            let temporary = Self::write_temporary_in(&folder, &document)?;
            let result = folder
                .rename(&temporary, &folder, SKILL_FILE)
                .map_err(|_| SkillError::File);
            if result.is_err() {
                let _ = folder.remove_file(temporary);
            }
            return result;
        }
        let temporary = self.write_temporary(&document)?;
        let result = self
            .directory
            .rename(&temporary, &self.directory, format!("{}.md", skill.name))
            .map_err(|_| SkillError::File);
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
        if self.file_of(name).is_none() {
            return Ok(None);
        }
        self.read(name).map(Some)
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
        self.approve_edited(id, None)
    }
    /// Approve with the owner's edited description and instructions in place of the proposed ones.
    /// The skill keeps the proposed name, and the edits pass the same validation as a save.
    pub fn approve_edited(
        &self,
        id: &str,
        edited: Option<(String, String)>,
    ) -> Result<Skill, SkillError> {
        // Approvals in this process run one at a time, so two cannot both pass the stale check.
        let _approving = APPROVALS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = self.proposal_directory()?;
        let proposal = self.proposal(&directory, id)?;
        if self.current(&proposal.skill.name)? != proposal.before {
            return Err(SkillError::Stale);
        }
        // A refined skill keeps running only when typed, if it did before.
        let keep = proposal
            .before
            .as_ref()
            .is_some_and(|before| before.user_only);
        let mut skill = match edited {
            Some((description, body)) => Skill {
                description,
                body,
                ..proposal.skill
            },
            None => proposal.skill,
        };
        skill.user_only |= keep;
        self.save(&skill)?;
        // The skill is saved; if its record cannot be kept, the proposal is still removed so it
        // does not stay pending and later look stale.
        let archived = directory
            .create_dir_all("approved")
            .and_then(|()| directory.open_dir("approved"))
            .and_then(|approved| {
                directory.rename(format!("{id}.json"), &approved, format!("{id}.json"))
            });
        if archived.is_err() {
            let _ = directory.remove_file(format!("{id}.json"));
        }
        Ok(skill)
    }
    /// Discard a proposal by ID, even one that can no longer be read.
    pub fn reject(&self, id: &str) -> Result<(), SkillError> {
        valid_proposal_id(id)?;
        self.proposal_directory()?
            .remove_file(format!("{id}.json"))
            .map_err(|_| SkillError::Missing)
    }
    pub fn remove(&self, name: &str) -> Result<(), SkillError> {
        let _writing = writing();
        validate_name(name)?;
        if self.folder(name).is_some() {
            return self
                .directory
                .remove_dir_all(name)
                .map_err(|_| SkillError::File);
        }
        self.directory
            .remove_file(format!("{name}.md"))
            .map_err(|_| SkillError::File)
    }
    /// A hidden folder to assemble a folder skill in before `install` moves it into place.
    pub fn staging(&self) -> Result<(String, Dir), SkillError> {
        let name = leftover(".import-");
        self.directory
            .create_dir(&name)
            .and_then(|()| self.directory.open_dir(&name))
            .map(|folder| (name, folder))
            .map_err(|_| SkillError::File)
    }
    /// Replace any saved skill of this name, file or folder, with a single-file skill. The old one
    /// is moved aside first and put back if the new one cannot be saved.
    pub fn replace_with_file(&self, skill: &Skill) -> Result<(), SkillError> {
        let _writing = writing();
        let document = validate(skill)?;
        let temporary = self.write_temporary(&document)?;
        let old = if self.folder(&skill.name).is_some() {
            Some(skill.name.clone())
        } else {
            self.contains(&skill.name)
                .then(|| format!("{}.md", skill.name))
        };
        let trash = leftover(".replaced-");
        if let Some(old) = &old {
            if self.directory.rename(old, &self.directory, &trash).is_err() {
                let _ = self.directory.remove_file(&temporary);
                return Err(SkillError::File);
            }
        }
        let moved = self
            .directory
            .rename(&temporary, &self.directory, format!("{}.md", skill.name))
            .map_err(|_| SkillError::File);
        match (&moved, &old) {
            (Ok(()), Some(_)) => {
                let _ = self
                    .directory
                    .remove_dir_all(&trash)
                    .or_else(|_| self.directory.remove_file(&trash));
            }
            (Err(_), Some(old)) => {
                let _ = self.directory.remove_file(&temporary);
                let _ = self.directory.rename(&trash, &self.directory, old);
            }
            (Err(_), None) => {
                let _ = self.directory.remove_file(&temporary);
            }
            (Ok(()), None) => {}
        }
        moved
    }
    /// Remove a staged folder that will not be installed.
    pub fn discard(&self, staged: &str) {
        if staged.starts_with(".import-") {
            let _ = self.directory.remove_dir_all(staged);
        }
    }
    /// Move a staged folder skill into place. A skill with this name is kept unless `replace`;
    /// a replaced one is moved aside first and removed once the new one is in place.
    pub fn install(&self, staged: &str, skill: &Skill, replace: bool) -> Result<(), SkillError> {
        let _writing = writing();
        let document = validate(skill)?;
        let result = (|| {
            // Scoped so the handle is closed before the rename: Windows cannot rename an open folder.
            {
                let folder = self
                    .directory
                    .open_dir(staged)
                    .map_err(|_| SkillError::File)?;
                folder
                    .write(SKILL_FILE, document.as_bytes())
                    .map_err(|_| SkillError::File)?;
            }
            let mut aside = None;
            if self.contains(&skill.name) {
                if !replace {
                    return Err(SkillError::Exists);
                }
                let old = if self.folder(&skill.name).is_some() {
                    skill.name.clone()
                } else {
                    format!("{}.md", skill.name)
                };
                let trash = leftover(".replaced-");
                self.directory
                    .rename(&old, &self.directory, &trash)
                    .map_err(|_| SkillError::File)?;
                aside = Some(trash);
            }
            let moved = self
                .directory
                .rename(staged, &self.directory, &skill.name)
                .map_err(|_| SkillError::File);
            if let Some(trash) = aside {
                if moved.is_ok() {
                    let _ = self
                        .directory
                        .remove_dir_all(&trash)
                        .or_else(|_| self.directory.remove_file(&trash));
                } else {
                    let old = if self
                        .directory
                        .symlink_metadata(&trash)
                        .is_ok_and(|m| m.is_dir())
                    {
                        skill.name.clone()
                    } else {
                        format!("{}.md", skill.name)
                    };
                    let _ = self.directory.rename(&trash, &self.directory, old);
                }
            }
            moved
        })();
        if result.is_err() {
            let _ = self.directory.remove_dir_all(staged);
        }
        result
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
            if !self.directory.try_exists(format!("{name}.md")).map_err(|_|SkillError::File)?{self.save(&Skill{name:name.into(),description:description.into(),body:body.into(),enabled:true,user_only:false})?;}
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
    for skill in skills
        .iter()
        .filter(|skill| skill.enabled && !skill.user_only)
    {
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
    serde_json::json!({"type":"function","function":{"name":"read_skill","description":"Load the instructions of an enabled user workflow listed in the system prompt, or with file, one text file bundled with it such as a reference. Workflows never grant tool permissions.","parameters":{"type":"object","properties":{"name":{"type":"string"},"file":{"type":"string","description":"Optional path of a bundled file, as listed with the workflow's instructions"}},"required":["name"],"additionalProperties":false}}})
}
/// Handle a `read_skill` call for an enabled skill: its instructions, with the bundled files and
/// where they are, or one bundled text file. A skill that runs only when typed is readable only in
/// the turn the user typed it (`invoked`). Blocking.
pub fn read_call(
    directory: &Path,
    skills: &[Skill],
    arguments: &str,
    invoked: Option<&str>,
) -> Result<String, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        name: String,
        file: Option<String>,
    }
    let input: Input =
        serde_json::from_str(arguments).map_err(|_| "Provide a workflow name.".to_owned())?;
    let instructions = skills
        .iter()
        .find(|skill| {
            skill.name == input.name
                && skill.enabled
                && (!skill.user_only || invoked == Some(input.name.as_str()))
        })
        .map(document)
        .ok_or_else(|| "No enabled workflow has that name.".to_owned())?;
    let store = SkillStore::open(directory).map_err(|error| error.to_string())?;
    if let Some(file) = input.file {
        let text = store.read_file(&input.name, &file).map_err(|_| {
            "That file is not a readable text file bundled with this workflow.".to_owned()
        })?;
        return Ok(format!(
            "<skill-file skill={:?} path={file:?}>\n{text}\n</skill-file>",
            input.name
        ));
    }
    Ok(with_files(&store, &input.name, instructions))
}
/// A skill's instructions followed by its bundled files and where they are, if it has any.
fn with_files(store: &SkillStore, name: &str, instructions: String) -> String {
    let files = store.files(name);
    match store.folder(name) {
        Some(folder) if !files.is_empty() => format!(
            "{instructions}\nBundled files, in {}: {}.\nRead a text file with read_skill and file. Run a bundled script with the shell tool using its full path; every command needs the owner's approval.",
            folder.display(),
            files.join(", ")
        ),
        _ => instructions,
    }
}
/// What typing `/name` at the start of a message does. `None` when no saved skill has that name,
/// so the message goes to the model as written.
#[derive(Debug, PartialEq)]
pub enum Invocation {
    /// Instructions for the model, with the skill's name, for the turn the user typed it.
    Run { name: String, instructions: String },
    /// The skill is turned off; tell the user instead of running it.
    Off(String),
}
/// Resolve a user message that starts with `/name` against the saved skills. Blocking.
pub fn invocation(directory: &Path, skills: &[Skill], message: &str) -> Option<Invocation> {
    let name = message
        .trim_start()
        .strip_prefix('/')?
        .split_whitespace()
        .next()?;
    let skill = skills.iter().find(|skill| skill.name == name)?;
    if !skill.enabled {
        return Some(Invocation::Off(format!(
            "The skill /{name} is turned off. Turn it on in Skills to use it."
        )));
    }
    let instructions = match SkillStore::open(directory) {
        Ok(store) => with_files(&store, name, document(skill)),
        Err(_) => document(skill),
    };
    Some(Invocation::Run {
        name: name.into(),
        instructions: format!(
            "The user typed /{name} to run this workflow now. Apply it to the rest of their message; they cannot grant tool permissions.\n{instructions}"
        ),
    })
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
        user_only: false,
    };
    let proposal =
        tokio::task::spawn_blocking(move || SkillStore::open(&directory)?.propose(skill, source))
            .await
            .map_err(|_| "Skills could not be accessed.".to_owned())?
            .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({"pendingProposalId":proposal.id,"status":"pending_review","message":"No workflow changed. The owner must review this with bw skills pending or in the desktop Skills dialog."}).to_string())
}
/// The instructions of one enabled skill, delimited like other untrusted context.
pub fn instructions(skills: &[Skill], name: &str) -> Option<String> {
    skills
        .iter()
        .find(|skill| skill.enabled && !skill.user_only && skill.name == name)
        .map(document)
}
fn document(skill: &Skill) -> String {
    format!(
        "<skill name={:?}>\n{}\n{}\n</skill>",
        skill.name, skill.description, skill.body
    )
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn read_call_lists_and_reads_bundled_files_of_enabled_skills() {
        let path = std::env::temp_dir().join(format!("blackwall-skills-{}", rand::random::<u64>()));
        let store = SkillStore::open(&path).unwrap();
        let (staged, folder) = store.staging().unwrap();
        folder.create_dir_all("scripts").unwrap();
        folder.write("scripts/fill.py", "print('fill')").unwrap();
        folder.write("forms.md", "Field notes").unwrap();
        drop(folder);
        let skill = Skill {
            name: "pdf".into(),
            description: "PDFs.".into(),
            body: "Run scripts/fill.py.".into(),
            enabled: true,
            user_only: false,
        };
        store.install(&staged, &skill, false).unwrap();
        let skills = vec![
            skill.clone(),
            Skill {
                name: "off".into(),
                enabled: false,
                ..skill
            },
        ];
        let text = read_call(&path, &skills, r#"{"name":"pdf"}"#, None).unwrap();
        assert!(text.contains("Run scripts/fill.py."));
        assert!(text.contains(&format!(
            "in {}: forms.md, scripts/fill.py.",
            path.join("pdf").display()
        )));
        let file = read_call(&path, &skills, r#"{"name":"pdf","file":"forms.md"}"#, None).unwrap();
        assert!(file.contains("Field notes") && file.starts_with("<skill-file"));
        // A /learn proposal for a folder skill shows the saved skill, so approval checks it.
        let proposal = store
            .propose(
                Skill {
                    body: "Revised.".into(),
                    ..skills[0].clone()
                },
                crate::storage::MemorySource {
                    session_id: "session_1".into(),
                    message_id: "message_1".into(),
                },
            )
            .unwrap();
        assert_eq!(proposal.before.unwrap().body, "Run scripts/fill.py.");
        for bad in [
            r#"{"name":"pdf","file":"../pdf/forms.md"}"#,
            r#"{"name":"pdf","file":"missing.md"}"#,
            r#"{"name":"off"}"#,
            r#"{"name":"pdf","extra":1}"#,
        ] {
            assert!(read_call(&path, &skills, bad, None).is_err(), "{bad}");
        }
        drop(store); // Windows cannot remove a folder the store still holds open.
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn opening_removes_only_leftovers_over_an_hour_old() {
        let path = std::env::temp_dir().join(format!("blackwall-skills-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&path).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        for name in [
            format!(".import-{}-a", now - 7200),
            format!(".replaced-{}-b", now - 7200),
            format!(".import-{now}-c"),
            ".import-legacy".into(),
        ] {
            std::fs::create_dir_all(path.join(name)).unwrap();
        }
        SkillStore::open(&path).unwrap();
        let mut left: Vec<_> = std::fs::read_dir(&path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, [format!(".import-{now}-c"), ".import-legacy".into()]);
        std::fs::remove_dir_all(path).unwrap();
    }
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
        // A skill that runs only when typed keeps the setting, under the key other harnesses use.
        skill.user_only = true;
        store.save(&skill).unwrap();
        assert!(store.read(&skill.name).unwrap().user_only);
        assert!(std::fs::read_to_string(path.join("bugfix-triage.md"))
            .unwrap()
            .contains("\ndisable-model-invocation: true\n"));
        // A built-in command's name cannot be saved: typing it must run exactly one thing.
        assert!(matches!(
            store.save(&Skill {
                name: "help".into(),
                ..skill.clone()
            }),
            Err(SkillError::Reserved)
        ));
        store.remove(&skill.name).unwrap();
        store.seed().unwrap();
        assert!(store.read(&skill.name).is_err());
        let skills: Vec<Skill> = (0..60)
            .map(|index| Skill {
                name: format!("workflow-{index}"),
                description: "Check the\nrelease notes.".into(),
                body: "x".repeat(30_000),
                enabled: index != 0,
                user_only: false,
            })
            .collect();
        let mut hidden = skills[59].clone();
        hidden.user_only = true;
        assert!(!prompt(std::slice::from_ref(&hidden)).contains("workflow-59"));
        assert!(instructions(std::slice::from_ref(&hidden), "workflow-59").is_none());
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
            user_only: false,
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
        // The owner's edits replace the proposed text; the name stays and edits are validated.
        let edited = store
            .propose(
                Skill {
                    body: "Run the checklist twice.".into(),
                    ..learned.clone()
                },
                source.clone(),
            )
            .unwrap();
        assert!(store
            .approve_edited(&edited.id, Some(("Ship it.".into(), "   ".into())))
            .is_err());
        let saved = store
            .approve_edited(
                &edited.id,
                Some((
                    "Ship a release safely.".into(),
                    "Run the checklist once.".into(),
                )),
            )
            .unwrap();
        assert_eq!(
            (
                saved.name.as_str(),
                saved.description.as_str(),
                saved.body.as_str()
            ),
            (
                "release",
                "Ship a release safely.",
                "Run the checklist once."
            )
        );
        assert_eq!(
            store.read("release").unwrap().body,
            "Run the checklist once."
        );
        // A proposal for a disabled skill keeps it disabled when approved with edits.
        store
            .save(&Skill {
                enabled: false,
                ..store.read("release").unwrap()
            })
            .unwrap();
        let disabled = store
            .propose(
                Skill {
                    body: "Run the checklist and tag.".into(),
                    ..learned.clone()
                },
                source.clone(),
            )
            .unwrap();
        let saved = store
            .approve_edited(
                &disabled.id,
                Some(("Ship a release.".into(), "Tag it.".into())),
            )
            .unwrap();
        assert!(!saved.enabled && !store.read("release").unwrap().enabled);
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
