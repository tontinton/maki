use std::cmp::Ordering;
use std::collections::HashSet;
use std::env::{current_dir, remove_var, set_current_dir, set_var, var, vars};
use std::ffi::{CStr, CString};
use std::fs::{
    File, FileType, OpenOptions, create_dir, create_dir_all, metadata, read, read_dir, remove_dir,
    remove_dir_all, remove_file, rename, set_permissions, symlink_metadata, write,
};
use std::os::fd::BorrowedFd;
use std::os::unix::io::{AsFd, FromRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, exit, id};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use maki_fs::search::{glob_walk, grep_search};
use maki_fs::{GrepFileEntry, GrepParams};
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, close, dup2, execve, fork, pipe, read as nix_read};
use serde_json::{Value, json};
use tracing::{debug, error, warn};

use crate::error::SandboxError;
use crate::ipc::{self, ChildMsg, DirEntry, FsOp, FsReply, FsResult, ParentMsg};
use crate::namespace::{self, NamespaceConfig};

const ENV_SANDBOX_FD: &str = "MAKI_SANDBOX_FD";

/// Upper bound for closing extraneous file descriptors in the fork child.
/// Linux kernels typically limit default FDs to 1024.
const MAX_FD_CLOSE: i32 = 1024;

/// Socket poll timeout in the child's IO thread, in milliseconds.
const IO_POLL_TIMEOUT_MS: u16 = 100;

/// Exit code reported when a command is killed after its timeout, matching
/// the GNU `timeout` convention.
const EXIT_CODE_TIMED_OUT: i32 = 124;

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
/// `exec` can run long, and it is blocking by design (streaming was dropped);
/// filesystem ops are quick.
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
            call_id,
            command,
            workdir,
            timeout_secs,
        } => match sandbox_exec(&command, workdir.as_deref(), timeout_secs) {
            Ok((output, exit_code)) => ipc::send_child_msg(
                sock,
                &ChildMsg::ExecResult {
                    call_id,
                    output,
                    exit_code,
                },
            )
            .is_ok(),
            Err(e) => ipc::send_child_msg(
                sock,
                &ChildMsg::ExecResult {
                    call_id,
                    output: e.to_string(),
                    exit_code: 1,
                },
            )
            .is_ok(),
        },
        ParentMsg::Ls { call_id, path } => ipc::send_child_msg(
            sock,
            &ChildMsg::LsResult {
                call_id,
                entries: list_dir_entries(&path),
            },
        )
        .is_ok(),
        ParentMsg::Pwd { call_id } => {
            let path = current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            ipc::send_child_msg(sock, &ChildMsg::PwdResult { call_id, path }).is_ok()
        }
        ParentMsg::Cd { call_id, path } => match set_current_dir(&path) {
            Ok(()) => ipc::send_child_msg(sock, &ChildMsg::CdResult { call_id }).is_ok(),
            Err(e) => {
                warn!(path = %path, error = %e, "sandbox child: cd failed");
                ipc::send_child_msg(
                    sock,
                    &ChildMsg::ExecResult {
                        call_id,
                        output: format!("cd failed: {e}"),
                        exit_code: 1,
                    },
                )
                .is_ok()
            }
        },
        ParentMsg::Fs { call_id, op } => {
            let result = handle_fs(op);
            ipc::send_child_msg(sock, &ChildMsg::FsResult { call_id, result }).is_ok()
        }
    }
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

/// Execute a shell command via fork+execve, capturing combined stdout+stderr.
///
/// Uses raw fork/execve instead of `std::process::Command` because the latter
/// uses `posix_spawnp` which fails with ENOENT inside user+mount namespaces.
/// Returns `(output, exit_code)`. When `timeout_secs` is set, the command
/// runs in its own session (setsid) and is killed after the deadline;
/// the reported exit code is [`EXIT_CODE_TIMED_OUT`].
pub(crate) fn sandbox_exec(
    command: &str,
    workdir: Option<&str>,
    timeout_secs: Option<u64>,
) -> Result<(String, i32), SandboxError> {
    let (pipe_r, pipe_w) = pipe().map_err(|e| SandboxError::Exec(format!("pipe failed: {e}")))?;
    let pipe_r = pipe_r.into_raw_fd();
    let pipe_w = pipe_w.into_raw_fd();

    let child = match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            // ── Child: redirect output and exec ──
            let _ = close(pipe_r);
            // Close all extraneous fds
            for fd in 3..MAX_FD_CLOSE {
                if fd != pipe_w {
                    let _ = close(fd);
                }
            }
            let _ = dup2(pipe_w, 1); // stdout -> pipe
            let _ = dup2(pipe_w, 2); // stderr -> pipe
            if let Ok(devnull) = File::open("/dev/null") {
                let fd = devnull.into_raw_fd();
                let _ = dup2(fd, 0); // stdin -> /dev/null
                let _ = close(fd);
            }
            let _ = close(pipe_w);

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
            let _ = close(pipe_r);
            let _ = close(pipe_w);
            return Err(SandboxError::Exec(format!("fork failed: {e}")));
        }
    };

    let _ = close(pipe_w);

    let deadline = timeout_secs.map(|secs| Instant::now() + Duration::from_secs(secs));
    let mut timed_out = false;
    let mut eof = false;
    let mut output = String::new();
    let mut buf = [0u8; 8192];
    while !eof {
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
        let mut pollfds = [PollFd::new(
            unsafe { BorrowedFd::borrow_raw(pipe_r) },
            PollFlags::POLLIN,
        )];
        match poll(&mut pollfds, PollTimeout::from(IO_POLL_TIMEOUT_MS)) {
            Ok(0) => {}
            Ok(_) => {
                let ready = pollfds[0].revents().unwrap_or(PollFlags::empty());
                if ready.intersects(PollFlags::POLLIN | PollFlags::POLLHUP) {
                    match nix_read(pipe_r, &mut buf) {
                        Ok(0) => eof = true,
                        Ok(n) => output.push_str(&String::from_utf8_lossy(&buf[..n])),
                        Err(nix::errno::Errno::EINTR) => {}
                        Err(_) => eof = true,
                    }
                }
            }
            Err(nix::errno::Errno::EINTR) => {}
            Err(_) => eof = true,
        }
    }
    let _ = close(pipe_r);

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
    Ok((output, exit_code))
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

    use super::*;
    use tempfile::TempDir;

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
