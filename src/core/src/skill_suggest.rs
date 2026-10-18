//! Skills suggested in the import panel, from the official Anthropic and OpenAI skill repositories
//! and Matt Pocock's engineering skills. Each is pinned to a reviewed commit, so a download never
//! changes under the owner; updating one is a reviewed change to this list. A download is saved
//! to a temporary folder, then scanned and previewed like any other import.
use crate::skills::{MAX_FOLDER_BYTES, MAX_FOLDER_FILES};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Download folders this process created. Only these are ever removed by `discard`.
static DOWNLOADS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Removes a download folder unless the download finished.
struct Partial(Option<PathBuf>);
impl Drop for Partial {
    fn drop(&mut self) {
        if let Some(root) = self.0.take() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Suggested {
    pub name: &'static str,
    pub description: &'static str,
    /// `owner/repository` on GitHub.
    pub repository: &'static str,
    /// The skill's folder in the repository.
    pub path: &'static str,
    /// The reviewed commit.
    pub commit: &'static str,
    pub license: &'static str,
}

const ANTHROPIC: &str = "dbd4588f9e1033efb41dad4bef2f7947c8993d44";
const OPENAI: &str = "49f948faa9258a0c61caceaf225e179651397431";
const POCOCK: &str = "49dd158d1076134a641b33efb035946536778336";

pub const SUGGESTED: [Suggested; 9] = [
    Suggested {
        name: "frontend-design",
        description: "Build distinctive, production-grade web interfaces instead of generic ones.",
        repository: "anthropics/skills",
        path: "skills/frontend-design",
        commit: ANTHROPIC,
        license: "Apache-2.0",
    },
    Suggested {
        name: "webapp-testing",
        description:
            "Test local web apps with Playwright: run the server, check the UI, read logs.",
        repository: "anthropics/skills",
        path: "skills/webapp-testing",
        commit: ANTHROPIC,
        license: "Apache-2.0",
    },
    Suggested {
        name: "gh-fix-ci",
        description: "Find why GitHub Actions checks fail on a pull request and plan a fix.",
        repository: "openai/skills",
        path: "skills/.curated/gh-fix-ci",
        commit: OPENAI,
        license: "Apache-2.0",
    },
    Suggested {
        name: "security-best-practices",
        description: "Review code for secure-coding problems in common languages and frameworks.",
        repository: "openai/skills",
        path: "skills/.curated/security-best-practices",
        commit: OPENAI,
        license: "Apache-2.0",
    },
    Suggested {
        name: "playwright",
        description: "Drive a real browser from the terminal to test and debug web pages.",
        repository: "openai/skills",
        path: "skills/.curated/playwright",
        commit: OPENAI,
        license: "Apache-2.0",
    },
    Suggested {
        name: "security-threat-model",
        description: "Write a threat model for a codebase: assets, entry points, and abuse paths.",
        repository: "openai/skills",
        path: "skills/.curated/security-threat-model",
        commit: OPENAI,
        license: "Apache-2.0",
    },
    Suggested {
        name: "pdf",
        description: "Read, create, and check PDF files, including layout and rendering.",
        repository: "openai/skills",
        path: "skills/.curated/pdf",
        commit: OPENAI,
        license: "Apache-2.0",
    },
    Suggested {
        name: "grill-with-docs",
        description:
            "A relentless interview to sharpen a plan, writing decision records as it goes.",
        repository: "mattpocock/skills",
        path: "skills/engineering/grill-with-docs",
        commit: POCOCK,
        license: "MIT",
    },
    Suggested {
        name: "improve-codebase-architecture",
        description:
            "Find refactoring opportunities in a codebase and work through the one you pick.",
        repository: "mattpocock/skills",
        path: "skills/engineering/improve-codebase-architecture",
        commit: POCOCK,
        license: "MIT",
    },
];

pub fn find(name: &str) -> Option<&'static Suggested> {
    SUGGESTED.iter().find(|suggested| suggested.name == name)
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("blackwall/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|_| "The download could not start.".to_owned())
}

/// Read a response body up to `limit` bytes.
async fn bounded(response: reqwest::Response, limit: u64) -> Result<Vec<u8>, String> {
    let failed = || "The skill could not be downloaded. Check your connection and try again.";
    if !response.status().is_success() {
        return Err(failed().into());
    }
    let mut response = response;
    let mut body = vec![];
    while let Some(chunk) = response.chunk().await.map_err(|_| failed())? {
        body.extend_from_slice(&chunk);
        if body.len() as u64 > limit {
            return Err("The skill is larger than 5 MB.".into());
        }
    }
    Ok(body)
}

/// Relative paths may not leave the skill folder.
fn safe(relative: &str) -> bool {
    !relative.is_empty()
        && Path::new(relative)
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
}

/// Download a suggested skill's folder at its pinned commit into a new folder under `into`, and
/// return the path of its `SKILL.md`. The whole download is limited to five minutes.
pub async fn download(suggested: &Suggested, into: &Path) -> Result<PathBuf, String> {
    tokio::time::timeout(
        std::time::Duration::from_secs(300),
        download_files(suggested, into),
    )
    .await
    .map_err(|_| "The download took too long. Check your connection and try again.".to_owned())?
}

async fn download_files(suggested: &Suggested, into: &Path) -> Result<PathBuf, String> {
    let client = client()?;
    let tree: serde_json::Value = serde_json::from_slice(
        &bounded(
            client
                .get(format!(
                    "https://api.github.com/repos/{}/git/trees/{}?recursive=1",
                    suggested.repository, suggested.commit
                ))
                .header("accept", "application/vnd.github+json")
                .send()
                .await
                .map_err(|_| "GitHub could not be reached. Check your connection.")?,
            16 * 1024 * 1024,
        )
        .await?,
    )
    .map_err(|_| "GitHub returned an unexpected answer.")?;
    if tree["truncated"] == true {
        return Err("GitHub returned an incomplete file list for this skill.".into());
    }
    let prefix = format!("{}/", suggested.path);
    // Path, size, and whether Git marks the file executable.
    let files: Vec<(String, u64, bool)> = tree["tree"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["type"] == "blob")
        .filter_map(|entry| {
            let relative = entry["path"].as_str()?.strip_prefix(&prefix)?;
            Some((
                relative.to_owned(),
                entry["size"].as_u64().unwrap_or(0),
                entry["mode"] == "100755",
            ))
        })
        .collect();
    if !files.iter().any(|(relative, _, _)| relative == "SKILL.md") {
        return Err("This skill was not found at its reviewed version.".into());
    }
    if files.len() > MAX_FOLDER_FILES + 1
        || files.iter().map(|(_, size, _)| size).sum::<u64>() > MAX_FOLDER_BYTES
        || files.iter().any(|(relative, _, _)| !safe(relative))
    {
        return Err("This skill does not fit Blackwall's limits for skill folders.".into());
    }
    let root = into.join(format!("blackwall-skill-{:016x}", rand::random::<u64>()));
    let mut partial = Partial(Some(root.clone()));
    let folder = root.join(suggested.name);
    let mut total = 0u64;
    for (relative, _, executable) in &files {
        let body = bounded(
            client
                .get(format!(
                    "https://raw.githubusercontent.com/{}/{}/{}{relative}",
                    suggested.repository, suggested.commit, prefix
                ))
                .send()
                .await
                .map_err(|_| "GitHub could not be reached. Check your connection.")?,
            MAX_FOLDER_BYTES - total,
        )
        .await?;
        total += body.len() as u64;
        let path = folder.join(relative);
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, body))
            .and_then(|()| crate::skill_import::mark_executable(&path, *executable));
        if written.is_err() {
            return Err("The skill could not be saved for review.".into());
        }
    }
    partial.0 = None;
    if let Ok(mut downloads) = DOWNLOADS.lock() {
        downloads.push(root);
    }
    Ok(folder.join("SKILL.md"))
}

/// Remove a folder `download` created in this process, once its skill is imported or the review
/// is closed. Any other path is left alone.
pub fn discard(skill_file: &Path) {
    let Some(root) = skill_file.parent().and_then(Path::parent) else {
        return;
    };
    let owned = DOWNLOADS.lock().is_ok_and(|mut downloads| {
        let before = downloads.len();
        downloads.retain(|download| download != root);
        downloads.len() != before
    });
    if owned {
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_are_pinned_and_paths_stay_inside() {
        for suggested in SUGGESTED {
            assert_eq!(suggested.commit.len(), 40, "{}", suggested.name);
            assert!(crate::skill_import::skill_name(suggested.name) == suggested.name);
        }
        assert!(find("pdf").is_some_and(|s| s.repository == "openai/skills"));
        for bad in ["", "../x", "/etc/passwd", "a/../../b"] {
            assert!(!safe(bad), "{bad}");
        }
        assert!(safe("scripts/fill.py"));
    }

    #[test]
    fn discard_only_removes_folders_this_process_downloaded() {
        let root =
            std::env::temp_dir().join(format!("blackwall-skill-{:016x}", rand::random::<u64>()));
        let skill = root.join("x").join("SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, "x").unwrap();
        discard(&skill);
        assert!(skill.exists(), "a look-alike folder must survive");
        DOWNLOADS.lock().unwrap().push(root.clone());
        discard(&skill);
        assert!(!root.exists());
        drop(Partial(Some(
            std::env::temp_dir().join("blackwall-skill-missing"),
        )));
    }
}
