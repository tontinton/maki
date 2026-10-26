//! Metadata, listings and directory scans, read straight from disk and shaped
//! the way CPython reports them. Every query takes a [`Cleared`] path, and only
//! the host's yes makes one, so no unchecked path ever reaches the disk.

use std::fs::{self, FileType, Metadata};
use std::io::{Error as IoError, ErrorKind};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf, absolute};
#[cfg(not(unix))]
use std::time::UNIX_EPOCH;

use monty_types::scan::{EntryInfo, Listing, ScanSource, collect_scan};
#[cfg(unix)]
use monty_types::stat_result;
use monty_types::{ExcType, MontyException, MontyObject, ScanArgs, StringRepr};
#[cfg(not(unix))]
use monty_types::{dir_stat, file_stat};

/// A `**` over a build tree can reach millions of entries, all held in host
/// memory before the sandbox sees any of them, so one listing or scan stops here.
const MAX_SCAN_ENTRIES: u64 = 100_000;
const MAX_SCAN_BYTES: usize = 64 * 1024 * 1024;
const BROKEN_SYMLINK: EntryInfo = EntryInfo {
    is_dir: false,
    is_file: false,
    is_symlink: true,
};
#[cfg(not(unix))]
const DIR_MODE: i64 = 0o755;
#[cfg(not(unix))]
const FILE_MODE: i64 = 0o644;

/// A path the host let the script look at, along with everything under it.
pub(crate) struct Cleared {
    disk: String,
    shown: String,
}

impl Cleared {
    /// `approve` sees the very string the disk will read, so the host can
    /// never say yes to one path while we read another. `shown` is how the
    /// script spelled it, which errors and listings echo back.
    pub(crate) fn new(
        shown: &str,
        disk: String,
        approve: impl FnOnce(&str) -> Result<(), String>,
    ) -> Result<Self, String> {
        approve(&disk)?;
        Ok(Self {
            disk,
            shown: shown.to_owned(),
        })
    }

    fn on_disk(&self) -> &Path {
        Path::new(&self.disk)
    }
}

pub(crate) fn probe(
    path: &Cleared,
    test: fn(&Path) -> bool,
) -> Result<MontyObject, MontyException> {
    Ok(MontyObject::bool(test(path.on_disk())))
}

pub(crate) fn stat(path: &Cleared) -> Result<MontyObject, MontyException> {
    fs::metadata(path.on_disk())
        .map(|meta| stat_object(&meta))
        .map_err(|e| io_exception(&e, &path.shown))
}

/// Sorted, so every run sees the same order. Names that are not UTF-8 are
/// skipped, since the sandbox cannot spell them.
pub(crate) fn iterdir(path: &Cleared) -> Result<MontyObject, MontyException> {
    let mut budget = ScanBudget::default();
    let mut names = Vec::new();
    for entry in fs::read_dir(path.on_disk()).map_err(|e| io_exception(&e, &path.shown))? {
        let entry = entry.map_err(|e| io_exception(&e, &path.shown))?;
        budget.visit()?;
        if let Ok(name) = entry.file_name().into_string() {
            budget.charge(name.len())?;
            names.push(name);
        }
    }
    names.sort_unstable();
    Ok(MontyObject::list(names.into_iter().map(|name| {
        MontyObject::path(child_path(&path.shown, &name))
    })))
}

pub(crate) fn scan(path: &Cleared, args: &ScanArgs) -> Result<MontyObject, MontyException> {
    let mut source = DiskSource {
        root: path.on_disk(),
        budget: ScanBudget::default(),
    };
    match collect_scan(args, &mut source)? {
        Some(reply) => Ok(reply.into_object()),
        None => Err(io_exception(&ErrorKind::NotFound.into(), &path.shown)),
    }
}

/// `resolve()` without `strict`, walked left to right like `os.path.realpath`.
/// A symlink is followed before a later `..` acts on it, and missing names
/// stay as written.
pub(crate) fn resolve(path: &Cleared) -> PathBuf {
    let path = absolute(path.on_disk()).unwrap_or_else(|_| path.on_disk().to_path_buf());
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                if let Ok(real) = fs::canonicalize(&resolved) {
                    resolved = real;
                }
            }
            root => resolved.push(root),
        }
    }
    resolved
}

/// The directory `mkdir` still has to create, or `None` when it exists and
/// `exist_ok` says that is fine. We decide here rather than trust the host's
/// mkdir, so `FileExistsError` acts like CPython, and the host creates the
/// exact path that was cleared.
pub(crate) fn dir_to_make(
    path: &Cleared,
    exist_ok: bool,
) -> Result<Option<String>, MontyException> {
    match fs::metadata(path.on_disk()) {
        Ok(meta) if meta.is_dir() && exist_ok => Ok(None),
        Ok(_) => Err(io_exception(&ErrorKind::AlreadyExists.into(), &path.shown)),
        Err(_) => Ok(Some(path.disk.clone())),
    }
}

/// POSIX errno wording whatever the host OS, like monty's own mounts.
fn io_exception(err: &IoError, path: &str) -> MontyException {
    let path = StringRepr(path);
    let (exc_type, message) = match err.kind() {
        ErrorKind::NotFound => (
            ExcType::FileNotFoundError,
            format!("[Errno 2] No such file or directory: {path}"),
        ),
        ErrorKind::AlreadyExists => (
            ExcType::FileExistsError,
            format!("[Errno 17] File exists: {path}"),
        ),
        ErrorKind::PermissionDenied => (
            ExcType::PermissionError,
            format!("[Errno 13] Permission denied: {path}"),
        ),
        ErrorKind::IsADirectory => (
            ExcType::IsADirectoryError,
            format!("[Errno 21] Is a directory: {path}"),
        ),
        ErrorKind::NotADirectory => (
            ExcType::NotADirectoryError,
            format!("[Errno 20] Not a directory: {path}"),
        ),
        _ => (ExcType::OSError, format!("{err}: {path}")),
    };
    MontyException::new(exc_type, Some(message))
}

fn child_path(parent: &str, name: &str) -> String {
    if parent.ends_with('/') {
        format!("{parent}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// Like `os.DirEntry`, a symlink reports what it points at, and a broken one
/// is only a symlink.
fn entry_info(path: &Path) -> Option<EntryInfo> {
    let own = fs::symlink_metadata(path).ok()?.file_type();
    if !own.is_symlink() {
        return Some(info(own, false));
    }
    Some(fs::metadata(path).map_or(BROKEN_SYMLINK, |target| info(target.file_type(), true)))
}

fn info(kind: FileType, is_symlink: bool) -> EntryInfo {
    EntryInfo {
        is_dir: kind.is_dir(),
        is_file: kind.is_file(),
        is_symlink,
    }
}

fn scan_limit() -> MontyException {
    MontyException::new(
        ExcType::RuntimeError,
        Some(format!(
            "directory listing too large (over {MAX_SCAN_ENTRIES} entries or {MAX_SCAN_BYTES} bytes), list a narrower path"
        )),
    )
}

#[derive(Default)]
struct ScanBudget {
    visits: u64,
    charged: usize,
}

impl ScanBudget {
    fn visit(&mut self) -> Result<(), MontyException> {
        self.visits += 1;
        if self.visits > MAX_SCAN_ENTRIES {
            return Err(scan_limit());
        }
        Ok(())
    }

    fn charge(&mut self, bytes: usize) -> Result<(), MontyException> {
        self.charged = self.charged.saturating_add(bytes);
        if self.charged > MAX_SCAN_BYTES {
            return Err(scan_limit());
        }
        Ok(())
    }
}

struct DiskSource<'a> {
    root: &'a Path,
    budget: ScanBudget,
}

impl ScanSource for DiskSource<'_> {
    type Error = MontyException;

    /// An unreadable directory reads as missing, which glob and walk skip.
    fn list(&mut self, dir: &str) -> Result<Option<Listing>, MontyException> {
        let Ok(entries) = fs::read_dir(self.root.join(dir)) else {
            return Ok(None);
        };
        let mut listing = Vec::new();
        for entry in entries.flatten() {
            let (Ok(name), Ok(kind)) = (entry.file_name().into_string(), entry.file_type()) else {
                continue;
            };
            // The kind from the listing is free, and only a symlink costs a stat.
            let found = if kind.is_symlink() {
                entry_info(&entry.path()).unwrap_or(BROKEN_SYMLINK)
            } else {
                info(kind, false)
            };
            listing.push((name, found));
        }
        Ok(Some(listing.into()))
    }

    fn lookup(&mut self, path: &str) -> Result<Option<EntryInfo>, MontyException> {
        Ok(entry_info(&self.root.join(path)))
    }

    fn visit(&mut self) -> Result<(), MontyException> {
        self.budget.visit()
    }

    fn charge(&mut self, bytes: usize) -> Result<(), MontyException> {
        self.budget.charge(bytes)
    }
}

#[cfg(unix)]
fn stat_object(meta: &Metadata) -> MontyObject {
    let int = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
    let secs = |secs: i64, nanos: i64| secs as f64 + nanos as f64 / 1e9;
    stat_result(
        i64::from(meta.mode()),
        int(meta.ino()),
        int(meta.dev()),
        int(meta.nlink()),
        i64::from(meta.uid()),
        i64::from(meta.gid()),
        int(meta.size()),
        secs(meta.atime(), meta.atime_nsec()),
        secs(meta.mtime(), meta.mtime_nsec()),
        secs(meta.ctime(), meta.ctime_nsec()),
    )
}

#[cfg(not(unix))]
fn stat_object(meta: &Metadata) -> MontyObject {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0.0, |since| since.as_secs_f64());
    if meta.is_dir() {
        dir_stat(DIR_MODE, mtime)
    } else {
        let size = i64::try_from(meta.len()).unwrap_or(i64::MAX);
        file_stat(FILE_MODE, size, mtime)
    }
}
