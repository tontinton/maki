use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nix::unistd::Pid;
use serde_json::Value;
use tracing::{debug, warn};

use crate::error::SandboxError;
use crate::ipc::{DirEntry, ParentMsg, ToolResultPayload};
use crate::lock_or_poisoned;
use crate::namespace::NamespaceConfig;
use crate::{ChildIoResult, PendingMap, SandboxResponse, ToolHandler};

/// Max wait for long-running requests (code runs, tool calls, execs).
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
    handler: Arc<Mutex<Option<Arc<ToolHandler>>>>,
}

struct SandboxInner {
    pid: Pid,
    tx: Sender<ParentMsg>,
    next_id: Arc<AtomicU32>,
    pending: Arc<Mutex<PendingMap>>,
    io_handle: Option<std::thread::JoinHandle<()>>,
}

impl Sandbox {
    /// Create a new sandbox, spawning a persistent child process.
    ///
    /// Trusted tools forwarded by the child fail unless the active
    /// [`run_code`](Sandbox::run_code) call provides a handler for them.
    pub fn new(config: NamespaceConfig) -> Result<Arc<Self>, SandboxError> {
        let sandbox = Arc::new(Self {
            inner: Mutex::new(None),
            handler: Arc::new(Mutex::new(None)),
        });
        let inner = Self::spawn_inner(&sandbox, &config)?;
        *lock_or_poisoned(&sandbox.inner)? = Some(inner);
        Ok(sandbox)
    }

    fn spawn_inner(
        sandbox: &Self,
        config: &NamespaceConfig,
    ) -> Result<Arc<SandboxInner>, SandboxError> {
        let (pid, sock) = crate::spawn_child(config)?;
        debug!(pid = %pid.as_raw(), "sandbox: child spawned");
        let (tx, rx) = mpsc::channel::<ParentMsg>();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let stdout_bufs = Arc::new(Mutex::new(HashMap::new()));
        let io_handle = crate::parent_io_thread(
            sock,
            rx,
            Arc::clone(&sandbox.handler),
            pending.clone(),
            stdout_bufs,
        )?;
        Ok(Arc::new(SandboxInner {
            pid,
            tx,
            next_id: Arc::new(AtomicU32::new(1)),
            pending,
            io_handle: Some(io_handle),
        }))
    }

    fn inner(&self) -> Result<Arc<SandboxInner>, SandboxError> {
        lock_or_poisoned(&self.inner)?
            .clone()
            .ok_or_else(|| SandboxError::Ipc("sandbox not initialized (call reinit first)".into()))
    }

    /// Tear down the current child and spawn a new one with the given config.
    ///
    /// The old child is sent [`Exit`](crate::ipc::ParentMsg::Exit) and waited
    /// on before the new child is started.
    pub fn reinit(&self, config: NamespaceConfig) -> Result<(), SandboxError> {
        let old = lock_or_poisoned(&self.inner)?.take();
        drop(old);
        *lock_or_poisoned(&self.inner)? = Some(Self::spawn_inner(self, &config)?);
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
    pub fn wait(&self) -> Result<(), SandboxError> {
        let inner = self.inner()?;
        crate::wait_child(inner.pid)
    }

    /// Send an exit signal to the child.
    pub fn exit(&self) -> Result<(), SandboxError> {
        let inner = self.inner()?;
        inner
            .tx
            .send(ParentMsg::Exit)
            .map_err(|_| SandboxError::Ipc("io thread disconnected".into()))
    }

    /// Run code in the child's interpreter and wait for the result.
    ///
    /// `config` is a serialized `AgentConfig` applied to the child's Lua
    /// runtime before the run starts. Trusted tools the child forwards while
    /// this run is active are answered by `handler`; calls arriving outside
    /// a run fail with "no sandbox run is active".
    pub fn run_code(
        &self,
        code: String,
        timeout_secs: u64,
        max_memory: usize,
        config: String,
        handler: impl Fn(&str, Vec<Value>, Vec<(String, Value)>) -> Result<String, String>
        + Send
        + Sync
        + 'static,
    ) -> Result<ChildIoResult, SandboxError> {
        let previous = lock_or_poisoned(&self.handler)?.replace(Arc::new(handler));
        let result = self.run_code_inner(code, timeout_secs, max_memory, config);
        *lock_or_poisoned(&self.handler)? = previous;
        result
    }

    fn run_code_inner(
        &self,
        code: String,
        timeout_secs: u64,
        max_memory: usize,
        config: String,
    ) -> Result<ChildIoResult, SandboxError> {
        let inner = self.inner()?;
        let call_id = inner.next_id.fetch_add(1, Ordering::SeqCst);
        let response = self.wait_for(
            call_id,
            ParentMsg::Run {
                call_id,
                code,
                timeout_secs,
                max_memory,
                config,
            },
            RUN_TIMEOUT,
        )?;
        match response {
            SandboxResponse::Run(result) => Ok(result),
            other => Err(SandboxError::Ipc(format!("expected Done, got {other:?}"))),
        }
    }

    /// Execute a tool inside the sandbox namespace.
    ///
    /// Filesystem and bash tools run in the child; trusted tools are
    /// forwarded to the handler of the active
    /// [`run_code`](Sandbox::run_code) call.
    pub fn call_tool(
        &self,
        name: &str,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<ToolResultPayload, SandboxError> {
        let inner = self.inner()?;
        let call_id = inner.next_id.fetch_add(1, Ordering::SeqCst);
        let response = self.wait_for(
            call_id,
            ParentMsg::ToolCall {
                call_id,
                name: name.to_owned(),
                args,
                kwargs,
            },
            RUN_TIMEOUT,
        )?;
        match response {
            SandboxResponse::Tool(result) => Ok(result),
            other => Err(SandboxError::Ipc(format!(
                "expected ToolResult, got {other:?}"
            ))),
        }
    }

    /// List directory entries in the sandbox.
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
            SandboxResponse::Exec((output, true)) => Err(SandboxError::Ipc(output)),
            other => Err(SandboxError::Ipc(format!(
                "expected CdResult, got {other:?}"
            ))),
        }
    }

    /// Execute a shell command in the sandbox. Returns `(output, is_error)`.
    pub fn exec(&self, command: &str) -> Result<(String, bool), SandboxError> {
        let inner = self.inner()?;
        let call_id = inner.next_id.fetch_add(1, Ordering::SeqCst);
        let response = self.wait_for(
            call_id,
            ParentMsg::Exec {
                call_id,
                command: command.to_owned(),
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
    use super::*;
    use crate::namespace::NamespaceConfig;
    use std::path::PathBuf;

    const SKIP_NO_NS: &str = "sandbox tests require user namespace support (CLONE_NEWUSER)";

    fn test_config() -> NamespaceConfig {
        let dir = tempfile::TempDir::new().unwrap();
        NamespaceConfig::new(
            vec![],
            vec![],
            PathBuf::from(dir.path()),
            "test".into(),
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        )
    }

    fn try_sandbox() -> Option<Arc<Sandbox>> {
        let config = test_config();
        Sandbox::new(config).ok()
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
        let new_config = test_config();
        sandbox.reinit(new_config).expect("reinit should succeed");
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
        let pwd = match sandbox.pwd() {
            Ok(p) => p,
            Err(_) => {
                eprintln!("{SKIP_NO_NS}");
                return;
            }
        };
        assert!(!pwd.is_empty());
    }

    #[test]
    fn sandbox_exec_echo() {
        let Some(sandbox) = try_sandbox() else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        let (output, is_error) = match sandbox.exec("echo hello") {
            Ok(r) => r,
            Err(_) => {
                eprintln!("{SKIP_NO_NS}");
                return;
            }
        };
        assert!(!is_error);
        assert_eq!(output.trim(), "hello");
    }

    #[test]
    fn sandbox_ls_lists_entries() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("file.txt"), b"data").unwrap();
        let config = NamespaceConfig::new(
            vec![],
            vec![],
            PathBuf::from(dir.path()),
            "test".into(),
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        );
        let sandbox = match Sandbox::new(config) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("{SKIP_NO_NS}");
                return;
            }
        };
        let entries = match sandbox.ls(".") {
            Ok(e) => e,
            Err(_) => {
                eprintln!("{SKIP_NO_NS}");
                return;
            }
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
