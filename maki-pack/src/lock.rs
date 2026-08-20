//! Advisory locks between maki processes.
//!
//! Several maki processes share one checkout directory, one lockfile, and one
//! approval store. Without a lock, two starting at the same moment would race
//! the same clone, and two writing the lockfile would lose one another's
//! entries.
//!
//! A lock is a file created exclusively, holding the pid of its owner. Holding
//! it across a whole read-modify-write is what makes concurrent updates to
//! different packages compose: atomic rename alone gives durability, not
//! isolation.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Held for as long as the operation runs. Dropping it releases the lock.
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error(
        "{path} is locked by process {pid}; if no such process is running, \
         delete that file"
    )]
    Held { path: PathBuf, pid: u32 },
    #[error("cannot lock {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Lock {
    /// Takes the lock, or reports who holds it.
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
        match Self::try_create(path) {
            Ok(lock) => Ok(lock),
            // Held by someone. Deliberately no automatic reclaim: two
            // processes that both judged a lock stale would each remove it and
            // create their own, and both would believe they held it, while
            // either one's release would delete the other's file. A lock
            // guarding a checkout is not worth that risk, so a leftover is
            // reported with its path and the user clears it.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(LockError::Held {
                path: path.to_path_buf(),
                pid: read_pid(path).unwrap_or(0),
            }),
            Err(source) => Err(LockError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    fn try_create(path: &Path) -> std::io::Result<Self> {
        let mut file: File = OpenOptions::new().write(true).create_new(true).open(path)?;
        let lock = Self {
            path: path.to_path_buf(),
        };
        let result = write!(file, "{}", std::process::id()).and_then(|()| file.sync_all());
        drop(file);
        result?;
        Ok(lock)
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    let mut text = String::new();
    File::open(path).ok()?.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
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
        assert!(!path.exists(), "dropping a lock removes its file");
    }

    /// A leftover lock is never reclaimed automatically, even when its owner is
    /// plainly gone. Two processes that both judged it stale would each create
    /// their own and both believe they held it, and either one's release would
    /// delete the other's. The error names the file so a user can clear it.
    #[test]
    fn a_leftover_lock_is_reported_rather_than_stolen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = lock_path(&dir);

        fs::write(&path, "999999").unwrap();
        let err = Lock::acquire(&path).expect_err("a leftover lock must not be stolen");
        let msg = err.to_string();
        assert!(msg.contains("999999"), "names the owner: {msg}");
        assert!(msg.contains("delete that file"), "says what to do: {msg}");
    }

    /// An unreadable lock is still held, and still names its path.
    #[test]
    fn an_unreadable_lock_is_treated_as_held() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = lock_path(&dir);

        fs::write(&path, "not a pid").unwrap();
        assert!(matches!(Lock::acquire(&path), Err(LockError::Held { .. })));
    }

    #[test]
    fn missing_parent_directory_is_created() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nested").join("deep").join("pack.lock");
        let _lock = Lock::acquire(&path).expect("the lock directory should be created");
        assert!(path.exists());
    }
}
