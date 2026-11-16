//! Persistent storage. `atomic_write` writes to a `tempfile` in the same
//! directory then persists (atomic rename) for crash safety.
//! `atomic_write_permissions` sets file mode before persist (for auth keys at 0600).

pub mod auth;
pub mod frame;
pub mod id;
pub mod input_history;
pub mod intern;
pub mod lock;
pub mod log;
pub mod model;
pub mod paths;
pub mod plans;
pub mod sessions;
pub mod stdin;
pub mod theme;
pub mod trusted_folders;
pub mod version;

use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::panic;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::thread;
#[cfg(windows)]
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;
use tokio::task::{JoinHandle, spawn_blocking};

use paths::state_dir;

const CANCELLED_JOIN: &str = "joined task was cancelled";
#[cfg(windows)]
const RENAME_ATTEMPTS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir(PathBuf);

impl StateDir {
    pub fn resolve() -> Result<Self, StorageError> {
        let dir = state_dir()?;
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
    let target = fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_symlink())
        .then(|| fs::canonicalize(path).ok())
        .flatten();
    let path = target.as_deref().unwrap_or(path);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(data)?;
    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(tmp.path(), metadata.permissions())?;
    }
    tmp.as_file().sync_data()?;
    persist(tmp, path)
}

pub(crate) fn atomic_write_permissions(
    path: &Path,
    data: &[u8],
    mode: u32,
) -> Result<(), StorageError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(data)?;
    #[cfg(unix)]
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(mode))?;
    #[cfg(not(unix))]
    let _ = mode;
    tmp.as_file().sync_all()?;
    persist(tmp, path)
}

/// `into_parts` drops the auto-cleanup-on-drop guarantee, but we need the
/// File handle closed (Windows can't rename an open file) and tempfile's
/// `persist()` doesn't support the fibonacci backoff retry that Windows
/// virus scanners require. On failure, we manually clean up the temp file.
fn persist(tmp: NamedTempFile, path: &Path) -> Result<(), StorageError> {
    let (_, tmp_path) = tmp.into_parts();
    retry_rename(&tmp_path, path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        StorageError::Io(e)
    })?;
    sync_parent_dir(path);
    Ok(())
}

/// A rename is durable only once the directory entry reaches disk; without
/// this a freshly created file can vanish after power loss even though the
/// write returned Ok. Best effort: not every filesystem accepts a directory
/// fsync. Windows gets the same from `MOVEFILE_WRITE_THROUGH` in
/// `retry_rename`.
pub(crate) fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent()
        && let Ok(f) = fs::File::open(dir)
    {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Rename with fibonacci backoff to handle transient `PermissionDenied` from
/// virus scanners on Windows. 20 steps from 1ms sums to ~18 seconds.
/// Matches the pattern used by juliaup and rustup.
///
/// On non-Windows platforms, `PermissionDenied` from rename is a real
/// permissions problem (different user, immutable flag, etc.) that
/// retrying will not fix, so we just call rename once.
#[cfg(windows)]
fn retry_rename(src: &Path, dest: &Path) -> std::io::Result<()> {
    let mut a: u64 = 0;
    let mut b: u64 = 1;
    for _ in 0..RENAME_ATTEMPTS {
        match fs::rename(src, dest) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                thread::sleep(Duration::from_millis(b));
                let next = a.saturating_add(b);
                a = b;
                b = next;
            }
            Err(e) => return Err(e),
        }
    }
    fs::rename(src, dest)
}

#[cfg(not(windows))]
fn retry_rename(src: &Path, dest: &Path) -> std::io::Result<()> {
    fs::rename(src, dest)
}

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Runs one blocking job, disk work or a terminal prompt, on the blocking
/// pool. Work that touches many files goes in as one job, so it costs one hop
/// instead of one per call.
pub async fn blocking<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> T {
    joined(spawn_blocking(job)).await
}

/// Awaits a spawned task. A panic in it is a bug, so it unwinds on in the
/// caller with its own payload rather than turning into an error. Only a
/// runtime shutdown or an abort cancels a task, and that panics too.
pub async fn joined<T>(task: JoinHandle<T>) -> T {
    task.await
        .unwrap_or_else(|error| panic::resume_unwind(error.try_into_panic().expect(CANCELLED_JOIN)))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::panic::panic_any;

    use test_case::test_case;

    use super::*;

    const ORIGINAL: &[u8] = b"original";
    const PANIC_MESSAGE: &str = "job panicked";
    const JOB_OUTPUT: u32 = 7;
    const OWNER_ONLY_FILE_MODE: u32 = 0o600;
    const REPLACEMENT: &[u8] = b"replacement";
    #[cfg(unix)]
    const FILE_MODE_MASK: u32 = 0o777;

    #[test]
    fn atomic_write_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        fs::write(&path, ORIGINAL).unwrap();

        atomic_write(&path, REPLACEMENT).unwrap();

        assert_eq!(fs::read(path).unwrap(), REPLACEMENT);
    }

    #[test]
    fn atomic_write_permissions_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        fs::write(&path, ORIGINAL).unwrap();

        atomic_write_permissions(&path, REPLACEMENT, OWNER_ONLY_FILE_MODE).unwrap();

        assert_eq!(fs::read(path).unwrap(), REPLACEMENT);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_creates_owner_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");

        atomic_write(&path, ORIGINAL).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & FILE_MODE_MASK,
            OWNER_ONLY_FILE_MODE
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_destination_permissions() {
        const MODE: u32 = 0o640;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        fs::write(&path, ORIGINAL).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(MODE)).unwrap();

        atomic_write(&path, REPLACEMENT).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & FILE_MODE_MASK,
            MODE
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_keeps_a_symlinked_destination_linked() {
        let dir = tempfile::tempdir().unwrap();
        let linked = tempfile::tempdir().unwrap();
        let target = linked.path().join("state");
        let path = dir.path().join("state");
        fs::write(&target, ORIGINAL).unwrap();
        symlink(&target, &path).unwrap();

        atomic_write(&path, REPLACEMENT).unwrap();

        assert!(fs::symlink_metadata(&path).unwrap().is_symlink());
        assert_eq!(fs::read(target).unwrap(), REPLACEMENT);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_cleans_up_temp_after_replacement_failure() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("destination");
        fs::create_dir(&destination).unwrap();

        assert!(atomic_write(&destination, REPLACEMENT).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test_case(tokio::spawn(blocking(|| panic_any(PANIC_MESSAGE))) ; "blocking_job")]
    #[test_case(tokio::spawn(joined(tokio::spawn(async { panic_any(PANIC_MESSAGE) }))) ; "spawned_task")]
    #[tokio::test]
    async fn panic_reraises_with_its_payload(task: JoinHandle<()>) {
        let payload = task.await.unwrap_err().into_panic();

        assert_eq!(payload.downcast_ref::<&str>(), Some(&PANIC_MESSAGE));
    }

    #[tokio::test]
    async fn blocking_returns_job_output() {
        assert_eq!(blocking(|| JOB_OUTPUT).await, JOB_OUTPUT);
    }
}
