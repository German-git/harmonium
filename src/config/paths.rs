//! XDG path resolution and filesystem helpers.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{HarmoniumError, Result as DomainResult};

const PROJECT_NAME: &str = "harmonium";

/// Resolved application directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// User configuration such as `config.toml` and future theme files.
    pub config_dir: PathBuf,
    /// User data such as saved playlists.
    pub data_dir: PathBuf,
    /// Regenerable cache content such as log files.
    pub cache_dir: PathBuf,
}

impl Paths {
    /// Resolve the system locations for the current user.
    pub fn system() -> DomainResult<Self> {
        if let Some(dirs) = directories::ProjectDirs::from("", "", PROJECT_NAME) {
            return Ok(Self::new(
                dirs.config_dir(),
                dirs.data_dir(),
                dirs.cache_dir(),
            ));
        }

        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| !home.as_os_str().is_empty())
            .ok_or(HarmoniumError::NoHomeDir)?;

        Ok(Self::for_home(&home))
    }

    /// Locations derived from an explicit home directory.
    pub fn for_home(home: &Path) -> Self {
        Self::new(
            &home.join(".config").join(PROJECT_NAME),
            &home.join(".local").join("share").join(PROJECT_NAME),
            &home.join(".cache").join(PROJECT_NAME),
        )
    }

    fn new(config_dir: &Path, data_dir: &Path, cache_dir: &Path) -> Self {
        Self {
            config_dir: config_dir.to_path_buf(),
            data_dir: data_dir.to_path_buf(),
            cache_dir: cache_dir.to_path_buf(),
        }
    }

    /// Directory holding saved playlists below the data root.
    pub fn playlists_dir(&self) -> PathBuf {
        self.data_dir.join("playlists")
    }
}

/// Create `path` including missing parents, tolerating concurrent creation.
pub fn ensure_dir(path: &Path) -> DomainResult<()> {
    fs::create_dir_all(path).map_err(|source| HarmoniumError::io(path, source))?;
    Ok(())
}
