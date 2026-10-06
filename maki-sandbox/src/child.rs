use std::cmp::Ordering;
use std::collections::HashSet;
use std::env::{current_dir, remove_var, set_current_dir, set_var, var, vars};
use std::ffi::{CStr, CString};
use std::fs::{
    File, FileType, OpenOptions, create_dir, create_dir_all, metadata, read, read_dir, remove_dir,
    remove_dir_all, remove_file, rename, set_permissions, symlink_metadata, write,
};
use std::os::fd::{BorrowedFd, RawFd};
use std::os::unix::io::{AsFd, FromRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, exit, id};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use maki_fs::search::{glob_walk, grep_search};
use maki_fs::{GrepFileEntry, GrepParams, JobStream, LineSplitter};
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, close, dup2, execve, fork, pipe, read as nix_read};
use serde_json::{Value, json};
use tracing::{debug, error, warn};

use crate::error::SandboxError;
use crate::ipc::{self, ChildMsg, DirEntry, FsOp, FsReply, FsResult, ParentMsg, RunId};
use crate::namespace::{self, NamespaceConfig};

const ENV_SANDBOX_FD: &str = "MAKI_SANDBOX_FD";

/// Upper bound for closing extraneous file descriptors in the fork child.
/// Linux kernels typically limit default FDs to 1024.
const MAX_FD_CLOSE: i32 = 1024;

/// Socket poll timeout in the child's IO thread, in milliseconds.
const IO_POLL_TIMEOUT_MS: u16 = 100;

/// Exit code reported when a command is killed after its timeout, matching
/// the GNU `timeout` convention.
pub(crate) const EXIT_CODE_TIMED_OUT: i32 = 124;

/// Stand-in file descriptor for a pipe whose command side is finished.
const CLOSED: RawFd = -1;

/// Bytes one read of a command's output takes at most.
const READ_BUF_SIZE: usize = 8 * 1024;

/// Scratch suffix for atomic writes; uniqueness comes from a counter.
static ATOMIC_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Sets up namespaces, then execs or enters inner loop.
struct SandboxChild {
    sock: UnixStream,
    config: NamespaceConfig,
}

impl SandboxChild {
    fn new(sock: UnixStream, config: NamespaceConfig) -> Self {
        Self { sock, config }
    }

    pub fn run(mut self) -> ! {
        match self.setup_sandbox() {
            Ok(()) => {
                let _ = ipc::send_child_msg(&mut self.sock, &ChildMsg::Setup { error: None });
                self.exec_inner()
            }
            Err(e) => {
                error!("sandbox child: setup failed: {e}");
                let _ = ipc::send_child_msg(&mut self.sock, &ChildMsg::Setup { error: Some(e) });
                exit(1);
            }
        }
    }

    fn setup_sandbox(&mut self) -> Result<(), SandboxError> {
        let parent_name = ipc::recv_handshake(&mut self.sock)?;
        if parent_name != "maki-server" {
            return Err(SandboxError::Ipc(format!(
                "unexpected parent handshake: got '{parent_name}', expected 'maki-server'"
            )));
        }
        ipc::send_handshake(&mut self.sock, "maki-child")?;

        self.config.filter_env()?;
        debug!("sandbox child: env filtered");

        namespace::isolate_user_ns(&mut self.sock)?;
        debug!("sandbox child: user namespace created");

        namespace::isolate_mount_ns()?;
        debug!("sandbox child: mount namespace created");

        self.config.setup_mounts()?;
        debug!("sandbox child: mounts set up");

        Ok(())
    }

    fn exec_inner(self) -> ! {
        let fd = self.sock.into_raw_fd();
        if let Err(e) = fcntl(fd, FcntlArg::F_SETFD(FdFlag::empty())) {
            warn!(error = %e, "sandbox child: failed to clear FD_CLOEXEC");
        }
        unsafe {
            set_var(ENV_SANDBOX_FD, fd.to_string());
        }
        let child = Command::new("/proc/self/exe")
            .arg("--sandbox-inner")
            .spawn();
        match child {
            Ok(mut child) => {
                let status = child.wait();
                match status {
                    Ok(s) if s.success() => {
                        exit(0);
                    }
                    _ => {
                        warn!("sandbox child: inner exec failed, continuing in-place");
                        Self::run_inner_static()
                    }
                }
            }
            Err(e) => {
                warn!(
                    "sandbox child: spawn failed ({e}), continuing in-place inside isolated root"
                );
                Self::run_inner_static()
            }
        }
    }

    fn run_inner_static() -> ! {
        let fd: i32 = if let Ok(val) = var(ENV_SANDBOX_FD) {
            if let Ok(fd) = val.parse() {
                fd
            } else {
                eprintln!("MAKI_SANDBOX_FD must be a valid fd number, got: {val}");
                exit(1);
            }
        } else {
            eprintln!("MAKI_SANDBOX_FD must be set for sandbox inner instance");
            exit(1);
        };
        unsafe {
            remove_var(ENV_SANDBOX_FD);
        }
        let sock = unsafe { UnixStream::from_raw_fd(fd) };
        InnerChild::new(sock).run()
    }
}

/// Entry point for the sandbox child's first invocation (fork child).
///
/// Sets up namespaces and mounts. When mount namespace is available, it
/// `pivot_roots` into the new root and execs `/proc/self/exe --sandbox-inner`
/// so the inner instance starts with a clean process state inside the
/// isolated filesystem. When mount namespace is unavailable, it calls the
/// inner loop directly (no isolation, no exec).
pub fn child_main(sock: UnixStream, ns_config: NamespaceConfig) -> ! {
    SandboxChild::new(sock, ns_config).run()
}

/// Second invocation (post-exec) entry point.
pub fn child_inner_main() -> ! {
    SandboxChild::run_inner_static()
}

/// Runs inside the isolated filesystem after setup.
///
/// A single thread owns the socket, handling every request inline. Only
/// `exec` can run long, and it is blocking by design: it holds the loop for the
/// whole run, streaming its lines to the parent as it goes; filesystem ops are
/// quick.
struct InnerChild {
    sock: UnixStream,
}

impl InnerChild {
    fn new(sock: UnixStream) -> Self {
        Self { sock }
    }

    fn run(mut self) -> ! {
        loop {
            let ready = {
                let mut pollfds = [PollFd::new(self.sock.as_fd(), PollFlags::POLLIN)];
                match poll(&mut pollfds, PollTimeout::from(IO_POLL_TIMEOUT_MS)) {
                    Ok(0) => PollFlags::empty(),
                    Ok(_) => pollfds[0].revents().unwrap_or(PollFlags::empty()),
                    Err(e) => {
                        error!("sandbox-io: poll error: {e}");
                        break;
                    }
                }
            };
            if !ready.contains(PollFlags::POLLIN) {
                continue;
            }

            let msg = match ipc::recv_parent_msg(&mut self.sock) {
                Ok(msg) => msg,
                Err(e) => {
                    error!("sandbox-io: recv error: {e}");
                    break;
                }
            };
            let reply = handle_parent_msg(&mut self.sock, msg);
            if !reply {
                break;
            }
        }
        exit(0);
    }
}

/// Handle one parent message, replying inline. Returns false to end the
/// request loop (on Exit or an unrecoverable socket error).
fn handle_parent_msg(sock: &mut UnixStream, msg: ParentMsg) -> bool {
    match msg {
        ParentMsg::Exit => false,
        ParentMsg::Exec {
            run,
            command,
            workdir,
            timeout_secs,
        } => exec_streaming(sock, run, &command, workdir.as_deref(), timeout_secs),
        ParentMsg::Ls { run, path } => ipc::send_child_msg(
            sock,
            &ChildMsg::LsResult {
                run,
                entries: list_dir_entries(&path),
            },
        )
        .is_ok(),
        ParentMsg::Pwd { run } => {
            let path = current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            ipc::send_child_msg(sock, &ChildMsg::PwdResult { run, path }).is_ok()
        }
        ParentMsg::Cd { run, path } => match set_current_dir(&path) {
            Ok(()) => ipc::send_child_msg(sock, &ChildMsg::CdResult { run, error: None }).is_ok(),
            Err(e) => {
                warn!(path = %path, error = %e, "sandbox child: cd failed");
                ipc::send_child_msg(
                    sock,
                    &ChildMsg::CdResult {
                        run,
                        error: Some(format!("cd failed: {e}")),
                    },
                )
                .is_ok()
            }
        },
        ParentMsg::Fs { run, op } => {
            let result = handle_fs(op);
            ipc::send_child_msg(sock, &ChildMsg::FsResult { run, result }).is_ok()
        }
    }
}

/// Run a command, sending every output line to the parent as the command
/// writes it, then its exit code. The socket is owned by the request loop and
/// only written to here, so a run in flight keeps every other request waiting,
/// exactly as a blocking exec did: what changes is that the caller watches it.
/// Returns false once the socket is gone.
fn exec_streaming(
    sock: &mut UnixStream,
    run: RunId,
    command: &str,
    workdir: Option<&str>,
    timeout_secs: Option<u64>,
) -> bool {
    let mut socket_lost = false;
    let exit_code = match sandbox_exec(command, workdir, timeout_secs, |stream, line| {
        if ipc::send_child_msg(sock, &ChildMsg::ExecLine { run, stream, line }).is_err() {
            socket_lost = true;
        }
    }) {
        Ok(exit_code) => exit_code,
        // The reason a command never ran is the one thing its caller has to
        // hear, so it travels as output with the exit it got.
        Err(e) => {
            error!(command = %command, error = %e, "sandbox child: exec failed");
            let _ = ipc::send_child_msg(
                sock,
                &ChildMsg::ExecLine {
                    run,
                    stream: JobStream::Stderr,
                    line: e.to_string(),
                },
            );
            1
        }
    };
    ipc::send_child_msg(sock, &ChildMsg::ExecResult { run, exit_code }).is_ok() && !socket_lost
}

/// Execute a filesystem operation inside the namespace.
fn handle_fs(op: FsOp) -> FsResult {
    match op {
        FsOp::Read { path, max_bytes } => match fs_read(&path, max_bytes) {
            Ok(bytes) => FsResult::Ok(FsReply::Bytes {
                data: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes),
            }),
            Err(e) => FsResult::Err(e),
        },
        FsOp::Metadata { path } => FsResult::Ok(FsReply::Value {
            payload: fs_metadata(&path),
        }),
        FsOp::Dir { path, depth } => match fs_dir(&path, depth) {
            Ok(payload) => FsResult::Ok(FsReply::Value { payload }),
            Err(e) => FsResult::Err(e),
        },
        FsOp::Exists { path } => FsResult::Ok(FsReply::Value {
            payload: Value::Bool(symlink_metadata(&path).is_ok()),
        }),
        FsOp::Write { path, content } => match fs_write(&path, &content) {
            Ok(()) => FsResult::Ok(FsReply::Done),
            Err(e) => FsResult::Err(e),
        },
        FsOp::Append { path, content } => match fs_append(&path, &content) {
            Ok(()) => FsResult::Ok(FsReply::Done),
            Err(e) => FsResult::Err(e),
        },
        FsOp::AtomicWrite { path, content } => match fs_atomic_write(&path, &content) {
            Ok(()) => FsResult::Ok(FsReply::Done),
            Err(e) => FsResult::Err(e),
        },
        FsOp::Remove { path, recursive } => match fs_remove(&path, recursive) {
            Ok(()) => FsResult::Ok(FsReply::Done),
            Err(e) => FsResult::Err(e),
        },
        FsOp::Mkdir { path, parents } => {
            let res = if parents {
                create_dir_all(&path)
            } else {
                create_dir(&path)
            };
            match res {
                Ok(()) => FsResult::Ok(FsReply::Done),
                Err(e) => FsResult::Err(e.to_string()),
            }
        }
        FsOp::Glob {
            path,
            patterns,
            gitignore,
            sort_mtime,
            limit,
        } => {
            if patterns.is_empty() {
                return FsResult::Err("glob: at least one pattern is required".into());
            }
            match glob_walk(Path::new(&path), &patterns, gitignore, sort_mtime, limit) {
                Ok(paths) => value_reply(&paths),
                Err(e) => FsResult::Err(e),
            }
        }
        FsOp::Grep {
            path,
            pattern,
            include,
            context_before,
            context_after,
            limit,
            max_line_bytes,
        } => {
            let params = GrepParams {
                pattern,
                path,
                include,
                context_before,
                context_after,
                limit,
                max_line_bytes,
            };
            match grep_search(params) {
                Ok((base, entries)) => {
                    let absolute: Vec<GrepFileEntry> = entries
                        .into_iter()
                        .map(|e| GrepFileEntry {
                            path: base.join(&e.path).to_string_lossy().into_owned(),
                            groups: e.groups,
                        })
                        .collect();
                    value_reply(&absolute)
                }
                Err(e) => FsResult::Err(e),
            }
        }
    }
}

fn value_reply<T: serde::Serialize>(payload: &T) -> FsResult {
    match serde_json::to_value(payload) {
        Ok(payload) => FsResult::Ok(FsReply::Value { payload }),
        Err(e) => FsResult::Err(e.to_string()),
    }
}

fn fs_read(path: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
    let meta = metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > max_bytes {
        return Err(format!("file exceeds the {max_bytes}-byte read limit"));
    }
    let bytes = read(path).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!("file exceeds the {max_bytes}-byte read limit"));
    }
    Ok(bytes)
}

fn fs_metadata(path: &str) -> Value {
    let Ok(meta) = metadata(path) else {
        return Value::Null;
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| json!(d.as_secs_f64()));
    json!({
        "size": meta.len(),
        "is_file": meta.is_file(),
        "is_dir": meta.is_dir(),
        "mtime": mtime,
    })
}

fn fs_dir(path: &str, max_depth: u32) -> Result<Value, String> {
    let meta = metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_dir() {
        return Err(format!("dir: not a directory: {path}"));
    }
    let base = PathBuf::from(path);
    let mut out = Vec::new();
    let mut visited = HashSet::new();
    collect_dir_entries(&base, &base, 1, max_depth, &mut visited, &mut out);
    let payload: Value = out
        .into_iter()
        .map(|(name, typ)| json!([name, typ]))
        .collect();
    Ok(payload)
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

fn fs_write(path: &str, content_b64: &str) -> Result<(), String> {
    let bytes = decode(content_b64)?;
    write(path, &bytes).map_err(|e| e.to_string())
}

fn fs_append(path: &str, content_b64: &str) -> Result<(), String> {
    use std::io::Write;
    let bytes = decode(content_b64)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(&bytes))
        .map_err(|e| e.to_string())
}

fn fs_atomic_write(path: &str, content_b64: &str) -> Result<(), String> {
    let bytes = decode(content_b64)?;
    let target = Path::new(path);
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("out");
    let counter = ATOMIC_WRITE_COUNTER.fetch_add(1, AtomicOrdering::SeqCst);
    let tmp = parent.join(format!(".{name}.tmp.{}.{counter}", id()));
    let res = (|| -> Result<(), String> {
        write(&tmp, &bytes).map_err(|e| e.to_string())?;
        if let Ok(meta) = metadata(path)
            && let Err(e) = set_permissions(&tmp, meta.permissions())
        {
            let _ = remove_file(&tmp);
            return Err(e.to_string());
        }
        rename(&tmp, target).map_err(|e| {
            let _ = remove_file(&tmp);
            e.to_string()
        })
    })();
    let _ = remove_file(&tmp);
    res
}

fn fs_remove(path: &str, recursive: bool) -> Result<(), String> {
    let meta = symlink_metadata(path).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        if recursive {
            remove_dir_all(path).map_err(|e| e.to_string())
        } else {
            remove_dir(path).map_err(|e| e.to_string())
        }
    } else {
        match remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if meta.file_type().is_symlink() => remove_dir(path).map_err(|_| e.to_string()),
            Err(e) => Err(e.to_string()),
        }
    }
}

fn decode(content_b64: &str) -> Result<Vec<u8>, String> {
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, content_b64)
        .map_err(|e| format!("invalid base64 content: {e}"))
}

/// One of a command's output pipes, and the line it has read so far. A `fd` of
/// [`CLOSED`] marks a stream the command is done with.
struct PipeReader {
    fd: RawFd,
    stream: JobStream,
    lines: LineSplitter,
}

impl PipeReader {
    fn new(stream: JobStream, read: RawFd) -> Self {
        Self {
            fd: read,
            stream,
            lines: LineSplitter::default(),
        }
    }

    /// Take whatever this stream has ready and report its lines. False once the
    /// stream ended, the line it wrote without a closing newline included.
    fn drain(&mut self, buf: &mut [u8], on_line: &mut impl FnMut(JobStream, String)) -> bool {
        match nix_read(self.fd, buf) {
            Ok(0) => {
                self.end(on_line);
                false
            }
            Ok(read) => {
                for line in self.lines.push(&buf[..read]) {
                    on_line(self.stream, line);
                }
                true
            }
            Err(nix::errno::Errno::EINTR) => true,
            Err(_) => {
                self.end(on_line);
                false
            }
        }
    }

    /// Give up on this stream, keeping the line it left unfinished.
    fn end(&mut self, on_line: &mut impl FnMut(JobStream, String)) {
        if let Some(last) = self.lines.flush() {
            on_line(self.stream, last);
        }
        self.fd = CLOSED;
    }
}

/// Execute a shell command via fork+execve, handing every output line to
/// {on_line} as the command writes it, and return its exit code.
///
/// Uses raw fork/execve instead of `std::process::Command` because the latter
/// uses `posix_spawnp` which fails with ENOENT inside user+mount namespaces.
/// Each stream gets a pipe of its own, so a line knows where it came from. When
/// `timeout_secs` is set, the command runs in its own session (setsid) and is
/// killed after the deadline; the reported exit code is
/// [`EXIT_CODE_TIMED_OUT`].
pub(crate) fn sandbox_exec(
    command: &str,
    workdir: Option<&str>,
    timeout_secs: Option<u64>,
    mut on_line: impl FnMut(JobStream, String),
) -> Result<i32, SandboxError> {
    let pipe_failed = |e| SandboxError::Exec(format!("pipe failed: {e}"));
    let (out_read, out_write) = pipe().map_err(pipe_failed)?;
    let (err_read, err_write) = pipe().map_err(pipe_failed)?;
    let out_read = out_read.into_raw_fd();
    let out_write = out_write.into_raw_fd();
    let err_read = err_read.into_raw_fd();
    let err_write = err_write.into_raw_fd();

    let child = match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            // ── Child: redirect output and exec ──
            let _ = close(out_read);
            let _ = close(err_read);
            // Close all extraneous fds
            for fd in 3..MAX_FD_CLOSE {
                if fd != out_write && fd != err_write {
                    let _ = close(fd);
                }
            }
            let _ = dup2(out_write, 1); // stdout -> its pipe
            let _ = dup2(err_write, 2); // stderr -> its pipe
            if let Ok(devnull) = File::open("/dev/null") {
                let fd = devnull.into_raw_fd();
                let _ = dup2(fd, 0); // stdin -> /dev/null
                let _ = close(fd);
            }
            let _ = close(out_write);
            let _ = close(err_write);

            // Own process group: the parent can kill the whole command tree
            // on timeout.
            if timeout_secs.is_some() {
                let _ = nix::unistd::setsid();
            }

            if let Some(dir) = workdir
                && let Err(e) = set_current_dir(dir)
            {
                eprintln!("sandbox exec: chdir to {dir} failed: {e}");
                exit(126);
            }

            let a2 = CString::new(command).unwrap_or_else(|_| exit(127));

            let argv = [c"sh", c"-c", a2.as_c_str()];
            let mut env_vars: Vec<CString> = Vec::new();
            for (k, v) in vars() {
                match CString::new(format!("{k}={v}")) {
                    Ok(cs) => env_vars.push(cs),
                    Err(_) => exit(127),
                }
            }
            let envp: Vec<&CStr> = env_vars.iter().map(CString::as_c_str).collect();
            let _ = execve(c"/usr/bin/sh", &argv[..], &envp[..]);
            exit(127);
        }
        Ok(ForkResult::Parent { child }) => child,
        Err(e) => {
            for fd in [out_read, out_write, err_read, err_write] {
                let _ = close(fd);
            }
            return Err(SandboxError::Exec(format!("fork failed: {e}")));
        }
    };

    let _ = close(out_write);
    let _ = close(err_write);

    let mut readers = [
        PipeReader::new(JobStream::Stdout, out_read),
        PipeReader::new(JobStream::Stderr, err_read),
    ];
    let deadline = timeout_secs.map(|secs| Instant::now() + Duration::from_secs(secs));
    let mut timed_out = false;
    let mut buf = [0u8; READ_BUF_SIZE];
    while readers.iter().any(|reader| reader.fd != CLOSED) {
        if let Some(dl) = deadline
            && !timed_out
            && Instant::now() >= dl
        {
            timed_out = true;
            let _ = kill(
                nix::unistd::Pid::from_raw(-(child.as_raw())),
                Signal::SIGKILL,
            );
        }
        let open: Vec<usize> = (0..readers.len())
            .filter(|i| readers[*i].fd != CLOSED)
            .collect();
        let mut ready: Vec<PollFd> = open
            .iter()
            .map(|i| {
                let fd = readers[*i].fd;
                PollFd::new(unsafe { BorrowedFd::borrow_raw(fd) }, PollFlags::POLLIN)
            })
            .collect();
        match poll(ready.as_mut_slice(), PollTimeout::from(IO_POLL_TIMEOUT_MS)) {
            Ok(0) => {}
            Ok(_) => {
                for (index, ready) in open.iter().zip(&ready) {
                    if ready
                        .revents()
                        .unwrap_or(PollFlags::empty())
                        .intersects(PollFlags::POLLIN | PollFlags::POLLHUP)
                    {
                        readers[*index].drain(&mut buf, &mut on_line);
                    }
                }
            }
            Err(nix::errno::Errno::EINTR) => {}
            Err(_) => {
                for reader in &mut readers {
                    reader.end(&mut on_line);
                }
            }
        }
    }
    for fd in readers
        .iter()
        .map(|reader| reader.fd)
        .filter(|fd| *fd != CLOSED)
    {
        let _ = close(fd);
    }

    let exit_code = loop {
        match waitpid(child, None) {
            Ok(WaitStatus::Exited(_, code)) => {
                break if timed_out { EXIT_CODE_TIMED_OUT } else { code };
            }
            Ok(WaitStatus::Signaled(_, sig, _)) => {
                break if timed_out {
                    EXIT_CODE_TIMED_OUT
                } else {
                    128 + sig as i32
                };
            }
            Ok(_) => {}
            Err(e) => return Err(SandboxError::Exec(format!("waitpid: {e}"))),
        }
    };
    Ok(exit_code)
}

fn list_dir_entries(path: &str) -> Vec<DirEntry> {
    let mut entries = Vec::new();
    if let Ok(rd) = read_dir(path) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().is_ok_and(|ft| ft.is_dir());
            entries.push(DirEntry { name, is_dir });
        }
    }
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    entries
}

#[cfg(test)]
mod tests {
    use std::fs::read_to_string;
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};

    use maki_fs::JobSink;

    use super::*;
    use crate::{PendingMap, SandboxResponse, StreamMap};
    use tempfile::TempDir;

    const A_RUN: RunId = RunId(3);

    /// Long enough that a line from a command still running cannot have been
    /// held back by the deadline of an unrelated test.
    const LINE_TIMEOUT: Duration = Duration::from_secs(30);
    /// A command sleeping this long is only ever reached by a test that meant
    /// to kill it, either by timeout or by asserting on a live line.
    const NEVER_RETURNS: &str = "sleep 30";
    const LATE: &str = "the line only arrived after the command had ended";

    /// Run a command on a worker thread, collecting what it streams. The exec
    /// itself blocks until the command is gone, which is what makes the
    /// arrival times of its lines worth asserting on.
    type Collected = (JobStream, String);
    type Run = JoinHandle<Result<i32, SandboxError>>;

    fn exec_collecting(command: String, timeout_secs: Option<u64>) -> (Run, Receiver<Collected>) {
        let (tx, rx) = channel();
        let run = thread::spawn(move || {
            sandbox_exec(&command, None, timeout_secs, move |stream, line| {
                let _ = tx.send((stream, line));
            })
        });
        (run, rx)
    }

    fn next_line(rx: &Receiver<(JobStream, String)>) -> (JobStream, String) {
        rx.recv_timeout(LINE_TIMEOUT)
            .expect("a line the command wrote")
    }

    /// A sink that hands what a run wrote to a channel, so a test can read it
    /// while the run is still going.
    struct Forwarding(Sender<Collected>);

    impl JobSink for Forwarding {
        fn line(&self, stream: JobStream, line: String) {
            let _ = self.0.send((stream, line));
        }
        fn exit(&self, _: i32) {}
    }

    /// The whole path a command in the sandbox takes: a real child forking
    /// `sh`, a real socket, and the parent IO thread feeding the sink of the
    /// run that asked for it. Only the namespaces are missing, which is why
    /// this covers what the end-to-end sandbox test cannot run everywhere.
    #[test]
    fn a_command_streams_over_the_socket_to_the_sink_of_its_run() {
        let (parent_sock, mut child_sock) = UnixStream::pair().unwrap();
        let (inbound, requests) = channel();
        let pending = Arc::new(Mutex::new(PendingMap::new()));
        let streams = Arc::new(Mutex::new(StreamMap::new()));
        let (lines, received) = channel();
        streams
            .lock()
            .unwrap()
            .insert(A_RUN, Arc::new(Forwarding(lines)));
        let (waiting, exited) = channel();
        pending.lock().unwrap().insert(A_RUN, waiting);
        let io = crate::parent_io_thread(parent_sock, requests, pending, streams).unwrap();

        let child = thread::spawn(move || {
            handle_parent_msg(
                &mut child_sock,
                ParentMsg::Exec {
                    run: A_RUN,
                    command: format!("echo one; {NEVER_RETURNS}"),
                    workdir: None,
                    timeout_secs: Some(1),
                },
            )
        });

        assert_eq!(
            next_line(&received),
            (JobStream::Stdout, "one".into()),
            "{LATE}"
        );
        assert!(child.join().unwrap(), "the child keeps the socket open");
        assert!(
            matches!(
                exited.recv_timeout(LINE_TIMEOUT),
                Ok(Ok(SandboxResponse::Exec(EXIT_CODE_TIMED_OUT)))
            ),
            "the waiter learns the exit code over the same socket"
        );

        drop(inbound);
        io.join().unwrap();
    }

    #[test]
    fn sandbox_exec_reports_a_line_before_the_command_ends() {
        let (run, rx) = exec_collecting(format!("echo one; {NEVER_RETURNS}"), Some(1));

        assert_eq!(next_line(&rx), (JobStream::Stdout, "one".into()), "{LATE}");
        assert_eq!(run.join().unwrap().unwrap(), EXIT_CODE_TIMED_OUT);
    }

    /// Each stream is a pipe of its own, so a line carries where it came from
    /// instead of the run reporting one merged blob.
    #[test]
    fn sandbox_exec_keeps_the_streams_apart() {
        let (run, rx) = exec_collecting("echo out; echo err 1>&2".into(), None);

        let mut lines = vec![next_line(&rx), next_line(&rx)];
        lines.sort_by_key(|(_, line)| line.clone());
        assert_eq!(run.join().unwrap().unwrap(), 0);
        assert_eq!(
            lines,
            [
                (JobStream::Stderr, "err".into()),
                (JobStream::Stdout, "out".into())
            ]
        );
    }

    /// A command that ends without a closing newline still had something to
    /// say, so its last line is reported rather than dropped.
    #[test]
    fn sandbox_exec_reports_a_last_line_without_a_newline() {
        let (run, rx) = exec_collecting("printf out; printf err 1>&2".into(), None);

        let mut lines = vec![next_line(&rx), next_line(&rx)];
        lines.sort_by_key(|(_, line)| line.clone());
        assert_eq!(run.join().unwrap().unwrap(), 0);
        assert_eq!(
            lines,
            [
                (JobStream::Stderr, "err".into()),
                (JobStream::Stdout, "out".into())
            ]
        );
    }

    #[test]
    fn sandbox_exec_reports_the_commands_exit_code() {
        let (run, _) = exec_collecting("exit 3".into(), None);
        assert_eq!(run.join().unwrap().unwrap(), 3);
    }

    #[test]
    fn list_dir_entries_dirs_first_then_alpha() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        create_dir(root.join("z_dir")).unwrap();
        create_dir(root.join("a_dir")).unwrap();
        write(root.join("m_file.txt"), "x").unwrap();
        write(root.join("a_file.txt"), "y").unwrap();

        let entries = list_dir_entries(&root.to_string_lossy());
        assert_eq!(entries.len(), 4);
        assert!(entries[0].is_dir);
        assert_eq!(entries[0].name, "a_dir");
        assert!(entries[1].is_dir);
        assert_eq!(entries[1].name, "z_dir");
        assert!(!entries[2].is_dir);
        assert_eq!(entries[2].name, "a_file.txt");
        assert!(!entries[3].is_dir);
        assert_eq!(entries[3].name, "m_file.txt");
    }

    #[test]
    fn list_dir_entries_nonexistent_returns_empty() {
        let entries = list_dir_entries("/nonexistent/path/that/does/not/exist");
        assert!(entries.is_empty());
    }

    #[test]
    fn fs_dir_shallow_and_nested_depth() {
        let tmp = TempDir::new().unwrap();
        create_dir(tmp.path().join("a")).unwrap();
        create_dir(tmp.path().join("a/b")).unwrap();
        write(tmp.path().join("a/b/c.txt"), "x").unwrap();

        let shallow = fs_dir(&tmp.path().to_string_lossy(), 1).unwrap();
        let shallow: Vec<Vec<String>> = serde_json::from_value(shallow).unwrap();
        assert_eq!(shallow.len(), 1);
        assert_eq!(shallow[0][0], "a");
        assert_eq!(shallow[0][1], "directory");

        let nested = fs_dir(&tmp.path().to_string_lossy(), 3).unwrap();
        let nested: Vec<(String, String)> = serde_json::from_value(nested).unwrap();
        assert!(nested.iter().any(|(n, t)| n == "a/b/c.txt" && t == "file"));
    }

    #[test]
    fn fs_metadata_variants() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path().join("f"), "hello").unwrap();
        let meta = fs_metadata(&tmp.path().join("f").to_string_lossy());
        assert_eq!(meta["size"], 5);
        assert!(meta["is_file"].as_bool().unwrap());
        assert!(!meta["is_dir"].as_bool().unwrap());
        let missing = fs_metadata("/definitely/not/here");
        assert_eq!(missing, Value::Null);
    }

    #[test]
    fn fs_read_respects_max_bytes() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path().join("f"), "hello").unwrap();
        let path = tmp.path().join("f").to_string_lossy().to_string();
        assert_eq!(
            fs_read(&path, 4).unwrap_err(),
            "file exceeds the 4-byte read limit"
        );
        assert_eq!(fs_read(&path, 5).unwrap(), b"hello");
    }

    #[test]
    fn fs_write_append_atomic_write_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("f").to_string_lossy().into_owned();
        let enc = |b: &[u8]| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b);
        fs_write(&path, &enc(b"one")).unwrap();
        fs_append(&path, &enc(b"two")).unwrap();
        assert_eq!(read_to_string(&path).unwrap(), "onetwo");
        fs_atomic_write(&path, &enc(b"three")).unwrap();
        assert_eq!(read_to_string(&path).unwrap(), "three");
        assert_eq!(read_dir(tmp.path()).unwrap().count(), 1);
    }
}
