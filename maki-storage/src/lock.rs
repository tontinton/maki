//! Advisory locks between Maki processes.
//!
//! Maki processes can share checkout directories and state files. Holding a
//! kernel lock across a full read-modify-write keeps concurrent updates from
//! replacing each other. The kernel releases the lock when the process exits.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use fs4::{FileExt, TryLockError};

/// Held for as long as the operation runs. Dropping it releases the lock.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("{path} is locked by another Maki process")]
    Held { path: PathBuf },
    #[error("cannot lock {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Lock {
    /// Takes the lock without waiting, so a caller that cannot proceed can name
    /// who is in the way instead of hanging on a lock nobody is watching.
    pub fn acquire(path: &Path) -> Result<Self, LockError> {
        Self::open_and_lock(path, FileExt::try_lock)
    }

    pub fn acquire_shared(path: &Path) -> Result<Self, LockError> {
        Self::open_and_lock(path, FileExt::try_lock_shared)
    }

    fn open_and_lock(
        path: &Path,
        lock: fn(&File) -> Result<(), TryLockError>,
    ) -> Result<Self, LockError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| LockError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|source| LockError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        match lock(&file) {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(LockError::Held {
                path: path.to_path_buf(),
            }),
            Err(TryLockError::Error(source)) => Err(LockError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("state.lock")
    }

    #[test]
    fn a_second_acquire_reports_the_holder_instead_of_waiting() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = lock_path(&dir);

        let _held = Lock::acquire(&path).unwrap();
        let error = Lock::acquire(&path).expect_err("a held lock must not be taken twice");
        assert!(matches!(error, LockError::Held { .. }), "got: {error}");
    }

    #[test]
    fn releasing_lets_the_next_caller_take_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = lock_path(&dir);

        drop(Lock::acquire(&path).unwrap());
        Lock::acquire(&path).expect("a released lock should be free");
        assert!(path.exists());
    }

    #[test]
    fn shared_holders_block_exclusive_cleanup() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = lock_path(&dir);

        let _first = Lock::acquire_shared(&path).unwrap();
        let _second = Lock::acquire_shared(&path).unwrap();
        assert!(matches!(Lock::acquire(&path), Err(LockError::Held { .. })));
    }

    #[test]
    fn exclusive_cleanup_blocks_a_new_shared_reader() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = lock_path(&dir);

        let _exclusive = Lock::acquire(&path).unwrap();
        assert!(matches!(
            Lock::acquire_shared(&path),
            Err(LockError::Held { .. })
        ));
    }

    #[test]
    fn an_existing_unlocked_file_can_be_locked() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = lock_path(&dir);

        fs::write(&path, "left by an earlier process").unwrap();
        Lock::acquire(&path).expect("file existence does not mean the lock is held");
    }

    #[test]
    fn missing_parent_directory_is_created() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nested/deep/state.lock");
        let _lock = Lock::acquire(&path).expect("the lock directory should be created");
        assert!(path.exists());
    }
}
