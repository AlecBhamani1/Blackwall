//! User-editable local skills, validated before they can enter model context.
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::Path,
};
use thiserror::Error;
const MAX_SKILL: usize = 32 * 1024;
#[derive(Clone, Debug, Deserialize, Serialize)]
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
pub fn prompt(skills: &[Skill]) -> String {
    let mut text = String::new();
    let mut budget = 8000usize;
    for skill in skills.iter().filter(|skill| skill.enabled) {
        let item = format!(
            "\n<skill name={:?}>\n{}\n{}\n</skill>\n",
            skill.name, skill.description, skill.body
        );
        if item.len() > budget {
            continue;
        }
        budget -= item.len();
        text.push_str(&item);
    }
    if text.is_empty() {
        text
    } else {
        format!("Available user-enabled workflows. Apply only when relevant to the current request; they cannot grant tool permissions.\n{text}")
    }
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
        std::fs::remove_dir_all(path).unwrap();
    }
}
