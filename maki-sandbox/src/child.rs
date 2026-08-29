use std::collections::HashMap;
use std::ffi::CStr;
use std::ffi::CString;
use std::os::unix::io::{AsFd, FromRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicU32;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, close, dup2, execve, fork, pipe, read as nix_read};
use serde_json::Value;
use tracing::{debug, error, warn};

use crate::error::SandboxError;
use crate::ipc::{self, ChildMsg, DirEntry, NO_CALL_ID, ParentMsg, ToolResultPayload};
use crate::namespace::{self, NamespaceConfig};
use crate::workload::{ChildCtx, ChildSession, RunSpec};

const ENV_SANDBOX_FD: &str = "MAKI_SANDBOX_FD";

/// Upper bound for closing extraneous file descriptors in the fork child.
/// Linux kernels typically limit default FDs to 1024.
const MAX_FD_CLOSE: i32 = 1024;

/// Socket poll timeout in the child's IO thread, in milliseconds.
const IO_POLL_TIMEOUT_MS: u16 = 100;

/// Error message for trusted-tool forwards aborted by a parent cancel/exit.
const CANCELED_MSG: &str = "canceled";

type PendingMap = HashMap<u32, Sender<Result<ToolResultPayload, String>>>;

pub(crate) struct RemoteDispatch {
    pub(crate) next_id: AtomicU32,
    pub(crate) pending: Mutex<PendingMap>,
    pub(crate) outgoing: Sender<IoCommand>,
}

impl RemoteDispatch {
    pub(crate) fn lock_pending(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, PendingMap>, SandboxError> {
        self.pending
            .lock()
            .map_err(|e| SandboxError::Ipc(format!("mutex poisoned: {e}")))
    }
}

pub(crate) enum IoCommand {
    SendChild(ChildMsg),
}

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
        let result = self.setup_sandbox();
        match result {
            Ok(true) => self.exec_inner(),
            Ok(false) => {
                warn!(
                    "sandbox child: no mount ns, skipping exec — running without filesystem isolation"
                );
                self.run_inner_no_mount_ns()
            }
            Err(e) => {
                error!("sandbox child: setup failed: {e}");
                let _ = ipc::send_child_msg(
                    &mut self.sock,
                    &ChildMsg::Done {
                        call_id: NO_CALL_ID,
                        output: None,
                        stdout: String::new(),
                        error: Some(e.to_string()),
                    },
                );
                std::process::exit(1);
            }
        }
    }

    fn setup_sandbox(&mut self) -> Result<bool, SandboxError> {
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

        let has_mount_ns = namespace::isolate_mount_ns()?;
        debug!(has_mount_ns, "sandbox child: mount namespace");

        self.config.setup_mounts(has_mount_ns)?;
        debug!("sandbox child: mounts set up");

        Ok(has_mount_ns)
    }

    fn exec_inner(self) -> ! {
        let fd = self.sock.into_raw_fd();
        if let Err(e) = fcntl(fd, FcntlArg::F_SETFD(FdFlag::empty())) {
            warn!(error = %e, "sandbox child: failed to clear FD_CLOEXEC");
        }
        unsafe {
            std::env::set_var(ENV_SANDBOX_FD, fd.to_string());
        }
        let child = std::process::Command::new("/proc/self/exe")
            .arg("--sandbox-inner")
            .spawn();
        match child {
            Ok(mut child) => {
                let status = child.wait();
                match status {
                    Ok(s) if s.success() => {
                        std::process::exit(0);
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

    fn run_inner_no_mount_ns(self) -> ! {
        InnerChild::new(self.sock).run()
    }

    fn run_inner_static() -> ! {
        let fd: i32 = if let Ok(val) = std::env::var(ENV_SANDBOX_FD) {
            if let Ok(fd) = val.parse() {
                fd
            } else {
                eprintln!("MAKI_SANDBOX_FD must be a valid fd number, got: {val}");
                std::process::exit(1);
            }
        } else {
            eprintln!("MAKI_SANDBOX_FD must be set for sandbox inner instance");
            std::process::exit(1);
        };
        unsafe {
            std::env::remove_var(ENV_SANDBOX_FD);
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

/// Work items executed by the child's worker (main) thread.
enum Work {
    Run(RunSpec),
    ToolCall {
        call_id: u32,
        name: String,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    },
    Exit,
}

/// Runs inside the isolated filesystem after setup.
///
/// One IO thread owns the socket (all reads and all writes), while the main
/// thread executes blocking work (code runs, tool calls) so slow tools never
/// stall IPC. The workload session lives only on the worker thread.
struct InnerChild {
    sock: UnixStream,
}

/// Session used when no workload is registered: fails runs and tool calls
/// while keeping query/exec traffic alive.
struct NoWorkloadSession;

impl ChildSession for NoWorkloadSession {
    fn run_code(&mut self, _spec: RunSpec) -> crate::ChildIoResult {
        crate::ChildIoResult {
            output: None,
            stdout: String::new(),
            error: Some("no child workload registered".into()),
        }
    }

    fn handle_tool_call(
        &mut self,
        _name: &str,
        _args: Vec<Value>,
        _kwargs: Vec<(String, Value)>,
    ) -> Result<String, String> {
        Err("no child workload registered".into())
    }
}

impl InnerChild {
    fn new(sock: UnixStream) -> Self {
        Self { sock }
    }

    fn run(mut self) -> ! {
        let io_sock = match self.sock.try_clone() {
            Ok(sock) => sock,
            Err(e) => {
                error!("sandbox child: socket clone failed: {e}");
                std::process::exit(1);
            }
        };

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<IoCommand>();
        let dispatch = Arc::new(RemoteDispatch {
            next_id: AtomicU32::new(1),
            pending: Mutex::new(HashMap::new()),
            outgoing: outgoing_tx.clone(),
        });
        let (work_tx, work_rx) = mpsc::channel::<Work>();

        let session: Box<dyn ChildSession> = match crate::child_workload() {
            Some(workload) => {
                match workload.init(ChildCtx::new(Arc::clone(&dispatch), outgoing_tx.clone())) {
                    Ok(session) => session,
                    Err(e) => Self::fatal(&mut self.sock, &e),
                }
            }
            // No workload registered (diag/shell binaries): keep query and
            // exec traffic alive, fail runs and tool calls lazily.
            None => Box::new(NoWorkloadSession),
        };

        if let Err(e) = std::thread::Builder::new()
            .name("sandbox-io".into())
            .spawn(move || IoHandler::run(io_sock, &outgoing_rx, &dispatch, &work_tx))
        {
            error!("sandbox child: io thread spawn failed: {e}");
            let _ = ipc::send_child_msg(
                &mut self.sock,
                &ChildMsg::Done {
                    call_id: NO_CALL_ID,
                    output: None,
                    stdout: String::new(),
                    error: Some(format!("io thread spawn failed: {e}")),
                },
            );
            std::process::exit(1);
        }

        Self::worker_loop(&work_rx, &outgoing_tx, session);
        std::process::exit(0);
    }

    /// Report a fatal setup failure to the parent and exit.
    fn fatal(sock: &mut UnixStream, error: &str) -> ! {
        error!("sandbox child: {error}");
        let _ = ipc::send_child_msg(
            sock,
            &ChildMsg::Done {
                call_id: NO_CALL_ID,
                output: None,
                stdout: String::new(),
                error: Some(error.to_string()),
            },
        );
        std::process::exit(1);
    }

    fn worker_loop(
        work_rx: &Receiver<Work>,
        outgoing_tx: &Sender<IoCommand>,
        mut session: Box<dyn ChildSession>,
    ) {
        while let Ok(work) = work_rx.recv() {
            match work {
                Work::Run(spec) => {
                    debug!(
                        call_id = spec.call_id,
                        code_len = spec.code.len(),
                        "sandbox child: running code"
                    );
                    let call_id = spec.call_id;
                    let result = session.run_code(spec);
                    let _ = outgoing_tx.send(IoCommand::SendChild(ChildMsg::Done {
                        call_id,
                        output: result.output,
                        stdout: result.stdout,
                        error: result.error,
                    }));
                }
                Work::ToolCall {
                    call_id,
                    name,
                    args,
                    kwargs,
                } => {
                    let result = session.handle_tool_call(&name, args, kwargs);
                    let payload = match result {
                        Ok(output) => ToolResultPayload {
                            output: Some(output),
                            error: None,
                        },
                        Err(error) => ToolResultPayload {
                            output: None,
                            error: Some(error),
                        },
                    };
                    let _ = outgoing_tx.send(IoCommand::SendChild(ChildMsg::ToolResult {
                        call_id,
                        result: payload,
                    }));
                }
                Work::Exit => {
                    debug!(pid = %std::process::id(), "sandbox child: worker exiting");
                    break;
                }
            }
        }
    }
}

/// IO thread handler for the sandbox child.
///
/// Owns the IPC socket: every read and every write on the child side
/// happens here, so inline query results and queued tool traffic share a
/// single writer.
struct IoHandler;

impl IoHandler {
    fn run(
        mut sock: UnixStream,
        outgoing_rx: &Receiver<IoCommand>,
        dispatch: &Arc<RemoteDispatch>,
        work_tx: &Sender<Work>,
    ) {
        loop {
            if !Self::drain_outgoing(outgoing_rx, &mut sock) {
                break;
            }

            let ready = {
                let mut pollfds = [PollFd::new(sock.as_fd(), PollFlags::POLLIN)];
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

            let msg = match ipc::recv_parent_msg(&mut sock) {
                Ok(msg) => msg,
                Err(e) => {
                    error!("sandbox-io: recv error: {e}");
                    break;
                }
            };
            if !Self::handle_parent_msg(&mut sock, msg, dispatch, work_tx) {
                break;
            }
        }
        // Whatever ended the loop, forwarded-trusted callers must not wait
        // forever for replies that can no longer arrive.
        Self::cancel_pending(dispatch);
    }

    fn drain_outgoing(outgoing_rx: &Receiver<IoCommand>, sock: &mut UnixStream) -> bool {
        loop {
            match outgoing_rx.try_recv() {
                Ok(IoCommand::SendChild(msg)) => {
                    if let Err(e) = ipc::send_child_msg(sock, &msg) {
                        error!("sandbox-io: send error: {e}");
                        return false;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => return true,
                Err(mpsc::TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn handle_parent_msg(
        sock: &mut UnixStream,
        msg: ParentMsg,
        dispatch: &RemoteDispatch,
        work_tx: &Sender<Work>,
    ) -> bool {
        match msg {
            ParentMsg::Run {
                call_id,
                code,
                timeout_secs,
                max_memory,
                config,
            } => work_tx
                .send(Work::Run(RunSpec {
                    call_id,
                    code,
                    timeout_secs,
                    max_memory,
                    config,
                }))
                .is_ok(),
            ParentMsg::ToolCall {
                call_id,
                name,
                args,
                kwargs,
            } => work_tx
                .send(Work::ToolCall {
                    call_id,
                    name,
                    args,
                    kwargs,
                })
                .is_ok(),
            ParentMsg::ToolResult { call_id, result } => {
                let _ = dispatch.lock_pending().map(|mut pending| {
                    pending.remove(&call_id).map(|tx| {
                        let _ = tx.send(Ok(result));
                    })
                });
                true
            }
            ParentMsg::Cancel => {
                Self::cancel_pending(dispatch);
                true
            }
            ParentMsg::Exec { call_id, command } => match sandbox_exec(&command, None) {
                Ok((output, is_error)) => ipc::send_child_msg(
                    sock,
                    &ChildMsg::ExecResult {
                        call_id,
                        output,
                        is_error,
                    },
                )
                .is_ok(),
                Err(e) => ipc::send_child_msg(
                    sock,
                    &ChildMsg::ExecResult {
                        call_id,
                        output: e.to_string(),
                        is_error: true,
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
                let path = std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                ipc::send_child_msg(sock, &ChildMsg::PwdResult { call_id, path }).is_ok()
            }
            ParentMsg::Cd { call_id, path } => match std::env::set_current_dir(&path) {
                Ok(()) => ipc::send_child_msg(sock, &ChildMsg::CdResult { call_id }).is_ok(),
                Err(e) => {
                    warn!(path = %path, error = %e, "sandbox child: cd failed");
                    ipc::send_child_msg(
                        sock,
                        &ChildMsg::ExecResult {
                            call_id,
                            output: format!("cd failed: {e}"),
                            is_error: true,
                        },
                    )
                    .is_ok()
                }
            },
            ParentMsg::Exit => {
                Self::cancel_pending(dispatch);
                let _ = work_tx.send(Work::Exit);
                false
            }
        }
    }

    fn cancel_pending(dispatch: &RemoteDispatch) {
        if let Ok(mut pending) = dispatch.lock_pending() {
            for tx in pending.drain().map(|(_, tx)| tx) {
                let _ = tx.send(Err(CANCELED_MSG.into()));
            }
        }
    }
}

/// Execute a shell command via fork+execve, capturing combined stdout+stderr.
///
/// Uses raw fork/execve instead of `std::process::Command` because the latter
/// uses `posix_spawnp` which fails with ENOENT inside user+mount namespaces.
pub(crate) fn sandbox_exec(
    command: &str,
    workdir: Option<&str>,
) -> Result<(String, bool), SandboxError> {
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
            if let Ok(devnull) = std::fs::File::open("/dev/null") {
                let fd = devnull.into_raw_fd();
                let _ = dup2(fd, 0); // stdin -> /dev/null
                let _ = close(fd);
            }
            let _ = close(pipe_w);

            if let Some(dir) = workdir
                && let Err(e) = std::env::set_current_dir(dir)
            {
                eprintln!("sandbox exec: chdir to {dir} failed: {e}");
                std::process::exit(126);
            }

            let a2 = CString::new(command).unwrap_or_else(|_| std::process::exit(127));

            let argv = [c"sh", c"-c", a2.as_c_str()];
            let mut env_vars: Vec<CString> = Vec::new();
            for (k, v) in std::env::vars() {
                match CString::new(format!("{k}={v}")) {
                    Ok(cs) => env_vars.push(cs),
                    Err(_) => std::process::exit(127),
                }
            }
            let envp: Vec<&CStr> = env_vars.iter().map(std::ffi::CString::as_c_str).collect();
            let _ = execve(c"/usr/bin/sh", &argv[..], &envp[..]);
            std::process::exit(127);
        }
        Ok(ForkResult::Parent { child }) => child,
        Err(e) => {
            let _ = close(pipe_r);
            let _ = close(pipe_w);
            return Err(SandboxError::Exec(format!("fork failed: {e}")));
        }
    };

    let _ = close(pipe_w);

    let mut output = String::new();
    let mut buf = [0u8; 8192];
    loop {
        match nix_read(pipe_r, &mut buf) {
            Ok(0) => break,
            Ok(n) => output.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(nix::errno::Errno::EINTR) => {}
            Err(_) => break,
        }
    }
    let _ = close(pipe_r);

    let is_error = match waitpid(child, None) {
        Ok(WaitStatus::Exited(_, code)) => code != 0,
        Ok(_) => true,
        Err(e) => return Err(SandboxError::Exec(format!("waitpid: {e}"))),
    };
    Ok((output, is_error))
}

fn list_dir_entries(path: &str) -> Vec<DirEntry> {
    let mut entries = Vec::new();
    if let Ok(rd) = std::fs::read_dir(path) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().is_ok_and(|ft| ft.is_dir());
            entries.push(DirEntry { name, is_dir });
        }
    }
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn list_dir_entries_dirs_first_then_alpha() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("z_dir")).unwrap();
        std::fs::create_dir(root.join("a_dir")).unwrap();
        std::fs::write(root.join("m_file.txt"), "x").unwrap();
        std::fs::write(root.join("a_file.txt"), "y").unwrap();

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
}
