use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::SystemTime;

use tracing::{debug, warn};

const STALE_READ_MSG: &str = "file changed since last read";

/// Dead `Weak`s are only ever cleared once the map grows to this many entries,
/// so the map is bounded by construction instead of by a drop hook.
const LOCK_PRUNE_AT: usize = 512;

/// A path that has been through [`maki_storage::paths::canonical_key`]. Both
/// maps here are keyed by one, so no caller can key them with a raw path. Two
/// spellings of one file would get two entries, and for the lock map that
/// means no mutual exclusion at all.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct FileKey(PathBuf);

impl FileKey {
    pub fn new(path: &Path) -> Self {
        Self(maki_storage::paths::canonical_key(path))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

type FileLock = Arc<async_lock::Mutex<()>>;
pub type FileGuard = async_lock::MutexGuardArc<()>;

/// Who read what and when, plus the per-file write locks that make one tool's
/// read-modify-write safe against another's.
#[derive(Default)]
pub struct FileAccess {
    mtimes: Mutex<HashMap<FileKey, SystemTime>>,
    locks: Mutex<HashMap<FileKey, Weak<async_lock::Mutex<()>>>>,
}

fn get_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl FileAccess {
    pub fn fresh() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The write lock for one file, held for a tool's entire execution so its
    /// read-modify-write cannot interleave with another tool's on the same
    /// file. The dispatcher is the only caller: a tool that declares
    /// `mutable_path` gets this for free and must not re-implement it.
    ///
    /// A tool that declares `mutable_path` and also dispatches a child tool
    /// onto that same path would wait on itself. None does, since `batch`,
    /// `task` and `code_execution` declare no mutable path, and nothing
    /// enforces it statically. Hence the log line when a wait starts: it is
    /// the only trace such a hang would leave.
    ///
    /// One path only. A tool mutating several would need a globally sorted
    /// acquire order to avoid lock-order inversion, and none exists yet.
    pub async fn acquire(&self, key: &FileKey) -> FileGuard {
        let lock = self.lock_for(key);
        match lock.try_lock_arc() {
            Some(guard) => guard,
            None => {
                debug!(path = %key.as_path().display(), "waiting for the file lock");
                lock.lock_arc().await
            }
        }
    }

    /// The guard and every waiter own an `Arc` clone, so an entry lives
    /// exactly as long as it is in use. [`LOCK_PRUNE_AT`] only bounds the
    /// residue of dead `Weak`s left behind.
    fn lock_for(&self, key: &FileKey) -> FileLock {
        let mut map = self.locks.lock().unwrap();
        if map.len() >= LOCK_PRUNE_AT {
            map.retain(|_, weak| weak.strong_count() > 0);
        }
        map.get(key).and_then(Weak::upgrade).unwrap_or_else(|| {
            let created = FileLock::default();
            map.insert(key.clone(), Arc::downgrade(&created));
            created
        })
    }

    pub fn record_read(&self, key: &FileKey) {
        match get_mtime(key.as_path()) {
            Some(mtime) => {
                self.mtimes.lock().unwrap().insert(key.clone(), mtime);
            }
            None => warn!(
                path = %key.as_path().display(),
                "record_read: could not get mtime, file will not be tracked"
            ),
        }
    }

    pub fn check_before_edit(&self, key: &FileKey) -> Result<(), String> {
        let mut guard = self.mtimes.lock().unwrap();
        let Some(&recorded) = guard.get(key) else {
            return Ok(());
        };
        let Some(current) = get_mtime(key.as_path()) else {
            guard.remove(key);
            return Ok(());
        };
        if recorded != current {
            return Err(format!(
                "{STALE_READ_MSG}: {} - re-read using read tool before editing",
                key.as_path().display(),
            ));
        }
        Ok(())
    }
}

/// Spelling-independence belongs to [`FileKey`], which is the only way to key
/// either map, so it is tested once over `canonical_key` in `maki-storage`
/// rather than again for every map.
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_lite::future::poll_once;
    use test_case::test_case;

    use super::*;

    const FILE: &str = "f.rs";
    const CONTENT: &str = "content";

    fn future_mtime(path: &Path) {
        let future = SystemTime::now() + Duration::from_secs(10);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(future)
            .unwrap();
    }

    /// `meanwhile` stands in for everything that can happen between a read and
    /// the edit that follows it. Only a file we read and someone else changed
    /// may be refused, since anything else would block an edit for no reason.
    #[test_case(|_, _, _| {} => true ; "never_read")]
    #[test_case(|access, key, _| access.record_read(key) => true ; "read_and_untouched")]
    #[test_case(|access, key, path| {
        access.record_read(key);
        future_mtime(path);
    } => false ; "changed_after_the_read")]
    #[test_case(|access, key, path| {
        access.record_read(key);
        future_mtime(path);
        access.record_read(key);
    } => true ; "changed_then_read_again")]
    #[test_case(|access, key, path| {
        access.record_read(key);
        fs::remove_file(path).unwrap();
    } => true ; "deleted_after_the_read")]
    #[test_case(|access, key, path| {
        fs::remove_file(path).unwrap();
        access.record_read(key);
    } => true ; "read_of_a_file_that_is_gone")]
    fn edit_is_allowed_unless_the_file_moved_under_us(
        meanwhile: fn(&FileAccess, &FileKey, &Path),
    ) -> bool {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(FILE);
        fs::write(&path, CONTENT).unwrap();
        let key = FileKey::new(&path);

        let access = FileAccess::default();
        meanwhile(&access, &key, &path);
        match access.check_before_edit(&key) {
            Ok(()) => true,
            Err(message) => {
                assert!(message.contains(STALE_READ_MSG), "{message}");
                false
            }
        }
    }

    #[test]
    fn one_lock_per_file_held_until_the_guard_drops() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = FileKey::new(&dir.path().join(FILE));
        let other = FileKey::new(&dir.path().join("other.rs"));

        let access = FileAccess::default();
        smol::block_on(async {
            let held = access.acquire(&path).await;
            assert!(
                poll_once(access.acquire(&path)).await.is_none(),
                "the same file must not be lockable twice"
            );
            assert!(
                poll_once(access.acquire(&other)).await.is_some(),
                "another file must not wait on this one"
            );
            drop(held);
            assert!(poll_once(access.acquire(&path)).await.is_some());
        });
    }

    #[test]
    fn dead_locks_do_not_accumulate() {
        let dir = tempfile::TempDir::new().unwrap();
        let access = FileAccess::default();
        smol::block_on(async {
            for i in 0..=LOCK_PRUNE_AT {
                let key = FileKey::new(&dir.path().join(format!("{i}.rs")));
                drop(access.acquire(&key).await);
            }
        });
        assert!(access.locks.lock().unwrap().len() <= LOCK_PRUNE_AT);
    }
}
