use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::paths;

const LOG_FILE_NAME: &str = "maki.log";
const LOCK_FILE_NAME: &str = "maki.log.lock";
const ROTATED_PREFIX: &str = "maki.";
const ROTATED_SUFFIX: &str = ".log";
const MIN_MAX_FILES: u32 = 1;

fn file_path(dir: &Path, index: u32) -> PathBuf {
    if index == 0 {
        dir.join(LOG_FILE_NAME)
    } else {
        dir.join(format!("maki.{index}.log"))
    }
}

/// The one place that decides whether a file name belongs to us, so migration,
/// pruning and rotation cannot drift apart on what `maki.2.log` means.
fn rotated_index(name: &str) -> Option<u32> {
    if name == LOG_FILE_NAME {
        return Some(0);
    }
    name.strip_prefix(ROTATED_PREFIX)?
        .strip_suffix(ROTATED_SUFFIX)?
        .parse()
        .ok()
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn flock_exclusive(file: &File) -> io::Result<()> {
    file.lock()
}

/// A handle whose last name was removed. Every write it accepts goes to an
/// inode no reader can open, so on unix this is the difference between logging
/// and only appearing to.
#[cfg(unix)]
fn is_unlinked(meta: &Metadata) -> bool {
    meta.nlink() == 0
}

/// Windows refuses to rename or delete a file another process holds open, so a
/// handle here cannot be pulled out from under us.
#[cfg(not(unix))]
fn is_unlinked(_meta: &Metadata) -> bool {
    false
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
        if rotated_index(name).is_none() {
            continue;
        }
        let dst = new_dir.join(name);
        if !dst.exists() {
            fs::rename(entry.path(), &dst).ok();
        }
    }
}

/// Delete every rotated file from `first` upwards. Rotation used to remove the
/// single index it was about to overwrite, so lowering `max_log_files` stranded
/// everything above the new ceiling on disk forever.
///
/// A file that will not go away is reported and skipped instead of failing the
/// rotation. `shift_up` is what frees `maki.log`, so a leftover file costs disk
/// space while a refused rotation costs the log.
fn prune_from(dir: &Path, first: u32) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(index) = name.to_str().and_then(rotated_index) else {
            continue;
        };
        if index >= first
            && let Err(e) = fs::remove_file(entry.path())
            && e.kind() != io::ErrorKind::NotFound
        {
            report(format_args!(
                "cannot remove {}: {e}",
                entry.path().display()
            ));
        }
    }
}

/// Walk the chain downwards, so every file lands in the slot the one above it
/// just left and `maki.log` moves last.
///
/// A refused rename comes back as an error, because the caller has to learn
/// that `maki.log` is still the full file it was. Reporting it and carrying on
/// is what turned a log dir that will not accept renames into a directory scan
/// and a line of stderr per log event.
fn shift_up(dir: &Path, last: u32) -> io::Result<()> {
    for i in (0..last).rev() {
        let src = file_path(dir, i);
        if src.exists() {
            fs::rename(&src, file_path(dir, i + 1))?;
        }
    }
    Ok(())
}

/// How this file reports trouble, and the reason it is not `tracing::warn`.
/// `RotatingFileWriter` is the subscriber's own writer, so everything here runs
/// with its lock held, and a log line would take that same lock again on the
/// same thread and hang the process.
fn report(args: std::fmt::Arguments<'_>) {
    eprintln!("maki: log rotation: {args}");
}

pub struct RotatingFileWriter {
    dir: PathBuf,
    file: File,
    max_bytes: u64,
    max_files: u32,
    /// Length the file has to reach before rotation is tried again.
    retry_above: u64,
}

impl RotatingFileWriter {
    pub fn new(max_bytes: u64, max_files: u32) -> io::Result<Self> {
        let logs = paths::logs_dir()?;
        if let Ok(state) = paths::state_dir() {
            migrate_stale_logs(&state, &logs);
        }
        Self::with_limits(&logs, max_bytes, max_files)
    }

    /// `max_files` is clamped rather than trusted: config validation rejects
    /// zero, but this is a public constructor, and zero used to underflow into
    /// four billion `exists` calls made while holding the rotation lock, which
    /// wedged every other maki process on the machine along with this one.
    fn with_limits(dir: &Path, max_bytes: u64, max_files: u32) -> io::Result<Self> {
        let dir = dir.to_path_buf();
        let file = open_append(&file_path(&dir, 0))?;
        Ok(Self {
            dir,
            file,
            max_bytes,
            max_files: max_files.max(MIN_MAX_FILES),
            retry_above: 0,
        })
    }

    /// Asks the open handle, not a counter.
    ///
    /// A per-process byte count was both too small and too large: several maki
    /// processes appending to one file each counted only their own bytes, so
    /// the file grew past `max_bytes` once per process, and a quiet process
    /// never reached its own limit at all, so it never looked at its handle
    /// again while peers rotated that handle out and eventually deleted it. One
    /// `fstat` per line costs far less than the write it guards.
    ///
    /// A file that lost its last name is due at any size, since every line it
    /// takes goes to an inode nobody can open. After a failed attempt both
    /// triggers wait for another `max_bytes` of writing, so a log dir that
    /// refuses renames costs one attempt per `max_bytes` instead of one per
    /// line.
    fn rotation_due(&self, meta: &Metadata) -> bool {
        meta.len() >= self.retry_above && (is_unlinked(meta) || meta.len() >= self.max_bytes)
    }

    /// Under the rotation lock, whether `maki.log` still names our handle. A
    /// peer that rotated first already did the work and all we owe is a reopen.
    #[cfg(unix)]
    fn holds_primary(&self, primary: &Path) -> io::Result<bool> {
        let ours = self.file.metadata()?;
        if is_unlinked(&ours) {
            return Ok(false);
        }
        Ok(match fs::metadata(primary) {
            Ok(m) => m.ino() == ours.ino(),
            Err(_) => true,
        })
    }

    /// Windows has no stable way to count the names an open handle still has,
    /// so we assume ours and let the rename decide. That rename is refused as
    /// well, because `OpenOptions` never asks for `FILE_SHARE_DELETE`, so
    /// rotation always fails here and the log grows past `max_log_bytes`.
    /// Asking for the share mode is the real fix, and it wants a Windows box to
    /// test on, since it also lets a peer delete the file under us where
    /// `is_unlinked` is blind.
    #[cfg(not(unix))]
    fn holds_primary(&self, _primary: &Path) -> io::Result<bool> {
        Ok(true)
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;

        // People clearing their logs sometimes take the whole folder, and then
        // our handle has no name left and every attempt below dies on the
        // missing lock file, so the lines pile up in an inode nobody can open.
        fs::create_dir_all(&self.dir)?;

        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(LOCK_FILE_NAME))?;
        flock_exclusive(&lock)?;

        let primary = file_path(&self.dir, 0);
        if self.holds_primary(&primary)? {
            let last = self.max_files - 1;
            prune_from(&self.dir, last);
            shift_up(&self.dir, last)?;
        }

        self.file = open_append(&primary)?;

        Ok(())
    }

    /// The one `fstat` per write, lent to both the decision and the backoff. A
    /// second one would cost every log line for nothing.
    fn rotate_if_due(&mut self) {
        let Ok(meta) = self.file.metadata() else {
            return;
        };
        if !self.rotation_due(&meta) {
            return;
        }
        match self.rotate() {
            Ok(()) => self.retry_above = 0,
            Err(e) => {
                report(format_args!(
                    "{}: {e}, still writing to the open file",
                    self.dir.display()
                ));
                self.retry_above = meta.len().saturating_add(self.max_bytes);
            }
        }
    }
}

impl Write for RotatingFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.rotate_if_due();
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const TEST_MAX_BYTES: u64 = 32;
    const TEST_MAX_FILES: u32 = 3;
    const NEEDLE: &str = "needle";

    fn test_writer(dir: &Path) -> RotatingFileWriter {
        RotatingFileWriter::with_limits(dir, TEST_MAX_BYTES, TEST_MAX_FILES).unwrap()
    }

    fn read_all_logs(dir: &Path) -> String {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| rotated_index(&e.file_name().to_string_lossy()).is_some())
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .collect()
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

        let all_content = read_all_logs(tmp.path());
        assert!(all_content.contains("from-w1"));
        assert!(all_content.contains("from-w2"));
    }

    /// The bug this whole file was rewritten for. A quiet process kept its
    /// original handle while a busy peer rotated that handle down the chain
    /// and finally deleted it, after which every line it wrote went to an
    /// inode with no name and nobody noticed for months.
    #[test]
    fn peer_rotation_never_strands_our_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut quiet = test_writer(tmp.path());
        let mut busy = test_writer(tmp.path());

        let filler = "x".repeat(TEST_MAX_BYTES as usize);
        for _ in 0..TEST_MAX_FILES + 1 {
            busy.write_all(filler.as_bytes()).unwrap();
            busy.flush().unwrap();
            busy.write_all(b"tick").unwrap();
            busy.flush().unwrap();
        }

        quiet.write_all(NEEDLE.as_bytes()).unwrap();
        quiet.flush().unwrap();

        assert!(
            read_all_logs(tmp.path()).contains(NEEDLE),
            "{NEEDLE} reached no file on disk"
        );
    }

    /// Two processes sharing one file used to count only their own bytes, so
    /// the file quietly grew to `max_bytes` per process.
    #[test]
    fn size_limit_counts_every_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w1 = test_writer(tmp.path());
        let mut w2 = test_writer(tmp.path());

        let half = "x".repeat(TEST_MAX_BYTES as usize / 2 + 1);
        w1.write_all(half.as_bytes()).unwrap();
        w1.flush().unwrap();
        w2.write_all(half.as_bytes()).unwrap();
        w2.flush().unwrap();

        w1.write_all(NEEDLE.as_bytes()).unwrap();
        w1.flush().unwrap();

        assert_eq!(
            fs::read_to_string(file_path(tmp.path(), 0)).unwrap(),
            NEEDLE
        );
    }

    /// Zero used to reach `max_files - 1` and wrap to `u32::MAX`, spending
    /// four billion syscalls under the rotation lock that every other process
    /// was waiting on.
    #[test]
    fn zero_max_files_keeps_one_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = RotatingFileWriter::with_limits(tmp.path(), TEST_MAX_BYTES, 0).unwrap();

        w.write_all("x".repeat(TEST_MAX_BYTES as usize).as_bytes())
            .unwrap();
        w.write_all(NEEDLE.as_bytes()).unwrap();
        w.flush().unwrap();

        assert_eq!(
            fs::read_to_string(file_path(tmp.path(), 0)).unwrap(),
            NEEDLE
        );
        assert!(!file_path(tmp.path(), 1).exists());
    }

    #[test]
    fn lowering_the_limit_prunes_the_files_above_it() {
        let tmp = tempfile::tempdir().unwrap();
        let stranded = TEST_MAX_FILES + 3;
        for i in 1..=stranded {
            fs::write(file_path(tmp.path(), i), "old").unwrap();
        }

        let mut w = test_writer(tmp.path());
        w.write_all("x".repeat(TEST_MAX_BYTES as usize).as_bytes())
            .unwrap();
        w.write_all(NEEDLE.as_bytes()).unwrap();
        w.flush().unwrap();

        for i in TEST_MAX_FILES..=stranded {
            assert!(!file_path(tmp.path(), i).exists(), "maki.{i}.log survived");
        }
    }

    /// The size trigger cannot see this one: the file is nowhere near the
    /// limit, it simply has no name left. Someone clearing their logs by hand
    /// is the everyday version.
    /// Same story one level up, and the one the backoff would otherwise punish:
    /// a missing dir fails every retry, so the writer would sit out a whole
    /// `max_bytes` before looking again.
    #[cfg(unix)]
    #[test]
    fn deleted_log_dir_comes_back() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        fs::create_dir(&dir).unwrap();
        let mut w = test_writer(&dir);

        w.write_all(b"before").unwrap();
        fs::remove_dir_all(&dir).unwrap();

        w.write_all(NEEDLE.as_bytes()).unwrap();
        w.flush().unwrap();

        assert_eq!(fs::read_to_string(file_path(&dir, 0)).unwrap(), NEEDLE);
    }

    #[cfg(unix)]
    #[test]
    fn deleted_log_file_comes_back() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = test_writer(tmp.path());

        w.write_all(b"before").unwrap();
        fs::remove_file(file_path(tmp.path(), 0)).unwrap();

        w.write_all(NEEDLE.as_bytes()).unwrap();
        w.flush().unwrap();

        assert_eq!(
            fs::read_to_string(file_path(tmp.path(), 0)).unwrap(),
            NEEDLE
        );
    }

    /// A directory in the last slot is a rename nothing can complete, and the
    /// file below it has to move there first, so the whole chain is stuck.
    ///
    /// The write after the slot is freed is the interesting one: a writer that
    /// kept retrying per line would have rotated there and left the tail alone
    /// in a fresh `maki.log`.
    #[test]
    fn failed_rotation_keeps_the_lines_and_stops_asking() {
        let tmp = tempfile::tempdir().unwrap();
        let last = TEST_MAX_FILES - 1;
        let blocked = file_path(tmp.path(), last);
        fs::create_dir(&blocked).unwrap();
        fs::write(file_path(tmp.path(), last - 1), "rotated").unwrap();

        let mut w = test_writer(tmp.path());
        let filler = "x".repeat(TEST_MAX_BYTES as usize);
        w.write_all(filler.as_bytes()).unwrap();
        w.write_all(NEEDLE.as_bytes()).unwrap();

        fs::remove_dir(&blocked).unwrap();
        let tail = "after-the-slot-is-free";
        w.write_all(tail.as_bytes()).unwrap();
        w.flush().unwrap();

        assert_eq!(
            fs::read_to_string(file_path(tmp.path(), 0)).unwrap(),
            format!("{filler}{NEEDLE}{tail}")
        );
    }

    #[test_case(LOG_FILE_NAME, Some(0) ; "primary_is_index_zero")]
    #[test_case("maki.7.log", Some(7) ; "rotated_index_parses")]
    #[test_case(LOCK_FILE_NAME, None ; "lock_file_is_not_a_log")]
    #[test_case("maki.old.log", None ; "non_numeric_index_is_not_ours")]
    #[test_case("other.1.log", None ; "foreign_prefix_is_not_ours")]
    fn rotated_index_reads_our_names_only(name: &str, expected: Option<u32>) {
        assert_eq!(rotated_index(name), expected);
    }
}
