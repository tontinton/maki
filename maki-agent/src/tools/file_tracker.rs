use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

pub struct FileReadTracker(Mutex<HashMap<PathBuf, SystemTime>>);

fn get_mtime(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn normalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

impl FileReadTracker {
    pub fn new() -> Self {
        Self(Mutex::new(HashMap::new()))
    }

    pub fn record_read(&self, path: &Path) {
        let normalized = normalize_path(path);
        let mtime = get_mtime(&normalized);
        self.0.lock().unwrap().insert(normalized, mtime);
    }

    pub fn check_before_edit(&self, path: &Path) -> Result<(), String> {
        let normalized = normalize_path(path);
        let current_mtime = get_mtime(&normalized);

        let guard = self.0.lock().unwrap();
        match guard.get(&normalized) {
            None => Err(format!(
                "file must be read before editing: {}",
                path.display()
            )),
            Some(&recorded) if recorded != current_mtime => Err(format!(
                "file changed since last read: {} - re-read before editing",
                path.display()
            )),
            Some(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ERR_NOT_READ: &str = "file must be read before editing";
    const ERR_FILE_CHANGED: &str = "file changed since last read";

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
    fn edit_after_read_succeeds() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "content").unwrap();

        let tracker = FileReadTracker::new();
        tracker.record_read(&path);
        tracker.check_before_edit(&path).unwrap();
    }

    #[test]
    fn stale_mtime_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "original").unwrap();

        let tracker = FileReadTracker::new();
        tracker.record_read(&path);

        let normalized = normalize_path(&path);
        let stale = SystemTime::UNIX_EPOCH;
        tracker.0.lock().unwrap().insert(normalized, stale);

        let err = tracker.check_before_edit(&path).unwrap_err();
        assert!(err.contains(ERR_FILE_CHANGED));
    }

    #[test]
    fn re_record_updates_mtime() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "content").unwrap();

        let tracker = FileReadTracker::new();
        tracker.record_read(&path);

        let normalized = normalize_path(&path);
        tracker
            .0
            .lock()
            .unwrap()
            .insert(normalized.clone(), SystemTime::UNIX_EPOCH);
        assert!(tracker.check_before_edit(&path).is_err());

        tracker.record_read(&path);
        tracker.check_before_edit(&path).unwrap();
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
        tracker.record_read(&real_path);
        tracker.check_before_edit(&link_path).unwrap();
    }
}
