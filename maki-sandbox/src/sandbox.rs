use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use nix::unistd::Pid;
use tracing::{debug, warn};

use crate::error::SandboxError;
use crate::ipc::{DirEntry, FsOp, FsReply, FsResult, ParentMsg};
use crate::lock_or_poisoned;
use crate::namespace::NamespaceConfig;
use crate::{PendingMap, SandboxResponse};

/// Max wait for long-running requests (fs ops, execs).
const RUN_TIMEOUT: Duration = Duration::from_mins(5);
/// Max wait for quick queries (ls, pwd, cd).
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Shared handle to a persistent sandboxed child process.
///
/// Consumers hold `Arc<Sandbox>` and call methods on it. A dedicated IO
/// thread owns the IPC socket and routes responses by call id, so calls may
/// run concurrently (each gets its own call id and waiter). When the
/// configuration changes, call [`reinit`](Sandbox::reinit) to tear down the
/// old child and spawn a new one.
pub struct Sandbox {
    inner: Mutex<Option<Arc<SandboxInner>>>,
}

struct SandboxInner {
    pid: Pid,
    tx: Sender<ParentMsg>,
    next_id: Arc<AtomicU32>,
    pending: Arc<Mutex<PendingMap>>,
    io_handle: Option<JoinHandle<()>>,
    /// Current namespace config, kept for host↔sandbox path translation.
    config: NamespaceConfig,
}

impl Sandbox {
    /// Create a new sandbox, spawning a persistent child process.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if the child cannot be spawned or its IO
    /// thread cannot be started.
    pub fn new(config: NamespaceConfig) -> Result<Arc<Self>, SandboxError> {
        let sandbox = Arc::new(Self {
            inner: Mutex::new(None),
        });
        let inner = Self::spawn_inner(config)?;
        *lock_or_poisoned(&sandbox.inner)? = Some(inner);
        Ok(sandbox)
    }

    fn spawn_inner(config: NamespaceConfig) -> Result<Arc<SandboxInner>, SandboxError> {
        let (pid, sock) = crate::spawn_child(config.clone())?;
        debug!(pid = %pid.as_raw(), "sandbox: child spawned");
        let (tx, rx) = mpsc::channel::<ParentMsg>();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let io_handle = crate::parent_io_thread(sock, rx, pending.clone())?;
        Ok(Arc::new(SandboxInner {
            pid,
            tx,
            next_id: Arc::new(AtomicU32::new(1)),
            pending,
            io_handle: Some(io_handle),
            config,
        }))
    }

    fn inner(&self) -> Result<Arc<SandboxInner>, SandboxError> {
        lock_or_poisoned(&self.inner)?
            .clone()
            .ok_or_else(|| SandboxError::Ipc("sandbox not initialized (call reinit first)".into()))
    }

    /// The namespace configuration of the currently running child.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if the sandbox mutex is poisoned or no
    /// child is running.
    pub fn config(&self) -> Result<NamespaceConfig, SandboxError> {
        let config = lock_or_poisoned(&self.inner)?
            .as_ref()
            .ok_or_else(|| SandboxError::Ipc("sandbox not initialized".into()))?
            .config
            .clone();
        Ok(config)
    }

    /// Tear down the current child and spawn a new one with the given config.
    ///
    /// The old child is sent [`Exit`](crate::ipc::ParentMsg::Exit) and waited
    /// on before the new child is started.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if the sandbox mutex is poisoned or the new
    /// child cannot be spawned.
    pub fn reinit(&self, config: NamespaceConfig) -> Result<(), SandboxError> {
        let new = Self::spawn_inner(config)?;
        let old = lock_or_poisoned(&self.inner)?.replace(new);
        drop(old);
        debug!("sandbox: reinit complete");
        Ok(())
    }

    /// The child's PID, if a child is running.
    pub fn pid(&self) -> Option<Pid> {
        match lock_or_poisoned(&self.inner) {
            Ok(inner) => inner.as_ref().map(|c| c.pid),
            Err(e) => {
                warn!("sandbox: pid() failed: {e}");
                None
            }
        }
    }

    /// Wait for the child process to exit.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if no child is running or the child exited
    /// unsuccessfully.
    pub fn wait(&self) -> Result<(), SandboxError> {
        let inner = self.inner()?;
        crate::wait_child(inner.pid)
    }

    /// Send an exit signal to the child.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if no child is running or the IO thread has
    /// disconnected.
    pub fn exit(&self) -> Result<(), SandboxError> {
        let inner = self.inner()?;
        inner
            .tx
            .send(ParentMsg::Exit)
            .map_err(|_| SandboxError::Ipc("io thread disconnected".into()))
    }

    /// List directory entries in the sandbox.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if no child is running or the query times out.
    pub fn ls(&self, path: &str) -> Result<Vec<DirEntry>, SandboxError> {
        let inner = self.inner()?;
        let call_id = inner.next_id.fetch_add(1, Ordering::SeqCst);
        let response = self.wait_for(
            call_id,
            ParentMsg::Ls {
                call_id,
                path: path.to_owned(),
            },
            QUERY_TIMEOUT,
        )?;
        match response {
            SandboxResponse::Ls(entries) => Ok(entries),
            other => Err(SandboxError::Ipc(format!(
                "expected LsResult, got {other:?}"
            ))),
        }
    }

    /// Query the sandbox's current working directory.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if no child is running or the query times out.
    pub fn pwd(&self) -> Result<String, SandboxError> {
        let inner = self.inner()?;
        let call_id = inner.next_id.fetch_add(1, Ordering::SeqCst);
        let response = self.wait_for(call_id, ParentMsg::Pwd { call_id }, QUERY_TIMEOUT)?;
        match response {
            SandboxResponse::Pwd(path) => Ok(path),
            other => Err(SandboxError::Ipc(format!(
                "expected PwdResult, got {other:?}"
            ))),
        }
    }

    /// Change the sandbox's working directory.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if no child is running, the query times out,
    /// or the directory does not exist inside the sandbox.
    pub fn cd(&self, path: &str) -> Result<(), SandboxError> {
        let inner = self.inner()?;
        let call_id = inner.next_id.fetch_add(1, Ordering::SeqCst);
        let response = self.wait_for(
            call_id,
            ParentMsg::Cd {
                call_id,
                path: path.to_owned(),
            },
            QUERY_TIMEOUT,
        )?;
        match response {
            SandboxResponse::Cd => Ok(()),
            SandboxResponse::Exec((output, exit_code)) => Err(SandboxError::Ipc(format!(
                "cd failed ({exit_code}): {output}"
            ))),
            other => Err(SandboxError::Ipc(format!(
                "expected CdResult, got {other:?}"
            ))),
        }
    }

    /// Execute a shell command in the sandbox. Returns `(output, exit_code)`.
    ///
    /// `workdir` must already be a sandbox-side path; `timeout_secs` kills
    /// the command after the deadline (reporting exit code 124).
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if no child is running, the exec times out,
    /// or the child reports an I/O failure.
    pub fn exec(
        &self,
        command: &str,
        workdir: Option<&str>,
        timeout_secs: Option<u64>,
    ) -> Result<(String, i32), SandboxError> {
        let inner = self.inner()?;
        let call_id = inner.next_id.fetch_add(1, Ordering::SeqCst);
        let response = self.wait_for(
            call_id,
            ParentMsg::Exec {
                call_id,
                command: command.to_owned(),
                workdir: workdir.map(str::to_owned),
                timeout_secs,
            },
            RUN_TIMEOUT,
        )?;
        match response {
            SandboxResponse::Exec(result) => Ok(result),
            other => Err(SandboxError::Ipc(format!(
                "expected ExecResult, got {other:?}"
            ))),
        }
    }

    /// Execute one filesystem operation inside the namespace.
    ///
    /// `op` paths must already be sandbox-side paths; the caller is
    /// responsible for translating them from the host view and back.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if no child is running, the call times out,
    /// the operation failed, or the reply fails to decode.
    pub fn fs(&self, op: FsOp) -> Result<FsReply, SandboxError> {
        let inner = self.inner()?;
        let call_id = inner.next_id.fetch_add(1, Ordering::SeqCst);
        let response = self.wait_for(call_id, ParentMsg::Fs { call_id, op }, RUN_TIMEOUT)?;
        match response {
            SandboxResponse::Fs(FsResult::Ok(reply)) => Ok(reply),
            SandboxResponse::Fs(FsResult::Err(message)) => Err(SandboxError::Ipc(message)),
            other => Err(SandboxError::Ipc(format!(
                "expected FsResult, got {other:?}"
            ))),
        }
    }

    /// Register a waiter for `call_id`, send `msg`, and block for the
    /// matching response (or the timeout).
    fn wait_for(
        &self,
        call_id: u32,
        msg: ParentMsg,
        timeout: Duration,
    ) -> Result<SandboxResponse, SandboxError> {
        let inner = self.inner()?;
        let (tx, rx) = mpsc::channel::<Result<SandboxResponse, String>>();
        lock_or_poisoned(&inner.pending)?.insert(call_id, tx);
        if inner.tx.send(msg).is_err() {
            let _ = lock_or_poisoned(&inner.pending)?.remove(&call_id);
            return Err(SandboxError::Ipc("io thread disconnected".into()));
        }
        match rx.recv_timeout(timeout) {
            Ok(Ok(response)) => {
                let _ = lock_or_poisoned(&inner.pending)?.remove(&call_id);
                Ok(response)
            }
            Ok(Err(message)) => {
                let _ = lock_or_poisoned(&inner.pending)?.remove(&call_id);
                Err(SandboxError::Ipc(message))
            }
            Err(RecvTimeoutError::Timeout) => {
                let _ = lock_or_poisoned(&inner.pending)?.remove(&call_id);
                Err(SandboxError::Ipc(format!(
                    "sandbox call timed out after {timeout:?}"
                )))
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = lock_or_poisoned(&inner.pending)?.remove(&call_id);
                Err(SandboxError::Ipc("sandbox io thread stopped".into()))
            }
        }
    }
}

impl Drop for SandboxInner {
    fn drop(&mut self) {
        let _ = self.tx.send(ParentMsg::Exit);
        let _ = crate::wait_child(self.pid);
        crate::namespace::cleanup_staging(self.pid);
        if let Some(handle) = self.io_handle.take()
            && handle.join().is_err()
        {
            warn!("sandbox: parent io thread panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::write;
    use std::ops::Deref;
    use std::path::Path;

    use super::*;
    use crate::namespace::NamespaceConfig;

    const SKIP_NO_NS: &str =
        "sandbox tests require user namespace (CLONE_NEWUSER) and mount namespace support";

    /// A live sandbox plus the temp workspace it was built over.
    ///
    /// The `TempDir` has to outlive the sandbox: the child bind-mounts that
    /// path, so letting it drop first leaves the mount pointing at a directory
    /// that no longer exists and every fs call with it.
    struct SandboxFixture {
        sandbox: Arc<Sandbox>,
        _workspace: tempfile::TempDir,
    }

    impl Deref for SandboxFixture {
        type Target = Sandbox;

        fn deref(&self) -> &Sandbox {
            &self.sandbox
        }
    }

    fn config_for(workspace: &Path) -> NamespaceConfig {
        NamespaceConfig::new(
            vec![],
            vec![],
            workspace.to_path_buf(),
            "test".into(),
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        )
    }

    /// `None` means this host cannot isolate at all, which the tests skip.
    /// Every other failure is a real defect, so it panics instead of turning
    /// the suite green on a broken sandbox.
    fn try_sandbox_with(setup: impl FnOnce(&Path)) -> Option<SandboxFixture> {
        let workspace = tempfile::TempDir::new().unwrap();
        setup(workspace.path());
        match Sandbox::new(config_for(workspace.path())) {
            Ok(sandbox) => Some(SandboxFixture {
                sandbox,
                _workspace: workspace,
            }),
            Err(e) if e.is_isolation_unavailable() => None,
            Err(e) => panic!("Sandbox::new failed for a reason that is not host support: {e}"),
        }
    }

    fn try_sandbox() -> Option<SandboxFixture> {
        try_sandbox_with(|_| {})
    }

    #[test]
    fn sandbox_new_and_drop() {
        let Some(sandbox) = try_sandbox() else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        assert!(sandbox.pid().is_some());
        drop(sandbox);
    }

    #[test]
    fn sandbox_reinit_spawns_new_child() {
        let Some(sandbox) = try_sandbox() else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        let pid1 = sandbox.pid();
        let next_workspace = tempfile::TempDir::new().unwrap();
        sandbox
            .reinit(config_for(next_workspace.path()))
            .expect("reinit should succeed");
        let pid2 = sandbox.pid();
        assert!(pid2.is_some());
        if let (Some(p1), Some(p2)) = (pid1, pid2) {
            assert_ne!(p1.as_raw(), p2.as_raw(), "reinit should spawn a new PID");
        }
    }

    #[test]
    fn sandbox_pwd_returns_workspace() {
        let Some(sandbox) = try_sandbox() else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        let Ok(pwd) = sandbox.pwd() else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        assert!(!pwd.is_empty());
    }

    #[test]
    fn sandbox_exec_echo() {
        let Some(sandbox) = try_sandbox() else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        let Ok((output, exit_code)) = sandbox.exec("echo hello", None, None) else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        assert_eq!(exit_code, 0);
        assert_eq!(output.trim(), "hello");
    }

    #[test]
    fn sandbox_exec_timeout_reports_exit_code() {
        let Some(sandbox) = try_sandbox() else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        let Ok((_, exit_code)) = sandbox.exec("sleep 5", None, Some(1)) else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        assert_eq!(exit_code, 124);
    }

    #[test]
    fn sandbox_ls_lists_entries() {
        let Some(sandbox) = try_sandbox_with(|dir| write(dir.join("file.txt"), b"data").unwrap())
        else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        let Ok(entries) = sandbox.ls(".") else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"file.txt"));
    }

    #[test]
    fn sandbox_exit_succeeds() {
        let Some(sandbox) = try_sandbox() else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        let _ = sandbox.exit();
    }

    #[test]
    fn sandbox_call_without_init_returns_error() {
        let Some(sandbox) = try_sandbox() else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        drop(sandbox.inner.lock().unwrap().take());
        let err = sandbox.pwd().unwrap_err();
        assert!(err.to_string().contains("not initialized"));
    }
}
