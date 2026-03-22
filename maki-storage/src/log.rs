use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::DataDir;

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

#[cfg(unix)]
fn flock_exclusive(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn flock_exclusive(_file: &File) -> io::Result<()> {
    Ok(())
}

pub struct RotatingFileWriter {
    dir: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
    max_files: u32,
}

impl RotatingFileWriter {
    pub fn new(data_dir: &DataDir, max_bytes: u64, max_files: u32) -> io::Result<Self> {
        Self::with_limits(data_dir.path(), max_bytes, max_files)
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
            let _ = fs::remove_file(file_path(&self.dir, last));

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
