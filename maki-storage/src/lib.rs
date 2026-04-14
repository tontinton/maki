//! Persistent storage under `~/.maki`. `atomic_write` writes to `.tmp` then renames for crash
//! safety. `atomic_write_permissions` sets file mode before rename (for auth keys at 0600).

pub mod auth;
pub mod input_history;
pub mod log;
pub mod model;
pub mod plans;
pub mod sessions;
pub mod theme;
pub mod version;

use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DATA_DIR_NAME: &str = ".maki";

#[derive(Debug, Clone)]
pub struct DataDir(PathBuf);

impl DataDir {
    pub fn resolve() -> Result<Self, StorageError> {
        let dir = dirs::home_dir()
            .ok_or(StorageError::HomeNotSet)?
            .join(DATA_DIR_NAME);
        fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }

    pub fn from_path(path: PathBuf) -> Self {
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn ensure_subdir(&self, name: &str) -> Result<PathBuf, StorageError> {
        let dir = self.0.join(name);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("home directory not found")]
    HomeNotSet,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("slug collision after max attempts")]
    SlugCollision,
}

pub fn atomic_write(path: &Path, data: &[u8]) -> Result<(), StorageError> {
    let tmp = path.with_extension("tmp");
    let mut f = fs::File::create(&tmp)?;
    f.write_all(data)?;
    f.sync_data()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

pub(crate) fn atomic_write_permissions(
    path: &Path,
    data: &[u8],
    mode: u32,
) -> Result<(), StorageError> {
    let tmp = path.with_extension("tmp");
    let mut f = fs::File::create(&tmp)?;
    f.write_all(data)?;
    #[cfg(unix)]
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    #[cfg(not(unix))]
    let _ = mode;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
