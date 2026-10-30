//! Selection and validation of the owner's Blackwall data directory.
use std::{
    ffi::OsStr,
    path::{Component, Path, PathBuf},
};

const HOME_ERROR: &str = "Your home folder could not be found.";
const OVERRIDE_ERROR: &str = "BLACKWALL_DATA_DIR must be an absolute, non-root path without `..`.";

/// Select the owner's data directory from an optional environment override and home folder.
///
/// An empty override is treated as unset. Overrides must identify a specific absolute folder;
/// relative paths, the filesystem root, and parent traversal are refused.
pub fn resolve(
    override_path: Option<&OsStr>,
    home: Option<&Path>,
) -> Result<PathBuf, &'static str> {
    if let Some(value) = override_path.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        if !path.is_absolute()
            || path.parent().is_none()
            || path
                .components()
                .any(|component| component == Component::ParentDir)
        {
            return Err(OVERRIDE_ERROR);
        }
        return Ok(path);
    }
    home.map(|home| home.join(".blackwall")).ok_or(HOME_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_wins_and_unset_uses_the_established_default() {
        let home = Path::new("/Users/owner");
        let owner = std::env::temp_dir().join("blackwall-owner");
        assert_eq!(resolve(Some(owner.as_os_str()), Some(home)).unwrap(), owner);
        assert_eq!(resolve(None, Some(home)).unwrap(), home.join(".blackwall"));
        assert_eq!(
            resolve(Some(OsStr::new("")), Some(home)).unwrap(),
            home.join(".blackwall")
        );
    }

    #[test]
    fn unsafe_overrides_have_one_stable_error() {
        for path in ["relative", "/", "/var/../private/blackwall"] {
            assert_eq!(resolve(Some(OsStr::new(path)), None), Err(OVERRIDE_ERROR));
        }
    }
}
