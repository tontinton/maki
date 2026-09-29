#![cfg(all(feature = "sandbox", target_os = "linux"))]

pub mod child;
pub mod error;
pub mod fs_backend;
pub mod ipc;
pub mod namespace;
pub mod profiles;
pub mod sandbox;

use std::collections::HashMap;
use std::os::unix::io::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, getgid, getuid};
use tracing::{debug, error, warn};

use crate::error::SandboxError;
use crate::ipc::{ChildMsg, DirEntry, FsResult, ParentMsg, SYNC_GO, SYNC_READY};
use crate::namespace::NamespaceConfig;

/// Socket poll timeout in the parent's IO thread, in milliseconds.
const IO_POLL_TIMEOUT_MS: u16 = 100;

const SHUTDOWN_MSG: &str = "sandbox shutting down";

/// Acquire a mutex lock, converting poison to [`SandboxError`].
///
/// # Errors
///
/// Returns [`SandboxError::MutexPoisoned`] if the mutex is poisoned.
pub fn lock_or_poisoned<T>(mutex: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, SandboxError> {
    mutex
        .lock()
        .map_err(|e| SandboxError::MutexPoisoned(e.to_string()))
}

pub use sandbox::Sandbox;

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
/// [`ParentMsg`](crate::ipc::ParentMsg) requests (fs ops, execs, queries)
/// over the socket.
///
/// # Errors
///
/// Returns a [`SandboxError`] if the fork fails, the handshake or sync
/// protocol fails, or the uid/gid map cannot be written.
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

            // The child only acks once its namespaces and mounts are in place.
            // Without this wait a host that refuses `unshare(CLONE_NEWNS)` would
            // hand back a Sandbox whose paths mean nothing, and every fs call
            // through it would fail later and further from the cause.
            match crate::ipc::recv_child_msg(&mut parent_sock)? {
                ChildMsg::Setup { error: None } => {}
                ChildMsg::Setup { error: Some(e) } => return Err(e),
                other => {
                    return Err(SandboxError::Ipc(format!(
                        "unexpected child message during setup: {other:?}"
                    )));
                }
            }

            debug!(child_pid = %child_pid.as_raw(), "sandbox: child spawned");
            Ok((child_pid, parent_sock))
        }
    }
}

/// Wait for the child process to exit and collect its status.
///
/// # Errors
///
/// Returns [`SandboxError::Ipc`] if the child exits with a non-zero code, is
/// killed by a signal, or [`waitpid`] itself fails.
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

/// A response to a parent-originated sandbox request, matched by call id.
#[derive(Debug)]
pub enum SandboxResponse {
    Ls(Vec<DirEntry>),
    Pwd(String),
    Cd,
    Exec((String, i32)),
    Fs(FsResult),
}

pub type PendingMap = HashMap<u32, Sender<Result<SandboxResponse, String>>>;

/// Parent-side IO handler for the sandbox child process.
///
/// Runs in a dedicated thread that owns the IPC socket: it sends queued
/// [`ParentMsg`] requests and routes every [`ChildMsg`] back to the pending
/// waiter for its call id.
struct ParentIo {
    sock: UnixStream,
    inbound: Receiver<ParentMsg>,
    pending: Arc<Mutex<PendingMap>>,
}

/// Spawn the parent-side IO thread for a sandbox child socket.
pub(crate) fn parent_io_thread(
    sock: UnixStream,
    inbound: Receiver<ParentMsg>,
    pending: Arc<Mutex<PendingMap>>,
) -> Result<std::thread::JoinHandle<()>, SandboxError> {
    let mut io = ParentIo {
        sock,
        inbound,
        pending,
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
            // `spawn_child` consumes this before the IO thread exists, so
            // reaching it here means the protocol ran off its rails.
            ChildMsg::Setup { error } => {
                let detail = error.map_or_else(|| "ok".to_owned(), |e| e.to_string());
                warn!(detail, "sandbox parent io: setup message after startup");
                false
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
                exit_code,
            } => {
                self.deliver(call_id, Ok(SandboxResponse::Exec((output, exit_code))));
                true
            }
            ChildMsg::FsResult { call_id, result } => {
                self.deliver(call_id, Ok(SandboxResponse::Fs(result)));
                true
            }
        }
    }

    fn deliver(&self, call_id: u32, response: Result<SandboxResponse, String>) {
        match self.pending.lock() {
            Ok(mut pending) => {
                if let Some(tx) = pending.remove(&call_id) {
                    if tx.send(response).is_err() {
                        debug!(call_id, "sandbox parent io: waiter gone");
                    }
                } else {
                    debug!(call_id, "sandbox parent io: no pending waiter");
                }
            }
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
