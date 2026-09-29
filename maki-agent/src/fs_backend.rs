//! [`maki_fs::FsBackend`] implementation over the plain host filesystem.
//!
//! Mirrors the sandbox child's native fs behavior byte-for-byte so the two
//! backends are interchangeable: same error messages, same metadata/dir
//! payload shapes. Job spawning is the one thing it does not share: a host
//! job streams a process it spawns itself, which the sandbox cannot do, so
//! [`FsBackend::exec_job`] is overridden here and the sandbox keeps the trait
//! default that reports the whole output when the command returns.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::{
    File, FileType, OpenOptions, create_dir, create_dir_all, metadata, read_dir, remove_dir,
    remove_dir_all, remove_file, rename, set_permissions, symlink_metadata, write,
};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio, id};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::thread::sleep;
use std::time::{Duration, Instant, UNIX_EPOCH};

use maki_fs::grep::{GrepFileEntry, GrepParams};
use maki_fs::search::{glob_walk, grep_search, mtime};
use maki_fs::{FsBackend, FsError, JobCommand, JobHandle, JobOut, JobRequest, JobSink, JobStream};
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
    ) -> Result<(String, i32), FsError> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }
        let output = match timeout_secs {
            Some(secs) => run_with_timeout(&mut cmd, secs),
            None => cmd.output(),
        }
        .map_err(err)?;
        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok((
            combined,
            output.status.code().unwrap_or_else(|| signal_code(&output)),
        ))
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
        let mut child = spawn_host(&command, workdir, env, stdout, stderr)?;
        let pid = child.id();
        let reaped = Arc::new(AtomicBool::new(false));
        let gone = Arc::clone(&reaped);
        let readers = [
            capture(
                "job-stdout",
                child.stdout.take(),
                JobStream::Stdout,
                Arc::clone(&sink),
            )?,
            capture(
                "job-stderr",
                child.stderr.take(),
                JobStream::Stderr,
                Arc::clone(&sink),
            )?,
        ];
        std::thread::Builder::new()
            .name("job-wait".into())
            .spawn(move || {
                // Reaping frees the pid, and that pid is the process group
                // `JobHandle::kill` signals. The readers only return once
                // every descendant dropped the pipes, so joining them first
                // keeps a kill target around for as long as the job is
                // really alive.
                for reader in readers.into_iter().flatten() {
                    let _ = reader.join();
                }
                let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                gone.store(true, Ordering::Relaxed);
                sink.exit(code);
            })
            .map_err(err)?;
        Ok(JobHandle::spawned(pid, reaped))
    }
}

/// Spawn {command} with the requested streams, in its own process group so
/// one signal reaches everything it starts.
fn spawn_host(
    command: &JobCommand,
    workdir: Option<PathBuf>,
    env: Option<HashMap<String, String>>,
    stdout: JobOut,
    stderr: JobOut,
) -> Result<Child, FsError> {
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
    cmd.spawn().map_err(err)
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
    let Some(stream) = stream else {
        return Ok(None);
    };
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            for line in BufReader::with_capacity(READER_BUF_SIZE, stream)
                .lines()
                .map_while(Result::ok)
            {
                sink.line(which, line);
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
fn signal_code(output: &Output) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    128 + output.status.signal().unwrap_or(0)
}

#[cfg(not(unix))]
fn signal_code(_output: &Output) -> i32 {
    0
}

fn run_with_timeout(cmd: &mut Command, secs: u64) -> io::Result<Output> {
    let mut child = cmd.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(secs);
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let status = child.wait()?;
                break status;
            }
            None => sleep(Duration::from_millis(10)),
        }
    };
    let mut stdout = Vec::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_end(&mut stdout);
    }
    let mut stderr = Vec::new();
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_end(&mut stderr);
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use std::fs::read_to_string;

    use test_case::test_case;

    use super::*;
    use tempfile::TempDir;

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

    #[test]
    fn exec_returns_output_and_code() {
        let (out, code) = HostFs.exec("echo hi", None, None).unwrap();
        assert_eq!(code, 0);
        assert!(out.contains("hi"));
        let (_, code) = HostFs.exec("exit 3", None, None).unwrap();
        assert_eq!(code, 3);
    }
}
