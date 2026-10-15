//! Import skills written for other agent harnesses. Claude Code, Codex, and most published skills
//! use a folder with a `SKILL.md` file: YAML frontmatter with a name and description, then the
//! instructions. Only the instructions are imported; bundled files and permission settings are not.
use crate::skills::{Skill, SkillError, SkillStore};
use serde::Serialize;
use std::{
    io::Read,
    path::{Path, PathBuf},
};

/// Larger files cannot fit in a skill once converted, so they are not read in full.
const MAX_SOURCE: u64 = 64 * 1024;
const MAX_ENTRIES: usize = 200;
const MAX_SCANNED: usize = 10_000;
const MAX_DESCRIPTION: usize = 500;

/// A skill found in another harness's folder, converted and ready to review before import.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCandidate {
    /// Where it was found, such as `Claude Code` or `Chosen folder`.
    pub origin: String,
    /// The Markdown file to import.
    pub source: String,
    pub skill: Option<Skill>,
    pub error: Option<String>,
    /// What changed or was left out in conversion.
    pub notes: Vec<String>,
    /// A saved skill already has this name.
    pub exists: bool,
}

/// Skill folders other harnesses use under the home folder.
pub fn known_folders(home: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("Claude Code", home.join(".claude").join("skills")),
        ("Codex", home.join(".codex").join("skills")),
        ("Agent Skills", home.join(".agents").join("skills")),
    ]
}

/// Lowercase letters, numbers, and single hyphens, up to 64 characters.
fn skill_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(64);
    out.trim_end_matches('-').to_owned()
}

fn is_skill_file(name: &str) -> bool {
    name.eq_ignore_ascii_case("SKILL.md")
}

/// Other harnesses accept frontmatter that is not strict YAML, such as `hint: [a] [b]`. Read it as
/// `key: value` lines, joining indented lines onto the value above.
fn loose(frontmatter: &str) -> Result<serde_yaml_ng::Mapping, SkillError> {
    let mut pairs: Vec<(String, String)> = vec![];
    for line in frontmatter.lines() {
        if line.starts_with([' ', '\t']) {
            let (_, value) = pairs.last_mut().ok_or(SkillError::Format)?;
            if !value.is_empty() {
                value.push(' ');
            }
            value.push_str(line.trim());
        } else if let Some((key, value)) = line.split_once(':') {
            // A block scalar such as `>-` or `|` starts its text on the next lines.
            let value = value.trim();
            let mut indicator = value.chars();
            let block = matches!(indicator.next(), Some('>' | '|'))
                && indicator.all(|c| matches!(c, '+' | '-') || c.is_ascii_digit());
            pairs.push((key.trim().into(), if block { "" } else { value }.into()));
        } else if !line.trim().is_empty() {
            return Err(SkillError::Format);
        }
    }
    Ok(pairs
        .into_iter()
        .map(|(key, value)| {
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            (key.into(), value.into())
        })
        .collect())
}

/// Convert a `SKILL.md` or Blackwall skill document. `folder` names the skill when its
/// frontmatter does not, and `bundled` lists files beside it that will not be imported.
pub fn convert(
    text: &str,
    folder: &str,
    bundled: &[String],
) -> Result<(Skill, Vec<String>), SkillError> {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let rest = text.strip_prefix("---\n").ok_or(SkillError::Format)?;
    let (frontmatter, body) = match rest.split_once("\n---\n") {
        Some(parts) => parts,
        None => (rest.strip_suffix("\n---").ok_or(SkillError::Format)?, ""),
    };
    let frontmatter = serde_yaml_ng::from_str::<serde_yaml_ng::Mapping>(frontmatter)
        .or_else(|_| loose(frontmatter))?;
    let text_of = |key: &str| {
        frontmatter
            .get(key)
            .and_then(serde_yaml_ng::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let mut notes = vec![];
    let original = text_of("name").unwrap_or(folder);
    let name = skill_name(original);
    if name.is_empty() {
        return Err(SkillError::Name);
    }
    if name != original {
        notes.push(format!("Renamed from {original} to {name}."));
    }
    let mut description = text_of("description")
        .ok_or(SkillError::Format)?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if description.len() > MAX_DESCRIPTION {
        let mut end = MAX_DESCRIPTION - '…'.len_utf8();
        while !description.is_char_boundary(end) {
            end -= 1;
        }
        description.truncate(end);
        description.push('…');
        notes.push("Description shortened to 500 bytes.".into());
    }
    let ignored: Vec<_> = frontmatter
        .keys()
        .filter_map(serde_yaml_ng::Value::as_str)
        .filter(|key| !matches!(*key, "name" | "description" | "enabled"))
        .collect();
    if !ignored.is_empty() {
        notes.push(format!(
            "Ignored settings: {}. Skills never grant tools or permissions.",
            ignored.join(", ")
        ));
    }
    if !bundled.is_empty() {
        let mut shown = bundled
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if bundled.len() > 5 {
            shown.push_str(&format!(", and {} more", bundled.len() - 5));
        }
        notes.push(format!(
            "Bundled files are not imported: {shown}. Instructions that use them may not work."
        ));
    }
    let skill = Skill {
        name,
        description,
        body: body.trim().into(),
        enabled: frontmatter
            .get("enabled")
            .and_then(serde_yaml_ng::Value::as_bool)
            .unwrap_or(true),
    };
    crate::skills::validate(&skill)?;
    Ok((skill, notes))
}

/// Names in a folder, skipping hidden ones, sorted. Very large folders are cut off at a bound, so
/// callers look for `SKILL.md` before limiting what they list.
fn entries(folder: &Path) -> Vec<(String, PathBuf)> {
    let mut found: Vec<_> = std::fs::read_dir(folder)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .take(MAX_SCANNED)
                .map(|entry| {
                    (
                        entry.file_name().to_string_lossy().into_owned(),
                        entry.path(),
                    )
                })
                .filter(|(name, _)| !name.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

/// Read a regular file without blocking on a pipe or device that has the same name.
fn read(path: &Path) -> Result<String, SkillError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| SkillError::File)?;
    if !file.metadata().map_err(|_| SkillError::File)?.is_file() {
        return Err(SkillError::File);
    }
    let mut bytes = vec![];
    file.take(MAX_SOURCE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SkillError::File)?;
    if bytes.len() as u64 > MAX_SOURCE {
        return Err(SkillError::Limit);
    }
    String::from_utf8(bytes).map_err(|_| SkillError::Limit)
}

/// Convert one Markdown file. A `SKILL.md` is named after its folder and reports the files
/// beside it; any other `.md` file is named after itself.
fn convert_file(path: &Path) -> Result<(Skill, Vec<String>), SkillError> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (folder, bundled) = if is_skill_file(&file_name) {
        let parent = path.parent().unwrap_or(Path::new(""));
        let bundled = entries(parent)
            .into_iter()
            .filter(|(name, _)| !is_skill_file(name))
            .map(|(name, path)| {
                if path.is_dir() {
                    format!("{name}/")
                } else {
                    name
                }
            })
            .collect();
        let folder = parent
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        (folder, bundled)
    } else {
        let stem = file_name
            .strip_suffix(".md")
            .unwrap_or(&file_name)
            .to_owned();
        (stem, vec![])
    };
    convert(&read(path)?, &folder, &bundled)
}

fn candidate(origin: &str, path: &Path) -> ImportCandidate {
    // The source is imported later by its path, so it must survive a round trip as text.
    let (skill, notes, error) = match path.to_str().map(|_| convert_file(path)) {
        Some(Ok((skill, notes))) => (Some(skill), notes, None),
        Some(Err(error)) => (None, vec![], Some(error.to_string())),
        None => (
            None,
            vec![],
            Some("Rename this file or its folder: the name is not valid text.".into()),
        ),
    };
    ImportCandidate {
        origin: origin.into(),
        source: path.to_string_lossy().into_owned(),
        skill,
        error,
        notes,
        exists: false,
    }
}

/// Find skills at `path`: a `SKILL.md` or Markdown file, a skill folder, or a folder of skill
/// folders and Markdown skills one level down. Nothing is saved.
pub fn scan(origin: &str, path: &Path) -> Vec<ImportCandidate> {
    if path.is_file() {
        return vec![candidate(origin, path)];
    }
    let here = entries(path);
    if let Some((_, skill)) = here.iter().find(|(name, _)| is_skill_file(name)) {
        return vec![candidate(origin, skill)];
    }
    here.into_iter()
        .take(MAX_ENTRIES)
        .filter_map(|(name, child)| {
            if child.is_dir() {
                entries(&child)
                    .into_iter()
                    .find(|(name, _)| is_skill_file(name))
                    .map(|(_, skill)| candidate(origin, &skill))
            } else {
                name.ends_with(".md").then(|| candidate(origin, &child))
            }
        })
        .collect()
}

/// Mark candidates whose names are already saved, or repeated earlier in the list.
pub fn mark(store: &SkillStore, candidates: &mut [ImportCandidate]) {
    let mut seen = std::collections::HashSet::new();
    for candidate in candidates {
        if let Some(skill) = &candidate.skill {
            candidate.exists = store.contains(&skill.name);
            if !seen.insert(skill.name.clone()) {
                candidate
                    .notes
                    .push("Another skill in this list has the same name.".into());
            }
        }
    }
}

/// Import one Markdown file found by `scan`. A saved skill with the same name is kept unless
/// `replace` is set.
pub fn import(store: &SkillStore, source: &Path, replace: bool) -> Result<Skill, SkillError> {
    if !source
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
    {
        return Err(SkillError::Format);
    }
    let (skill, _) = convert_file(source)?;
    if replace {
        store.save(&skill)?;
    } else {
        store.create(&skill)?;
    }
    Ok(skill)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Folder(PathBuf);
    impl Folder {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("blackwall-import-{}", rand::random::<u64>()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn write(&self, relative: &str, text: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        }
    }
    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn converts_agent_skills_and_reports_what_was_left_out() {
        let (skill, notes) = convert(
            "\u{feff}---\r\nname: PDF_Tools\r\ndescription: >\r\n  Fill and\r\n  merge PDFs.\r\nallowed-tools: Bash\r\nlicense: MIT\r\n---\r\n\r\n# Steps\r\nUse scripts/fill.py.\r\n",
            "ignored",
            &["scripts/".into(), "forms.md".into()],
        )
        .unwrap();
        assert_eq!(skill.name, "pdf-tools");
        assert_eq!(skill.description, "Fill and merge PDFs.");
        assert_eq!(skill.body, "# Steps\nUse scripts/fill.py.");
        assert!(skill.enabled);
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes[0].contains("PDF_Tools"));
        assert!(notes[1].contains("allowed-tools, license"));
        assert!(notes[2].contains("scripts/, forms.md"));
        let long = format!("---\ndescription: {}\n---\nBody", "é".repeat(400));
        let (skill, notes) = convert(&long, "Folder Name", &[]).unwrap();
        assert_eq!(skill.name, "folder-name");
        assert!(skill.description.len() <= MAX_DESCRIPTION && skill.description.ends_with('…'));
        assert!(notes.iter().any(|note| note.contains("shortened")));
        let (skill, notes) = convert(
            "---\nname: stack\ndescription: 'Manage the stack:\n  start or stop'\nargument-hint: [command] [args...]\n---\nRun it.",
            "",
            &[],
        )
        .unwrap();
        assert_eq!(skill.description, "Manage the stack: start or stop");
        assert!(notes[0].contains("argument-hint"));
        for indicator in [">-", "|", ">+", "|2-"] {
            let (skill, _) = convert(
                &format!("---\nname: pdf\ndescription: {indicator}\n  Fill and\n  merge PDFs.\nhint: [a] [b]\n---\nSteps"),
                "",
                &[],
            )
            .unwrap();
            assert_eq!(skill.description, "Fill and merge PDFs.", "{indicator}");
        }
    }

    #[test]
    fn rejects_documents_that_cannot_become_skills() {
        for text in [
            "No frontmatter",
            "---\nname: a\n---\nNo description",
            "---\ndescription: d\n---\n   ",
            "---\nname: '!!!'\ndescription: d\n---\nBody",
            "---\n  indented first\n---\nBody",
            "---\njust words\n---\nBody",
        ] {
            assert!(convert(text, "", &[]).is_err(), "{text}");
        }
        let big = format!("---\ndescription: d\n---\n{}", "x".repeat(40 * 1024));
        assert!(matches!(convert(&big, "big", &[]), Err(SkillError::Limit)));
    }

    #[test]
    fn scans_harness_folders_and_imports_without_overwriting() {
        let source = Folder::new();
        source.write(
            "pdf/SKILL.md",
            "---\nname: pdf\ndescription: PDFs.\n---\nSteps",
        );
        source.write("pdf/scripts/fill.py", "print()");
        source.write("broken/SKILL.md", "no frontmatter");
        source.write("notes/readme.txt", "not a skill");
        source.write(
            "release.md",
            "---\nname: release\ndescription: Ship.\nenabled: false\n---\nCheck",
        );
        source.write(
            ".hidden/SKILL.md",
            "---\nname: hidden\ndescription: h\n---\nh",
        );
        let found = scan("Chosen folder", &source.0);
        let names: Vec<_> = found
            .iter()
            .map(|c| c.skill.as_ref().map_or("error", |s| s.name.as_str()))
            .collect();
        assert_eq!(names, ["error", "pdf", "release"]);
        assert!(found[1].notes.iter().any(|n| n.contains("scripts/")));
        assert!(found[2].skill.as_ref().is_some_and(|s| !s.enabled));
        assert_eq!(scan("x", &source.0.join("pdf")).len(), 1);

        let data = Folder::new();
        let store = SkillStore::open(&data.0).unwrap();
        store
            .save(&Skill {
                name: "pdf".into(),
                description: "Mine.".into(),
                body: "Keep".into(),
                enabled: true,
            })
            .unwrap();
        let mut found = scan("Chosen folder", &source.0);
        mark(&store, &mut found);
        assert!(found[1].exists && !found[2].exists);
        let pdf = Path::new(&found[1].source);
        assert!(matches!(
            import(&store, pdf, false),
            Err(SkillError::Exists)
        ));
        assert_eq!(store.read("pdf").unwrap().body, "Keep");
        assert_eq!(import(&store, pdf, true).unwrap().body, "Steps");
        assert_eq!(store.read("pdf").unwrap().description, "PDFs.");
        assert!(import(&store, &source.0.join("notes/readme.txt"), false).is_err());
        // A saved file that no longer parses is still kept unless replacing.
        std::fs::write(data.0.join("release.md"), "broken").unwrap();
        let release = source.0.join("release.md");
        assert!(matches!(
            import(&store, &release, false),
            Err(SkillError::Exists)
        ));
        assert_eq!(
            std::fs::read_to_string(data.0.join("release.md")).unwrap(),
            "broken"
        );
        assert!(import(&store, &release, true).is_ok());
        assert!(matches!(
            import(&store, Path::new(&found[0].source), false),
            Err(SkillError::Format)
        ));
    }

    #[test]
    fn finds_skill_md_in_large_folders_and_skips_special_files() {
        let source = Folder::new();
        for index in 0..MAX_ENTRIES + 50 {
            source.write(&format!("big/a{index:04}.txt"), "");
        }
        source.write("big/SKILL.md", "---\nname: big\ndescription: d\n---\nBody");
        let found = scan("x", &source.0.join("big"));
        assert_eq!(found.len(), 1);
        assert!(found[0]
            .notes
            .iter()
            .any(|note| note.contains("and 245 more")));
        #[cfg(unix)]
        {
            let fifo = source.0.join("pipe/SKILL.md");
            std::fs::create_dir_all(fifo.parent().unwrap()).unwrap();
            assert!(std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success());
            let found = scan("x", &source.0.join("pipe"));
            assert!(found[0].skill.is_none() && found[0].error.is_some());
        }
    }
}
