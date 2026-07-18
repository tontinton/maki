use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use tracing::warn;

use crate::paths;

const LOG_FILE_NAME: &str = "maki.log";
const LOCK_FILE_NAME: &str = "maki.log.lock";
pub const DEFAULT_MAX_BYTES: u64 = 200 * 1024 * 1024;
pub const DEFAULT_MAX_FILES: u32 = 10;

fn file_path(dir: &Path, index: u32) -> PathBuf {
    if index == 0 {
        dir.join(LOG_FILE_NAME)
    } else {
        dir.join(format!("maki.{index}.log"))
    }
}

fn flock_exclusive(file: &File) -> io::Result<()> {
    file.lock()
}

/// Logs used to live in the state dir. Move any leftover `maki.*.log` files
/// to the logs dir once. Never overwrites: on any conflict or rename failure
/// the source file stays where it is.
fn migrate_stale_logs(old_dir: &Path, new_dir: &Path) {
    if old_dir == new_dir {
        return;
    }
    let Ok(entries) = fs::read_dir(old_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == LOCK_FILE_NAME {
            fs::remove_file(entry.path()).ok();
            continue;
        }
        if !name.starts_with("maki.") || !name.ends_with(".log") {
            continue;
        }
        let dst = new_dir.join(name);
        if !dst.exists() {
            fs::rename(entry.path(), &dst).ok();
        }
    }
}

pub struct RotatingFileWriter {
    dir: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
    max_files: u32,
}

impl RotatingFileWriter {
    pub fn new(max_bytes: u64, max_files: u32) -> io::Result<Self> {
        let logs = paths::logs_dir()?;
        if let Ok(state) = paths::state_dir() {
            migrate_stale_logs(&state, &logs);
        }
        Self::with_limits(&logs, max_bytes, max_files)
    }

    fn with_limits(dir: &Path, max_bytes: u64, max_files: u32) -> io::Result<Self> {
        let dir = dir.to_path_buf();
        let path = file_path(&dir, 0);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            dir,
            file,
            written,
            max_bytes,
            max_files,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;

        let _lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(LOCK_FILE_NAME))?;
        flock_exclusive(&_lock)?;

        let primary = file_path(&self.dir, 0);
        #[cfg(unix)]
        let needs_rotate = {
            let our_inode = self.file.metadata()?.ino();
            match fs::metadata(&primary) {
                Ok(m) => m.ino() == our_inode,
                Err(_) => true,
            }
        };
        #[cfg(not(unix))]
        let needs_rotate = true;

        if needs_rotate {
            let last = self.max_files - 1;
            match fs::remove_file(file_path(&self.dir, last)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => warn!(error = %e, "log rotate: failed to remove oldest file"),
            }

            for i in (0..last).rev() {
                let src = file_path(&self.dir, i);
                if src.exists() {
                    let dst = file_path(&self.dir, i + 1);
                    if let Err(e) = fs::rename(&src, &dst) {
                        eprintln!("maki: log rotate rename {src:?} -> {dst:?}: {e}");
                    }
                }
            }
        }

        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&primary)?;
        self.written = self.file.metadata()?.len();

        Ok(())
    }
}

impl Write for RotatingFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written >= self.max_bytes
            && let Err(e) = self.rotate()
        {
            eprintln!("maki: log rotation failed: {e}");
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MAX_BYTES: u64 = 32;
    const TEST_MAX_FILES: u32 = 3;

    fn test_writer(dir: &Path) -> RotatingFileWriter {
        RotatingFileWriter::with_limits(dir, TEST_MAX_BYTES, TEST_MAX_FILES).unwrap()
    }

    #[test]
    fn write_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = test_writer(tmp.path());
        w.write_all(b"hello\n").unwrap();
        w.flush().unwrap();

        let contents = fs::read_to_string(file_path(tmp.path(), 0)).unwrap();
        assert_eq!(contents, "hello\n");
    }

    #[test]
    fn rotates_when_size_exceeded() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = test_writer(tmp.path());

        let filler = "x".repeat(TEST_MAX_BYTES as usize);
        w.write_all(filler.as_bytes()).unwrap();
        w.flush().unwrap();

        w.write_all(b"after").unwrap();
        w.flush().unwrap();

        let current = fs::read_to_string(file_path(tmp.path(), 0)).unwrap();
        assert_eq!(current, "after");

        let rotated = fs::read_to_string(file_path(tmp.path(), 1)).unwrap();
        assert_eq!(rotated, filler);
    }

    #[test]
    fn evicts_oldest_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = test_writer(tmp.path());

        let chunk = "x".repeat(TEST_MAX_BYTES as usize);
        for _ in 0..TEST_MAX_FILES + 2 {
            w.write_all(chunk.as_bytes()).unwrap();
            w.flush().unwrap();
        }

        w.write_all(b"final").unwrap();
        w.flush().unwrap();

        assert!(!file_path(tmp.path(), TEST_MAX_FILES).exists());
    }

    #[test]
    fn resumes_existing_file_size() {
        let tmp = tempfile::tempdir().unwrap();

        {
            let mut w = test_writer(tmp.path());
            w.write_all(b"preexisting-data-that-is-long-enough")
                .unwrap();
            w.flush().unwrap();
        }

        let mut w = test_writer(tmp.path());
        w.write_all(b"new").unwrap();
        w.flush().unwrap();

        assert!(
            file_path(tmp.path(), 1).exists(),
            "should have rotated on first write since pre-existing data exceeded threshold"
        );
    }

    #[test]
    fn migrates_only_log_files_and_removes_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("state");
        let new = tmp.path().join("logs");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        for name in [
            LOG_FILE_NAME,
            "maki.1.log",
            LOCK_FILE_NAME,
            "cwd_latest.json",
        ] {
            fs::write(old.join(name), "").unwrap();
        }

        migrate_stale_logs(&old, &new);

        assert!(new.join(LOG_FILE_NAME).exists());
        assert!(new.join("maki.1.log").exists());
        assert!(!old.join(LOG_FILE_NAME).exists());
        assert!(!old.join(LOCK_FILE_NAME).exists());
        assert!(old.join("cwd_latest.json").exists());
    }

    #[test]
    fn migration_never_overwrites_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("state");
        let new = tmp.path().join("logs");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(old.join(LOG_FILE_NAME), "old").unwrap();
        fs::write(new.join(LOG_FILE_NAME), "existing").unwrap();

        migrate_stale_logs(&old, &new);

        assert_eq!(
            fs::read_to_string(new.join(LOG_FILE_NAME)).unwrap(),
            "existing"
        );
        assert_eq!(fs::read_to_string(old.join(LOG_FILE_NAME)).unwrap(), "old");
    }

    #[test]
    fn migration_keeps_live_lock_when_dirs_are_same() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(LOCK_FILE_NAME), "").unwrap();

        migrate_stale_logs(tmp.path(), tmp.path());

        assert!(tmp.path().join(LOCK_FILE_NAME).exists());
    }

    #[test]
    fn two_writers_no_data_loss() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w1 = test_writer(tmp.path());
        let mut w2 = test_writer(tmp.path());

        let filler = "x".repeat(TEST_MAX_BYTES as usize);
        w1.write_all(filler.as_bytes()).unwrap();
        w1.flush().unwrap();

        w1.write_all(b"from-w1").unwrap();
        w1.flush().unwrap();

        w2.write_all(b"from-w2").unwrap();
        w2.flush().unwrap();

        let all_content: String = (0..TEST_MAX_FILES)
            .filter_map(|i| fs::read_to_string(file_path(tmp.path(), i)).ok())
            .collect();
        assert!(all_content.contains("from-w1"));
        assert!(all_content.contains("from-w2"));
    }
}
