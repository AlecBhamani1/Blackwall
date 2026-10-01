//! Structured, bounded file browsing through the selected workspace capability.
use super::{ToolError, Workspace};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_directory: bool,
}

#[derive(Debug, Serialize)]
pub struct DirectoryListing {
    pub entries: Vec<FileEntry>,
    pub truncated: bool,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileSearchMode {
    Names,
    Contents,
}

#[derive(Debug, Serialize)]
pub struct FileSearchMatch {
    #[serde(flatten)]
    pub entry: FileEntry,
    pub line: Option<usize>,
    pub preview: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FileSearch {
    pub matches: Vec<FileSearchMatch>,
    pub truncated: bool,
}

impl Workspace {
    pub fn browse(&self, path: &str) -> Result<DirectoryListing, ToolError> {
        self.directory_entries(path, 500)
            .map(|(listing, _)| listing)
    }

    fn directory_entries(
        &self,
        path: &str,
        limit: usize,
    ) -> Result<(DirectoryListing, usize), ToolError> {
        let root = Self::checked(path)?;
        let mut entries = Vec::new();
        let mut truncated = false;
        let mut scanned = 0;
        for (index, entry) in self
            .directory
            .read_dir(root)
            .map_err(|_| ToolError::File)?
            .enumerate()
        {
            if index >= limit {
                truncated = true;
                break;
            }
            scanned += 1;
            let entry = entry.map_err(|_| ToolError::File)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name == ".git" {
                continue;
            }
            let kind = entry.file_type().map_err(|_| ToolError::File)?;
            // Do not expose symlinks or special files as browseable entries.
            if !kind.is_file() && !kind.is_dir() {
                continue;
            }
            let relative = root.join(name);
            let Some(label) = super::label(&relative) else {
                continue;
            };
            if Self::checked(&label).is_err() {
                continue;
            }
            entries.push(FileEntry {
                name: name.into(),
                path: label,
                is_directory: kind.is_dir(),
            });
        }
        entries.sort_by(|a, b| {
            b.is_directory
                .cmp(&a.is_directory)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok((DirectoryListing { entries, truncated }, scanned))
    }

    pub fn find_files(&self, query: &str, mode: FileSearchMode) -> Result<FileSearch, ToolError> {
        if query.trim().is_empty() || query.len() > 256 {
            return Err(ToolError::Arguments);
        }
        let needle = query.to_lowercase();
        let mut directories = vec![(".".to_owned(), 0)];
        let mut visited = 0;
        let mut bytes = 0;
        let mut result = FileSearch {
            matches: Vec::new(),
            truncated: false,
        };
        while let Some((directory, depth)) = directories.pop() {
            if visited >= 2000 {
                result.truncated = true;
                return Ok(result);
            }
            let (listing, scanned) = match self.directory_entries(&directory, 2000 - visited) {
                Ok(listing) => listing,
                Err(error) if depth == 0 => return Err(error),
                Err(_) => continue,
            };
            visited += scanned;
            result.truncated |= listing.truncated;
            for entry in listing.entries {
                if bytes >= 8 * 1024 * 1024 || result.matches.len() >= 100 {
                    result.truncated = true;
                    return Ok(result);
                }
                if entry.is_directory {
                    if ["node_modules", "target", "dist"].contains(&entry.name.as_str()) {
                        continue;
                    }
                    if depth < 20 {
                        directories.push((entry.path.clone(), depth + 1));
                    } else {
                        result.truncated = true;
                    }
                }
                match mode {
                    FileSearchMode::Names if entry.path.to_lowercase().contains(&needle) => {
                        result.matches.push(FileSearchMatch {
                            entry,
                            line: None,
                            preview: None,
                        });
                    }
                    FileSearchMode::Contents if !entry.is_directory => {
                        let Ok(text) = self.read(&entry.path) else {
                            continue;
                        };
                        bytes += text.len();
                        if let Some((index, line)) = text
                            .lines()
                            .enumerate()
                            .find(|(_, line)| line.to_lowercase().contains(&needle))
                        {
                            result.matches.push(FileSearchMatch {
                                entry,
                                line: Some(index + 1),
                                preview: Some(line.chars().take(240).collect()),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        result
            .matches
            .sort_by(|a, b| a.entry.path.cmp(&b.entry.path));
        Ok(result)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rand::{rand_core::UnwrapErr, rngs::SysRng, Rng};

    #[test]
    fn browsing_preserves_names_and_enforces_the_workspace_boundary() {
        let root = std::env::temp_dir().join(format!(
            "blackwall-browser-{:016x}",
            UnwrapErr(SysRng).next_u64()
        ));
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        // Windows forbids control characters in names; check an unusual Unicode name there.
        let odd_name = if cfg!(windows) {
            "two lines — ünïcode.txt"
        } else {
            "two\nlines.txt"
        };
        std::fs::write(root.join(odd_name), "Needle in text").unwrap();
        std::fs::write(root.join("docs/guide.md"), "another needle").unwrap();
        let workspace = Workspace::open(&root).unwrap();
        let listing = workspace.browse(".").unwrap();
        assert_eq!(listing.entries.len(), 2);
        assert_eq!(listing.entries[0].name, "docs");
        assert_eq!(listing.entries[1].name, odd_name);
        assert!(!listing.truncated);
        assert!(workspace.browse("../").is_err());
        assert!(workspace.browse("/etc").is_err());
        assert!(workspace.browse(".git").is_err());
        let matches = workspace
            .find_files("GUIDE", FileSearchMode::Names)
            .unwrap();
        assert_eq!(matches.matches[0].entry.path, "./docs/guide.md");
        let matches = workspace
            .find_files("NEEDLE", FileSearchMode::Contents)
            .unwrap();
        assert_eq!(matches.matches.len(), 2);
        assert_eq!(matches.matches[0].line, Some(1));
        assert!(workspace.find_files("", FileSearchMode::Names).is_err());
        assert!(workspace
            .find_files(&"x".repeat(257), FileSearchMode::Contents)
            .is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", root.join("escape")).unwrap();
            assert_eq!(workspace.browse(".").unwrap().entries.len(), 2);
            assert!(workspace.browse("escape").is_err());
            assert!(workspace.read("escape/passwd").is_err());
        }
        std::fs::write(root.join("large.txt"), vec![b'x'; 64 * 1024 + 1]).unwrap();
        std::fs::write(root.join("binary.dat"), [0xff, 0xfe]).unwrap();
        assert!(workspace.read("large.txt").is_err());
        assert!(workspace.read("binary.dat").is_err());
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::write(root.join("node_modules/dependency.txt"), "needle").unwrap();
        assert_eq!(
            workspace
                .find_files("needle", FileSearchMode::Contents)
                .unwrap()
                .matches
                .len(),
            2
        );
        for index in 0..501 {
            std::fs::write(root.join(format!("file-{index}")), "text").unwrap();
        }
        assert!(workspace.browse(".").unwrap().truncated);
        let matches = workspace
            .find_files("file-", FileSearchMode::Names)
            .unwrap();
        assert!(matches.truncated);
        assert_eq!(matches.matches.len(), 100);
        assert_eq!(
            workspace
                .find_files("file-500", FileSearchMode::Names)
                .unwrap()
                .matches
                .len(),
            1
        );
        // Close the directory handle first; Windows cannot delete an open directory.
        drop(workspace);
        std::fs::remove_dir_all(root).unwrap();
    }
}
