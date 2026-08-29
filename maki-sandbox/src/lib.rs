#![cfg(all(feature = "sandbox", target_os = "linux"))]

pub mod child;
pub mod error;
pub mod ipc;
pub mod namespace;
pub mod profiles;
pub mod sandbox;
pub mod workload;

use std::collections::HashMap;
use std::os::unix::io::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, getgid, getuid};
use serde_json::Value;
use tracing::{debug, error, warn};

use crate::error::SandboxError;
use crate::ipc::{
    ChildMsg, DirEntry, NO_CALL_ID, ParentMsg, SYNC_GO, SYNC_READY, ToolResultPayload,
};
use crate::namespace::NamespaceConfig;

/// Socket poll timeout in the parent's IO thread, in milliseconds.
const IO_POLL_TIMEOUT_MS: u16 = 100;

const SHUTDOWN_MSG: &str = "sandbox shutting down";

/// Acquire a mutex lock, converting poison to [`SandboxError`].
pub fn lock_or_poisoned<T>(mutex: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, SandboxError> {
    mutex
        .lock()
        .map_err(|e| SandboxError::MutexPoisoned(e.to_string()))
}

pub use sandbox::Sandbox;
pub use workload::{child_workload, register_child_workload};

/// Spawn a sandboxed child process.
///
/// Returns the child PID and the parent end of the IPC socket.
/// The child has already:
/// - Filtered its environment
/// - Created a user namespace (uid/gid mapped)
/// - Created a mount namespace
/// - Set up bind mounts
///
/// After this returns, the child is in its persistent IO loop and accepts
/// [`ParentMsg`](crate::ipc::ParentMsg) requests (runs, tool calls, queries)
/// over the socket.
pub fn spawn_child(config: NamespaceConfig) -> Result<(Pid, UnixStream), SandboxError> {
    let (mut parent_sock, child_sock) =
        UnixStream::pair().map_err(|e| SandboxError::Ipc(format!("socketpair: {e}")))?;

    match unsafe { nix::unistd::fork() }.map_err(|e| SandboxError::Fork(e.to_string()))? {
        ForkResult::Child => {
            drop(parent_sock);
            child::child_main(child_sock, config);
        }
        ForkResult::Parent { child } => {
            drop(child_sock);

            let child_pid = child;

            crate::ipc::send_handshake(&mut parent_sock, "maki-server")?;
            let child_name = crate::ipc::recv_handshake(&mut parent_sock)?;
            if child_name != "maki-child" {
                return Err(SandboxError::Ipc(format!(
                    "unexpected child handshake: got '{child_name}', expected 'maki-child'"
                )));
            }

            crate::ipc::recv_sync(&mut parent_sock, SYNC_READY)?;

            crate::namespace::write_uid_map(child_pid, getuid().as_raw(), getgid().as_raw())?;

            crate::ipc::send_sync(&mut parent_sock, SYNC_GO)?;

            debug!(child_pid = %child_pid.as_raw(), "sandbox: child spawned");
            Ok((child_pid, parent_sock))
        }
    }
}

/// Wait for the child process to exit and collect its status.
pub fn wait_child(pid: Pid) -> Result<(), SandboxError> {
    match waitpid(pid, None) {
        Ok(WaitStatus::Exited(_, 0)) => Ok(()),
        Ok(WaitStatus::Exited(_, code)) => {
            Err(SandboxError::Ipc(format!("child exited with code {code}")))
        }
        Ok(WaitStatus::Signaled(_, sig, _)) => {
            Err(SandboxError::Ipc(format!("child killed by signal {sig}")))
        }
        Ok(status) => Err(SandboxError::Ipc(format!(
            "unexpected wait status: {status:?}"
        ))),
        Err(e) => Err(SandboxError::Ipc(format!("waitpid: {e}"))),
    }
}

/// Handler for trusted tool calls forwarded by the child during a
/// [`Sandbox::run_code`](crate::Sandbox::run_code) call.
pub type ToolHandler =
    dyn Fn(&str, Vec<Value>, Vec<(String, Value)>) -> Result<String, String> + Send + Sync;

/// Result of a code run inside the sandbox.
#[derive(Debug)]
pub struct ChildIoResult {
    pub output: Option<Value>,
    pub stdout: String,
    pub error: Option<String>,
}

/// A response to a parent-originated sandbox request, matched by call id.
#[derive(Debug)]
pub enum SandboxResponse {
    Run(ChildIoResult),
    Tool(ToolResultPayload),
    Ls(Vec<DirEntry>),
    Pwd(String),
    Cd,
    Exec((String, bool)),
}

pub type PendingMap = HashMap<u32, Sender<Result<SandboxResponse, String>>>;
pub type StdoutBufs = HashMap<u32, String>;

/// Parent-side IO handler for the sandbox child process.
///
/// Runs in a dedicated thread that owns the IPC socket: it sends queued
/// [`ParentMsg`] requests and routes every [`ChildMsg`] back to the pending
/// waiter for its call id. Tool calls forwarded by the child (trusted tools)
/// are answered by the handler of the active
/// [`run_code`](crate::Sandbox::run_code) call.
struct ParentIo {
    sock: UnixStream,
    inbound: Receiver<ParentMsg>,
    handler: Arc<Mutex<Option<Arc<ToolHandler>>>>,
    pending: Arc<Mutex<PendingMap>>,
    stdout_bufs: Arc<Mutex<StdoutBufs>>,
}

/// Spawn the parent-side IO thread for a sandbox child socket.
pub(crate) fn parent_io_thread(
    sock: UnixStream,
    inbound: Receiver<ParentMsg>,
    handler: Arc<Mutex<Option<Arc<ToolHandler>>>>,
    pending: Arc<Mutex<PendingMap>>,
    stdout_bufs: Arc<Mutex<StdoutBufs>>,
) -> Result<std::thread::JoinHandle<()>, SandboxError> {
    let mut io = ParentIo {
        sock,
        inbound,
        handler,
        pending,
        stdout_bufs,
    };
    std::thread::Builder::new()
        .name("sandbox-parent-io".into())
        .spawn(move || io.run())
        .map_err(|e| SandboxError::Ipc(format!("spawn sandbox-parent-io thread: {e}")))
}

impl ParentIo {
    fn run(&mut self) {
        loop {
            if let Err(message) = self.drain_inbound() {
                self.fail_all(&message);
                return;
            }

            let ready = {
                let mut pollfds = [PollFd::new(self.sock.as_fd(), PollFlags::POLLIN)];
                match poll(&mut pollfds, PollTimeout::from(IO_POLL_TIMEOUT_MS)) {
                    Ok(0) => PollFlags::empty(),
                    Ok(_) => pollfds[0].revents().unwrap_or(PollFlags::empty()),
                    Err(e) => {
                        error!("sandbox-parent-io: poll error: {e}");
                        self.fail_all(&format!("sandbox ipc poll failed: {e}"));
                        return;
                    }
                }
            };
            if !ready.contains(PollFlags::POLLIN) {
                continue;
            }

            match ipc::recv_child_msg(&mut self.sock) {
                Ok(msg) => {
                    if !self.route(msg) {
                        return;
                    }
                }
                Err(e) => {
                    warn!("sandbox parent io: recv error: {e}");
                    self.fail_all(&format!("child closed the IPC socket: {e}"));
                    return;
                }
            }
        }
    }

    /// Send queued requests to the child. Err carries the message used to
    /// fail every pending waiter.
    fn drain_inbound(&mut self) -> Result<(), String> {
        loop {
            match self.inbound.try_recv() {
                Ok(msg) => {
                    if let Err(e) = ipc::send_parent_msg(&mut self.sock, &msg) {
                        warn!("sandbox parent io: send failed: {e}");
                        return Err(format!("sandbox ipc socket write failed: {e}"));
                    }
                }
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => {
                    debug!("sandbox parent io: inbound queue closed, shutting down");
                    self.fail_all(SHUTDOWN_MSG);
                    return Err(SHUTDOWN_MSG.to_string());
                }
            }
        }
    }

    /// Route one child message. Returns false to stop the IO thread.
    fn route(&mut self, msg: ChildMsg) -> bool {
        match msg {
            ChildMsg::ToolCall {
                call_id,
                name,
                args,
                kwargs,
            } => {
                let handler = self.current_handler();
                let (output, error) = match handler {
                    Some(handler) => match handler(&name, args, kwargs) {
                        Ok(output) => (Some(output), None),
                        Err(error) => (None, Some(error)),
                    },
                    None => (None, Some("no sandbox run is active".into())),
                };
                let sent = ipc::send_parent_msg(
                    &mut self.sock,
                    &ParentMsg::ToolResult {
                        call_id,
                        result: ToolResultPayload { output, error },
                    },
                );
                match sent {
                    Ok(()) => true,
                    Err(e) => {
                        warn!(
                            "sandbox parent io: send tool result failed (call_id={call_id}): {e}"
                        );
                        false
                    }
                }
            }
            ChildMsg::Stdout { call_id, text } => {
                if let Ok(mut bufs) = self.stdout_bufs.lock() {
                    bufs.entry(call_id).or_default().push_str(&text);
                }
                true
            }
            ChildMsg::Done {
                call_id,
                output,
                stdout,
                error,
            } => {
                let streamed = self
                    .stdout_bufs
                    .lock()
                    .ok()
                    .and_then(|mut bufs| bufs.remove(&call_id))
                    .unwrap_or_default();
                let stdout = if stdout.is_empty() { streamed } else { stdout };
                let response = SandboxResponse::Run(ChildIoResult {
                    output,
                    stdout,
                    error: error.clone(),
                });
                if call_id == NO_CALL_ID {
                    // Orphan Done without a call id means the child died during
                    // setup; no further IPC is possible.
                    warn!(error = ?error, "sandbox parent io: child failed fatally");
                    self.fail_all(&error.unwrap_or_else(|| "sandbox child exited".into()));
                    return false;
                }
                self.deliver(call_id, Ok(response));
                true
            }
            ChildMsg::ToolResult { call_id, result } => {
                self.deliver(call_id, Ok(SandboxResponse::Tool(result)));
                true
            }
            ChildMsg::LsResult { call_id, entries } => {
                self.deliver(call_id, Ok(SandboxResponse::Ls(entries)));
                true
            }
            ChildMsg::PwdResult { call_id, path } => {
                self.deliver(call_id, Ok(SandboxResponse::Pwd(path)));
                true
            }
            ChildMsg::CdResult { call_id } => {
                self.deliver(call_id, Ok(SandboxResponse::Cd));
                true
            }
            ChildMsg::ExecResult {
                call_id,
                output,
                is_error,
            } => {
                self.deliver(call_id, Ok(SandboxResponse::Exec((output, is_error))));
                true
            }
        }
    }

    fn current_handler(&self) -> Option<Arc<ToolHandler>> {
        match self.handler.lock() {
            Ok(guard) => guard.clone(),
            Err(e) => {
                warn!("sandbox parent io: handler mutex poisoned: {e}");
                None
            }
        }
    }

    /// Wake the waiter registered for `call_id`, if any. Stale responses to
    /// calls that already timed out are dropped without disturbing the loop.
    fn deliver(&self, call_id: u32, response: Result<SandboxResponse, String>) {
        match self.pending.lock() {
            Ok(mut pending) => match pending.remove(&call_id) {
                Some(tx) => {
                    if tx.send(response).is_err() {
                        debug!(call_id, "sandbox parent io: waiter gone");
                    }
                }
                None => debug!(call_id, "sandbox parent io: no pending waiter"),
            },
            Err(e) => warn!("sandbox parent io: pending mutex poisoned: {e}"),
        }
    }

    fn fail_all(&self, message: &str) {
        if let Ok(mut pending) = self.pending.lock() {
            for (call_id, tx) in pending.drain() {
                debug!(call_id, "sandbox parent io: failing pending waiter");
                let _ = tx.send(Err(message.to_string()));
            }
        }
    }
}
