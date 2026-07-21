//! Injection seam for the child's execution engine.
//!
//! `maki-sandbox` owns namespaces and IPC but not what runs inside the
//! child. The embedding binary registers a [`ChildWorkload`] at startup;
//! after fork (and after a possible `--sandbox-inner` re-exec, which keeps
//! the same binary) the child builds its [`ChildSession`] from the registry.

use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock, mpsc};
use std::time::Duration;

use serde_json::Value;

use crate::ChildIoResult;
use crate::child::{IoCommand, RemoteDispatch, sandbox_exec};
use crate::error::SandboxError;
use crate::ipc::ChildMsg;

/// Backstop for a trusted-tool reply that never arrives (dead IO thread,
/// lost message). Matches the parent-side run timeout.
const FORWARD_TIMEOUT: Duration = Duration::from_mins(5);

static CHILD_WORKLOAD: OnceLock<Arc<dyn ChildWorkload>> = OnceLock::new();

/// Register the workload executed inside spawned children.
///
/// Must be called before any child is forked or re-execed; returns false if
/// one was already registered.
pub fn register_child_workload(workload: Arc<dyn ChildWorkload>) -> bool {
    CHILD_WORKLOAD.set(workload).is_ok()
}

/// The registered workload, if any.
pub fn child_workload() -> Option<Arc<dyn ChildWorkload>> {
    CHILD_WORKLOAD.get().cloned()
}

/// One code run requested by the parent.
pub struct RunSpec {
    pub call_id: u32,
    pub code: String,
    pub timeout_secs: u64,
    pub max_memory: usize,
    /// Serialized config applied by the session before running.
    pub config: String,
}

/// Primitives a child-side session may use to talk back to the parent.
///
/// Backed by the child's single IO thread; every method queues a message or
/// performs a request/reply round-trip over IPC.
#[derive(Clone)]
pub struct ChildCtx {
    dispatch: Arc<RemoteDispatch>,
    outgoing: mpsc::Sender<IoCommand>,
}

impl ChildCtx {
    pub(crate) fn new(dispatch: Arc<RemoteDispatch>, outgoing: mpsc::Sender<IoCommand>) -> Self {
        Self { dispatch, outgoing }
    }

    /// Stream a chunk of run output to the parent.
    pub fn stream_stdout(&self, call_id: u32, text: String) {
        let _ = self
            .outgoing
            .send(IoCommand::SendChild(ChildMsg::Stdout { call_id, text }));
    }

    /// Run a trusted tool in the parent process (network, UI, agent state).
    pub fn forward_trusted(
        &self,
        name: &str,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<String, String> {
        let dispatch = &self.dispatch;
        let call_id = dispatch.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        {
            let mut pending = dispatch.lock_pending().map_err(|e| e.to_string())?;
            pending.insert(call_id, tx);
            if dispatch
                .outgoing
                .send(IoCommand::SendChild(ChildMsg::ToolCall {
                    call_id,
                    name: name.to_string(),
                    args,
                    kwargs,
                }))
                .is_err()
            {
                pending.remove(&call_id);
                return Err("io thread disconnected".into());
            }
        }
        // Nothing may hold the pending lock across the wait: the reply is
        // routed by the IO thread, which needs this map to deliver it.
        let reply = rx.recv_timeout(FORWARD_TIMEOUT);
        if let Ok(mut pending) = dispatch.lock_pending() {
            pending.remove(&call_id);
        }
        match reply {
            Ok(Ok(payload)) => match payload.error {
                Some(error) => Err(error),
                None => Ok(payload.output.unwrap_or_default()),
            },
            Ok(Err(err)) => Err(err),
            Err(_) => Err("trusted tool forward timed out or io thread stopped".into()),
        }
    }

    /// Execute a shell command inside the isolated filesystem.
    pub fn exec(
        &self,
        command: &str,
        workdir: Option<&str>,
    ) -> Result<(String, bool), SandboxError> {
        sandbox_exec(command, workdir)
    }
}

/// Factory for the child-side execution engine.
pub trait ChildWorkload: Send + Sync {
    /// Build the session. Called once per child process, before any IPC
    /// traffic is served; an Err fails the child with a fatal report.
    fn init(&self, ctx: ChildCtx) -> Result<Box<dyn ChildSession>, String>;
}

/// Stateful execution engine living on the child's worker thread.
pub trait ChildSession: Send {
    /// Run a piece of code; stream stdout chunks via [`ChildCtx`].
    fn run_code(&mut self, spec: RunSpec) -> ChildIoResult;

    /// Answer a parent-initiated tool call meant for local execution.
    /// Returns the textual tool output, or Err for a tool-level error.
    fn handle_tool_call(
        &mut self,
        name: &str,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<String, String>;
}
