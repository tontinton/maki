//! [`maki_fs::FsBackend`] implementation over the plain host filesystem.
//!
//! Mirrors the sandbox child's native fs behavior byte-for-byte so the two
//! backends are interchangeable: same error messages, same metadata/dir
//! payload shapes, and the same lines out of a command, since both read their
//! streams through [`LineSplitter`]. Job spawning is the one thing it does not
//! share: only a host job owns a process, so [`FsBackend::exec_job`] is
//! overridden here to hand back one that can be stopped.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::{
    File, FileType, OpenOptions, create_dir, create_dir_all, metadata, read_dir, remove_dir,
    remove_dir_all, remove_file, rename, set_permissions, symlink_metadata, write,
};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio, id};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, UNIX_EPOCH};

use maki_fs::grep::{GrepFileEntry, GrepParams};
use maki_fs::search::{glob_walk, grep_search, mtime};
use maki_fs::{
    FsBackend, FsError, JobCommand, JobHandle, JobOut, JobRequest, JobSink, JobStream, LineSplitter,
};
use maki_providers::strip_provider_keys;

const READER_BUF_SIZE: usize = 8 * 1024;

/// Runs on the bare host filesystem.
pub struct HostFs;

fn err(e: impl ToString) -> FsError {
    FsError::new(e.to_string())
}

fn read_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>, FsError> {
    let file = File::open(path).map_err(err)?;
    let size = file.metadata().map_err(err)?.len();
    if size > max_bytes {
        return Err(err(format!("file exceeds the {max_bytes}-byte read limit")));
    }
    // Files can grow, and some streams report a size of zero.
    let mut bytes = Vec::with_capacity(size as usize);
    let read = file
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(err)?;
    if read as u64 > max_bytes {
        return Err(err(format!("file exceeds the {max_bytes}-byte read limit")));
    }
    Ok(bytes)
}

fn filetype_str(ft: &FileType) -> &'static str {
    if ft.is_file() {
        "file"
    } else if ft.is_dir() {
        "directory"
    } else if ft.is_symlink() {
        "link"
    } else {
        "unknown"
    }
}

fn collect_dir_entries(
    base: &Path,
    dir: &Path,
    depth: u32,
    max_depth: u32,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<(String, &'static str)>,
) {
    let entries = match read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.strip_prefix(base).ok().and_then(|p| p.to_str()) {
            Some(s) => s.to_owned(),
            None => continue,
        };
        let (type_str, is_dir) = match entry.file_type() {
            Ok(ft) if ft.is_symlink() => match metadata(&path) {
                Ok(meta) => (filetype_str(&meta.file_type()), meta.is_dir()),
                Err(_) => ("link", false),
            },
            Ok(ft) => (filetype_str(&ft), ft.is_dir()),
            Err(_) => ("unknown", false),
        };
        out.push((name, type_str));
        if is_dir && depth < max_depth {
            let canonical = match path.canonicalize() {
                Ok(c) => c,
                Err(_) => continue,
            };
            if visited.insert(canonical) {
                collect_dir_entries(base, &path, depth + 1, max_depth, visited, out);
            }
        }
    }
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<(), FsError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("out");
    let tmp = parent.join(format!(".{name}.tmp.{}", id()));
    let res = (|| -> Result<(), FsError> {
        write(&tmp, content).map_err(err)?;
        if let Ok(meta) = metadata(path)
            && let Err(e) = set_permissions(&tmp, meta.permissions())
        {
            let _ = remove_file(&tmp);
            return Err(err(e));
        }
        rename(&tmp, path).map_err(|e| {
            let _ = remove_file(&tmp);
            err(e)
        })
    })();
    let _ = remove_file(&tmp);
    res
}

impl FsBackend for HostFs {
    fn read(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, FsError> {
        read_file(path, max_bytes)
    }

    fn metadata(&self, path: &Path) -> Result<serde_json::Value, FsError> {
        let Ok(meta) = metadata(path) else {
            return Ok(serde_json::Value::Null);
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| serde_json::json!(d.as_secs_f64()));
        Ok(serde_json::json!({
            "size": meta.len(),
            "is_file": meta.is_file(),
            "is_dir": meta.is_dir(),
            "mtime": mtime,
        }))
    }

    fn dir(&self, path: &Path, depth: u32) -> Result<serde_json::Value, FsError> {
        let meta = metadata(path).map_err(err)?;
        if !meta.is_dir() {
            return Err(err(format!("dir: not a directory: {}", path.display())));
        }
        let mut out = Vec::new();
        let mut visited = HashSet::new();
        collect_dir_entries(path, path, 1, depth, &mut visited, &mut out);
        Ok(out
            .into_iter()
            .map(|(name, typ)| serde_json::json!([name, typ]))
            .collect())
    }

    fn exists(&self, path: &Path) -> Result<bool, FsError> {
        Ok(symlink_metadata(path).is_ok())
    }

    fn write(&self, path: &Path, content: &[u8]) -> Result<(), FsError> {
        write(path, content).map_err(err)
    }

    fn append(&self, path: &Path, content: &[u8]) -> Result<(), FsError> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(content))
            .map_err(err)
    }

    fn atomic_write(&self, path: &Path, content: &[u8]) -> Result<(), FsError> {
        atomic_write(path, content)
    }

    fn remove(&self, path: &Path, recursive: bool) -> Result<(), FsError> {
        let meta = symlink_metadata(path).map_err(err)?;
        if meta.is_dir() {
            if recursive {
                remove_dir_all(path).map_err(err)
            } else {
                remove_dir(path).map_err(err)
            }
        } else {
            match remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if meta.file_type().is_symlink() => remove_dir(path).map_err(|_| err(e)),
                Err(e) => Err(err(e)),
            }
        }
    }

    fn mkdir(&self, path: &Path, parents: bool) -> Result<(), FsError> {
        if parents {
            create_dir_all(path).map_err(err)
        } else {
            create_dir(path).map_err(err)
        }
    }

    fn glob(
        &self,
        root: &Path,
        patterns: &[String],
        gitignore: bool,
        sort_mtime: bool,
        limit: Option<usize>,
    ) -> Result<Vec<PathBuf>, FsError> {
        glob_walk(root, patterns, gitignore, sort_mtime, limit).map_err(err)
    }

    fn grep(&self, params: GrepParams) -> Result<Vec<GrepFileEntry>, FsError> {
        let (base, entries) = grep_search(params).map_err(err)?;
        let mut entries: Vec<GrepFileEntry> = entries
            .into_iter()
            .map(|e| GrepFileEntry {
                path: base.join(&e.path).to_string_lossy().into_owned(),
                groups: e.groups,
            })
            .collect();
        entries.sort_by_cached_key(|e| (Reverse(mtime(Path::new(&e.path))), e.path.clone()));
        Ok(entries)
    }

    fn exec(
        &self,
        command: &str,
        workdir: Option<&str>,
        timeout_secs: Option<u64>,
        sink: Arc<dyn JobSink>,
    ) -> Result<i32, FsError> {
        let (mut child, readers) = spawn_host(
            &JobCommand::Shell(command.to_owned()),
            workdir.map(PathBuf::from),
            None,
            JobOut::Capture,
            JobOut::Capture,
            &sink,
        )?;
        let reaped = Arc::new(AtomicBool::new(false));
        let _deadline = kill_after(child.id(), Arc::clone(&reaped), timeout_secs);
        let code = wait_streamed(&mut child, readers);
        reaped.store(true, Ordering::Relaxed);
        Ok(code)
    }

    /// Spawn the job and stream it: one reader thread per piped stream
    /// reports its lines as they arrive, and the wait thread reports the exit
    /// only after joining them, so an exit never overtakes the last line.
    fn exec_job(self: Arc<Self>, request: JobRequest) -> Result<JobHandle, FsError> {
        let JobRequest {
            command,
            workdir,
            env,
            stdout,
            stderr,
            sink,
        } = request;
        let (mut child, readers) = spawn_host(&command, workdir, env, stdout, stderr, &sink)?;
        let pid = child.id();
        let reaped = Arc::new(AtomicBool::new(false));
        let gone = Arc::clone(&reaped);
        std::thread::Builder::new()
            .name("job-wait".into())
            .spawn(move || {
                // Reaping frees the pid, and that pid is the process group
                // `JobHandle::kill` signals, so the flag has to be set before
                // the exit is reported.
                let code = wait_streamed(&mut child, readers);
                gone.store(true, Ordering::Relaxed);
                sink.exit(code);
            })
            .map_err(err)?;
        Ok(JobHandle::spawned(pid, reaped))
    }
}

/// Wait for a spawned command. The readers are joined first: they only return
/// once every descendant dropped the pipes, so an exit can never overtake the
/// last line, and a kill target stays around while the job is really alive.
fn wait_streamed(child: &mut Child, readers: Vec<JoinHandle<()>>) -> i32 {
    for reader in readers {
        let _ = reader.join();
    }
    child
        .wait()
        .ok()
        .map(|status| status.code().unwrap_or_else(|| signal_code(&status)))
        .unwrap_or(-1)
}

/// A watchdog that kills the job's process group once `secs` have passed,
/// unless the caller reaped it first: through a `JobHandle` a pid the kernel
/// may already have handed to someone else is never signalled. Nothing waits
/// for the watchdog, so a command that finished in time leaves it sleeping out
/// a deadline instead of holding a pipe nobody reads.
fn kill_after(pid: u32, reaped: Arc<AtomicBool>, secs: Option<u64>) -> Option<JoinHandle<()>> {
    let secs = secs?;
    std::thread::Builder::new()
        .name("exec-timeout".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            JobHandle::spawned(pid, reaped).kill();
        })
        .ok()
}

/// Spawn {command} in its own process group, so one signal reaches everything
/// it starts, piping the streams a sink should hear and one reader thread each.
/// The child goes back to the caller with its readers, which own the wait: a
/// stream that was redirected or dropped gets no reader at all.
fn spawn_host(
    command: &JobCommand,
    workdir: Option<PathBuf>,
    env: Option<HashMap<String, String>>,
    stdout: JobOut,
    stderr: JobOut,
    sink: &Arc<dyn JobSink>,
) -> Result<(Child, Vec<JoinHandle<()>>), FsError> {
    let mut cmd = match command {
        JobCommand::Shell(line) => shell_command(line),
        JobCommand::Argv(argv) => {
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]);
            cmd
        }
    };
    // Before the caller's `env` lands, so a job can still hand a key over on
    // purpose. A background job runs agent code, which must not inherit the
    // keys maki itself reads.
    strip_provider_keys(&mut cmd);
    cmd.stdin(Stdio::null())
        .stdout(stdio(stdout)?)
        .stderr(stdio(stderr)?);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe, so it is sound to call in pre_exec.
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::setsid()?;
                Ok(())
            });
        }
    }

    if let Some(dir) = workdir {
        cmd.current_dir(dir);
    }
    for (key, value) in env.into_iter().flatten() {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().map_err(err)?;
    let readers = [
        capture(
            "out-reader",
            child.stdout.take(),
            JobStream::Stdout,
            Arc::clone(sink),
        )?,
        capture(
            "err-reader",
            child.stderr.take(),
            JobStream::Stderr,
            Arc::clone(sink),
        )?,
    ];
    Ok((child, readers.into_iter().flatten().collect()))
}

fn stdio(out: JobOut) -> Result<Stdio, FsError> {
    match out {
        JobOut::Capture => Ok(Stdio::piped()),
        JobOut::Discard => Ok(Stdio::null()),
        JobOut::File(path) => File::options()
            .create(true)
            .append(true)
            .open(&path)
            .map(Stdio::from)
            .map_err(|e| FsError::new(format!("cannot open {}: {e}", path.display()))),
    }
}

/// Feed one piped stream into the sink, line by line. `None` when the stream
/// was not piped: a redirected or discarded one has no reader at all.
fn capture(
    name: &str,
    stream: Option<impl Read + Send + 'static>,
    which: JobStream,
    sink: Arc<dyn JobSink>,
) -> Result<Option<JoinHandle<()>>, FsError> {
    let Some(mut stream) = stream else {
        return Ok(None);
    };
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let mut splitter = LineSplitter::default();
            let mut buf = [0u8; READER_BUF_SIZE];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        for line in splitter.push(&buf[..read]) {
                            sink.line(which, line);
                        }
                    }
                }
            }
            if let Some(last) = splitter.flush() {
                sink.line(which, last);
            }
        })
        .map(Some)
        .map_err(err)
}

fn shell_command(cmd: &str) -> Command {
    #[cfg(unix)]
    {
        let mut c = Command::new("bash");
        c.arg("-c").arg(cmd);
        c
    }
    #[cfg(windows)]
    {
        let mut c = Command::new("cmd.exe");
        c.arg("/C").arg(cmd);
        c
    }
}

#[cfg(unix)]
fn signal_code(status: &ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    128 + status.signal().unwrap_or(0)
}

#[cfg(not(unix))]
fn signal_code(_status: &ExitStatus) -> i32 {
    0
}

#[cfg(test)]
mod tests {
    use std::fs::read_to_string;
    use std::sync::Mutex;
    use std::sync::mpsc::{self, Sender};
    use std::thread;
    use std::time::Duration;

    use test_case::test_case;

    use super::*;
    use tempfile::TempDir;

    const STREAM_TIMEOUT: Duration = Duration::from_secs(20);

    #[test_case(b""; "empty")]
    #[test_case(b"ab"; "below_limit")]
    #[test_case(b"abcd"; "at_limit")]
    fn read_accepts_contents_within_limit(contents: &[u8]) {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("f");
        write(&f, contents).unwrap();
        assert_eq!(read_file(&f, 4).unwrap(), contents);
    }

    /// A stream that never reports a size still cannot be read past the limit:
    /// the reader is capped before it starts.
    #[cfg(unix)]
    #[test]
    fn read_limits_a_stream_with_zero_reported_size() {
        const TEST_READ_LIMIT: u64 = 4;
        let err = read_file(Path::new("/dev/zero"), TEST_READ_LIMIT).unwrap_err();
        assert_eq!(err.to_string(), "file exceeds the 4-byte read limit");
    }

    #[test]
    fn metadata_missing_is_null() {
        assert_eq!(
            HostFs.metadata(Path::new("/definitely/not/here")).unwrap(),
            serde_json::Value::Null
        );
    }

    #[test_case(false; "no_recursion")]
    #[test_case(true; "recursion")]
    fn dir_roundtrip(recursive: bool) {
        let tmp = TempDir::new().unwrap();
        create_dir(tmp.path().join("a")).unwrap();
        write(tmp.path().join("a/b.txt"), "x").unwrap();
        let depth = if recursive { 3 } else { 1 };
        let payload = HostFs.dir(tmp.path(), depth).unwrap();
        let entries: Vec<Vec<String>> = serde_json::from_value(payload).unwrap();
        assert!(entries.iter().any(|e| e[0] == "a" && e[1] == "directory"));
        if recursive {
            assert!(entries.iter().any(|e| e[0] == "a/b.txt" && e[1] == "file"));
        }
    }

    #[test]
    fn dir_not_a_directory() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("f");
        write(&f, "x").unwrap();
        assert!(
            HostFs
                .dir(&f, 1)
                .unwrap_err()
                .to_string()
                .contains("dir: not a directory")
        );
    }

    #[test]
    fn atomic_write_replaces() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("f");
        atomic_write(&f, b"one").unwrap();
        atomic_write(&f, b"two").unwrap();
        assert_eq!(read_to_string(&f).unwrap(), "two");
        assert_eq!(read_dir(tmp.path()).unwrap().count(), 1);
    }

    /// Collects a run's output and passes its first line on, which is how a
    /// test knows a line arrived while the command was still going.
    struct Sink {
        lines: Mutex<Vec<(JobStream, String)>>,
        first: Sender<String>,
    }

    impl JobSink for Sink {
        fn line(&self, stream: JobStream, line: String) {
            let mut lines = self.lines.lock().unwrap();
            if lines.is_empty() {
                let _ = self.first.send(line.clone());
            }
            lines.push((stream, line));
        }
        fn exit(&self, _: i32) {}
    }

    fn sink() -> (Arc<Sink>, Arc<dyn JobSink>, mpsc::Receiver<String>) {
        let (first, received) = mpsc::channel();
        let sink = Arc::new(Sink {
            lines: Mutex::new(Vec::new()),
            first,
        });
        let erased = Arc::clone(&sink) as Arc<dyn JobSink>;
        (sink, erased, received)
    }

    #[test]
    fn exec_returns_output_and_code() {
        let (collected, erased, _) = sink();
        assert_eq!(HostFs.exec("echo hi", None, None, erased).unwrap(), 0);
        assert_eq!(
            *collected.lines.lock().unwrap(),
            [(JobStream::Stdout, "hi".to_string())]
        );
        let (_, erased, _) = sink();
        assert_eq!(HostFs.exec("exit 3", None, None, erased).unwrap(), 3);
    }

    /// Each line carries the stream it came from, so a caller can act on a
    /// diagnostic without guessing from its text.
    #[test]
    fn exec_keeps_the_two_streams_apart() {
        let (sink, erased, _) = sink();
        HostFs
            .exec("echo out; echo err 1>&2", None, None, erased)
            .unwrap();
        assert_eq!(
            *sink.lines.lock().unwrap(),
            [
                (JobStream::Stdout, "out".to_string()),
                (JobStream::Stderr, "err".to_string())
            ]
        );
    }

    /// The whole point of a sink: what a command wrote first is readable
    /// while the run goes on, not gathered once it ended.
    #[test]
    fn exec_reports_a_line_before_the_command_ends() {
        const LATE: &str = "the line only arrived after the command had ended";
        let (sink, erased, received) = sink();
        let host = Arc::new(HostFs);
        let run = thread::spawn(move || {
            host.exec("echo one; sleep 30", None, Some(1), erased)
                .unwrap()
        });

        assert_eq!(
            received.recv_timeout(STREAM_TIMEOUT).unwrap(),
            "one",
            "{LATE}"
        );
        assert!(
            !sink.lines.lock().unwrap().is_empty(),
            "the line has to be readable, not just announced: {LATE}"
        );
        assert_ne!(
            run.join().unwrap(),
            0,
            "the timeout has to stop the command"
        );
    }
}
