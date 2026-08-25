//! Advisory locks between maki processes.
//!
//! Several maki processes share one checkout directory, one lockfile, and one
//! approval store. Without a lock, two starting at the same moment would race
//! the same clone, and two writing the lockfile would lose one another's
//! entries.
//!
//! The kernel owns the lock and releases it when the process dies. Holding it
//! across a whole read-modify-write is what makes concurrent updates to
//! different packages compose: atomic rename alone gives durability, not
//! isolation.

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
    /// Takes the lock without waiting.
    ///
    /// This never waits. A caller that cannot proceed tells the user which
    /// operation is in the way, which is more useful than a startup that hangs
    /// on a lock nobody is watching.
    pub fn acquire(path: &Path) -> Result<Self, LockError> {
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
        match FileExt::try_lock(&file) {
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
        dir.path().join("pack.lock")
    }

    #[test]
    fn a_second_acquire_reports_the_holder_instead_of_waiting() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = lock_path(&dir);

        let _held = Lock::acquire(&path).unwrap();
        let err = Lock::acquire(&path).expect_err("a held lock must not be taken twice");
        assert!(matches!(err, LockError::Held { .. }), "got: {err}");
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
    fn an_existing_unlocked_file_can_be_locked() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = lock_path(&dir);

        fs::write(&path, "left by an earlier process").unwrap();
        Lock::acquire(&path).expect("file existence does not mean the lock is held");
    }

    #[test]
    fn missing_parent_directory_is_created() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nested").join("deep").join("pack.lock");
        let _lock = Lock::acquire(&path).expect("the lock directory should be created");
        assert!(path.exists());
    }
}
