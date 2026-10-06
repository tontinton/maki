use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use maki_fs::{JobSink, JobStream};
use nix::unistd::Pid;
use tracing::{debug, warn};

use crate::error::SandboxError;
use crate::ipc::{DirEntry, FsOp, FsReply, FsResult, ParentMsg, RunId};
use crate::lock_or_poisoned;
use crate::namespace::NamespaceConfig;
use crate::{PendingMap, SandboxResponse, StreamMap};

/// Max wait for long-running requests (fs ops, execs).
const RUN_TIMEOUT: Duration = Duration::from_mins(5);
/// Max wait for quick queries (ls, pwd, cd).
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Shared handle to a persistent sandboxed child process.
///
/// Consumers hold `Arc<Sandbox>` and call methods on it. A dedicated IO
/// thread owns the IPC socket and routes responses by [`RunId`], so calls may
/// run concurrently (each gets its own run id and waiter). When the
/// configuration changes, call [`reinit`](Sandbox::reinit) to tear down the
/// old child and spawn a new one.
pub struct Sandbox {
    inner: Mutex<Option<Arc<SandboxInner>>>,
    /// Ids for the requests this sandbox is serving. They live on the handle
    /// rather than the child, so a reply can never reach a run of a child that
    /// has already been replaced.
    runs: AtomicU64,
}

struct SandboxInner {
    pid: Pid,
    tx: Sender<ParentMsg>,
    pending: Arc<Mutex<PendingMap>>,
    /// Sinks for the runs in flight, which the IO thread feeds from the
    /// child's [`ExecLine`](crate::ipc::ChildMsg::ExecLine) messages.
    streams: Arc<Mutex<StreamMap>>,
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
            runs: AtomicU64::new(1),
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
        let streams = Arc::new(Mutex::new(HashMap::new()));
        let io_handle = crate::parent_io_thread(sock, rx, pending.clone(), streams.clone())?;
        Ok(Arc::new(SandboxInner {
            pid,
            tx,
            pending,
            streams,
            io_handle: Some(io_handle),
            config,
        }))
    }

    /// The id for a request the caller is about to make. Ids are unique for
    /// the life of this handle, so a caller can label its own bookkeeping with
    /// one and a reply still finds the run it belongs to.
    pub fn next_run(&self) -> RunId {
        RunId(self.runs.fetch_add(1, Ordering::SeqCst))
    }

    /// A handle with no child behind it, for tests that need real run ids
    /// without a child to serve them.
    #[cfg(any(test, feature = "testing"))]
    pub fn without_child() -> Self {
        Self {
            inner: Mutex::new(None),
            runs: AtomicU64::new(1),
        }
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
        let run = self.next_run();
        wait_for(
            &inner,
            run,
            ParentMsg::Ls {
                run,
                path: path.to_owned(),
            },
            QUERY_TIMEOUT,
            |response| match response {
                SandboxResponse::Ls(entries) => Ok(entries),
                other => Err(unexpected("LsResult", other)),
            },
        )
    }

    /// Query the sandbox's current working directory.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if no child is running or the query times out.
    pub fn pwd(&self) -> Result<String, SandboxError> {
        let inner = self.inner()?;
        let run = self.next_run();
        wait_for(
            &inner,
            run,
            ParentMsg::Pwd { run },
            QUERY_TIMEOUT,
            |response| match response {
                SandboxResponse::Pwd(path) => Ok(path),
                other => Err(unexpected("PwdResult", other)),
            },
        )
    }

    /// Change the sandbox's working directory.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if no child is running, the query times out,
    /// or the directory does not exist inside the sandbox.
    pub fn cd(&self, path: &str) -> Result<(), SandboxError> {
        let inner = self.inner()?;
        let run = self.next_run();
        wait_for(
            &inner,
            run,
            ParentMsg::Cd {
                run,
                path: path.to_owned(),
            },
            QUERY_TIMEOUT,
            |response| match response {
                SandboxResponse::Cd => Ok(()),
                other => Err(unexpected("CdResult", other)),
            },
        )
    }

    /// Execute a shell command in the sandbox, streaming its output.
    ///
    /// `run` names the run this call is, and both the sink's lines and the
    /// answer come back tagged with it, so the caller can keep its own record
    /// of the run alongside the output. `workdir` must already be a sandbox-side
    /// path. Every output line goes to {sink} as the command writes it, and the
    /// call returns once the command is gone, with its exit code. `timeout_secs`
    /// kills it after the deadline (reporting exit code 124).
    ///
    /// Reporting the exit to the sink is the caller's business: only a caller
    /// that owns the whole run promises the sink one, so [`exec`](Self::exec)
    /// and `maki_fs::FsBackend::exec_job` are the shapes that do.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if no child is running, the exec times out, or
    /// the child reports an I/O failure.
    pub fn exec_streaming(
        &self,
        run: RunId,
        command: &str,
        workdir: Option<&str>,
        timeout_secs: Option<u64>,
        sink: Arc<dyn JobSink>,
    ) -> Result<i32, SandboxError> {
        // One child answers this call, so its map and its IO thread have to be
        // the same one: a reinit in the middle would send the lines to a
        // stream map this sink was never put in.
        let inner = self.inner()?;
        lock_or_poisoned(&inner.streams)?.insert(run, sink);
        let result = wait_for(
            &inner,
            run,
            ParentMsg::Exec {
                run,
                command: command.to_owned(),
                workdir: workdir.map(str::to_owned),
                timeout_secs,
            },
            RUN_TIMEOUT,
            |response| match response {
                SandboxResponse::Exec(exit_code) => Ok(exit_code),
                other => Err(unexpected("ExecResult", other)),
            },
        );
        // The IO thread drops the entry with the exit it routes, so this is
        // only the path where no exit ever arrived.
        drop(lock_or_poisoned(&inner.streams).map(|mut s| s.remove(&run)));
        result
    }

    /// Execute a shell command in the sandbox, returning `(output, exit_code)`.
    ///
    /// The one-shot shape of [`exec_streaming`](Self::exec_streaming), for the
    /// callers that only want the run's output at the end.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if no child is running, the exec times out, or
    /// the child reports an I/O failure.
    pub fn exec(
        &self,
        command: &str,
        workdir: Option<&str>,
        timeout_secs: Option<u64>,
    ) -> Result<(String, i32), SandboxError> {
        let output = Arc::new(Mutex::new(String::new()));
        let code = self.exec_streaming(
            self.next_run(),
            command,
            workdir,
            timeout_secs,
            Arc::new(Collected(Arc::clone(&output))),
        )?;
        let output = match lock_or_poisoned(&output) {
            Ok(output) => output,
            Err(e) => return Err(SandboxError::MutexPoisoned(e.to_string())),
        };
        Ok((output.clone(), code))
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
        let run = self.next_run();
        wait_for(
            &inner,
            run,
            ParentMsg::Fs { run, op },
            RUN_TIMEOUT,
            |response| match response {
                SandboxResponse::Fs(FsResult::Ok(reply)) => Ok(reply),
                SandboxResponse::Fs(FsResult::Err(message)) => Err(SandboxError::Ipc(message)),
                other => Err(unexpected("FsResult", other)),
            },
        )
    }
}

/// Register a waiter for `run` on {inner}, send {msg}, and block for the
/// response (or the timeout), turning it into the caller's answer.
fn wait_for<T>(
    inner: &SandboxInner,
    run: RunId,
    msg: ParentMsg,
    timeout: Duration,
    from: impl FnOnce(SandboxResponse) -> Result<T, SandboxError>,
) -> Result<T, SandboxError> {
    let (tx, rx) = mpsc::channel::<Result<SandboxResponse, String>>();
    lock_or_poisoned(&inner.pending)?.insert(run, tx);
    if inner.tx.send(msg).is_err() {
        let _ = lock_or_poisoned(&inner.pending)?.remove(&run);
        return Err(SandboxError::Ipc("io thread disconnected".into()));
    }
    let received = match rx.recv_timeout(timeout) {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(message)) => Err(SandboxError::Ipc(message)),
        Err(RecvTimeoutError::Timeout) => Err(SandboxError::Ipc(format!(
            "sandbox call timed out after {timeout:?}"
        ))),
        Err(RecvTimeoutError::Disconnected) => {
            Err(SandboxError::Ipc("sandbox io thread stopped".into()))
        }
    };
    let _ = lock_or_poisoned(&inner.pending)?.remove(&run);
    received.and_then(from)
}

/// A sink that keeps a run's output as one string, which is what
/// [`Sandbox::exec`] hands back.
struct Collected(Arc<Mutex<String>>);

impl JobSink for Collected {
    fn line(&self, _: JobStream, line: String) {
        if let Ok(mut output) = lock_or_poisoned(&self.0) {
            output.push_str(&line);
            output.push('\n');
        }
    }
    fn exit(&self, _: i32) {}
}

fn unexpected(expected: &str, got: SandboxResponse) -> SandboxError {
    SandboxError::Ipc(format!("expected {expected}, got {got:?}"))
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
    use std::thread;

    use super::*;
    use crate::child::EXIT_CODE_TIMED_OUT;
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
        assert_eq!(exit_code, EXIT_CODE_TIMED_OUT);
    }

    /// A sink that forwards lines to a channel, so a test can look at them
    /// while the run is still going.
    struct Forwarding(mpsc::Sender<(JobStream, String)>);

    impl JobSink for Forwarding {
        fn line(&self, stream: JobStream, line: String) {
            let _ = self.0.send((stream, line));
        }
        fn exit(&self, _: i32) {}
    }

    /// The line has to reach the sink while the command is still running: that
    /// is the whole point of streaming through the child instead of reporting
    /// one blob when the run returns.
    #[test]
    fn sandbox_exec_streams_a_line_before_the_command_ends() {
        const LATE: &str = "the line only arrived after the command had ended";
        const A_LINE: Duration = Duration::from_secs(10);
        let Some(sandbox) = try_sandbox() else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        let (tx, rx) = mpsc::channel();

        let run = thread::spawn({
            let sandbox = sandbox.sandbox.clone();
            move || {
                sandbox.exec_streaming(
                    sandbox.next_run(),
                    "echo one; sleep 5",
                    None,
                    Some(2),
                    Arc::new(Forwarding(tx)),
                )
            }
        });

        assert_eq!(
            rx.recv_timeout(A_LINE).expect("a line the command wrote"),
            (JobStream::Stdout, "one".to_string()),
            "{LATE}"
        );
        assert_eq!(run.join().unwrap().unwrap(), EXIT_CODE_TIMED_OUT);
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
