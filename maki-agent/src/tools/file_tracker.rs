use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

pub struct FileReadTracker(Mutex<HashMap<PathBuf, FileReadInfo>>);

#[derive(Clone, Copy)]
struct FileReadInfo {
    mtime: SystemTime,
    offset: usize,
    limit: usize,
}

fn get_mtime(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn normalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

impl Default for FileReadTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl FileReadTracker {
    pub fn new() -> Self {
        Self(Mutex::new(HashMap::new()))
    }

    pub fn fresh() -> Arc<Self> {
        Arc::new(Self::new())
    }

    pub fn record_read(&self, path: &Path, offset: usize, limit: usize) {
        let normalized = normalize_path(path);
        let mtime = get_mtime(&normalized);
        self.0.lock().unwrap().insert(
            normalized,
            FileReadInfo {
                mtime,
                offset,
                limit,
            },
        );
    }

    pub fn check_before_edit(&self, path: &Path) -> Result<(), String> {
        let normalized = normalize_path(path);
        let current_mtime = get_mtime(&normalized);

        let guard = self.0.lock().unwrap();
        match guard.get(&normalized) {
            None => Err(format!(
                "file must be read before editing with read tool: {}",
                path.display()
            )),
            Some(&recorded) if recorded.mtime != current_mtime => Err(format!(
                "file changed since last read: {} - re-read using read tool before editing",
                path.display()
            )),
            Some(_) => Ok(()),
        }
    }

    /// Remove the read record for a file, allowing it to be re-read.
    pub fn clear_read(&self, path: &Path) {
        let normalized = normalize_path(path);
        self.0.lock().unwrap().remove(&normalized);
    }

    /// Check whether a read with the given parameters is allowed.
    /// Returns `Ok(())` if the file has changed since the last read
    /// or if the parameters differ. Returns an error if the read would
    /// be a duplicate of the previous read on an unchanged file.
    pub fn check_before_read(
        &self,
        path: &Path,
        offset: usize,
        limit: usize,
    ) -> Result<(), String> {
        let normalized = normalize_path(path);
        let current_mtime = get_mtime(&normalized);

        let guard = self.0.lock().unwrap();
        match guard.get(&normalized) {
            None => Ok(()),
            Some(&info) => {
                // File changed since last read - always allow.
                if info.mtime != current_mtime {
                    return Ok(());
                }
                // Same file, same content - allow only if parameters differ.
                if info.offset != offset || info.limit != limit {
                    return Ok(());
                }
                Err(format!(
                    "file {} was already read with the same parameters (offset={}, limit={}) - no need to read again",
                    path.display(),
                    offset,
                    limit
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ERR_NOT_READ: &str = "file must be read before editing";

    #[test]
    fn edit_without_read_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("untracked.rs");
        fs::write(&path, "content").unwrap();

        let tracker = FileReadTracker::new();
        let err = tracker.check_before_edit(&path).unwrap_err();
        assert!(err.contains(ERR_NOT_READ));
    }

    #[test]
    #[cfg(unix)]
    fn symlink_resolves_to_same_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let real_path = dir.path().join("real.rs");
        let link_path = dir.path().join("link.rs");
        fs::write(&real_path, "content").unwrap();
        std::os::unix::fs::symlink(&real_path, &link_path).unwrap();

        let tracker = FileReadTracker::new();
        tracker.record_read(&real_path, 1, 2000);
        tracker.check_before_edit(&link_path).unwrap();
    }

    const DUP_READ_ERR: &str = "already read with the same parameters";

    #[test]
    fn duplicate_read_with_same_params_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "content").unwrap();

        let tracker = FileReadTracker::new();
        tracker.record_read(&path, 1, 2000);
        let err = tracker.check_before_read(&path, 1, 2000).unwrap_err();
        assert!(err.contains(DUP_READ_ERR));
    }

    #[test]
    fn different_offset_allows_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "content").unwrap();

        let tracker = FileReadTracker::new();
        tracker.record_read(&path, 1, 2000);
        tracker.check_before_read(&path, 5, 2000).unwrap();
    }

    #[test]
    fn different_limit_allows_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "content").unwrap();

        let tracker = FileReadTracker::new();
        tracker.record_read(&path, 1, 2000);
        tracker.check_before_read(&path, 1, 500).unwrap();
    }

    #[test]
    fn file_changed_since_last_read_allows_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "content").unwrap();

        let tracker = FileReadTracker::new();
        tracker.record_read(&path, 1, 2000);

        // Modify the file to change its mtime
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(&path, "modified content").unwrap();

        // Same params but file changed - should allow
        tracker.check_before_read(&path, 1, 2000).unwrap();
    }

    #[test]
    fn never_read_allows_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "content").unwrap();

        let tracker = FileReadTracker::new();
        tracker.check_before_read(&path, 1, 2000).unwrap();
    }
}
