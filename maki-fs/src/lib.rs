//! Swappable filesystem + exec backend contract.
//!
//! Everything the sandbox child can do — and everything a host tool needs —
//! is expressed as one [`FsBackend`] trait. A backend is either the host
//! filesystem (`HostFs`, in maki-agent) or a sandboxed view (`SandboxFs`, in
//! maki-sandbox). Routing a tool then means "pick a [`FsBackend`]" instead of
//! switching on tool names against an IPC surface.
//!
//! The contract: all paths accepted and returned are **host paths**. A
//! sandboxed backend translates between the host view and its namespace
//! internally, so callers never see sandbox-side paths.
//!
//! [`search`] holds the ripgrep/`ignore`-powered glob and grep walks behind
//! [`FsBackend::glob`]/[`FsBackend::grep`], so both backends search the same
//! way instead of each carrying its own copy.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use serde_json::Value;
use thiserror::Error;

pub mod grep;
pub mod job;
pub mod search;
pub use grep::{GrepFileEntry, GrepLine, GrepMatchGroup, GrepParams};
pub use job::{JobCommand, JobHandle, JobOut, JobRequest, JobSink, JobStream};

/// An opaque filesystem/exec failure; the message is already human-readable
/// and shown to the model verbatim.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{message}")]
pub struct FsError {
    message: String,
}

impl FsError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<&str> for FsError {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for FsError {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<io::Error> for FsError {
    fn from(e: io::Error) -> Self {
        Self::new(e.to_string())
    }
}

/// Filesystem + exec operations, all in the host path space. A backend is
/// held behind an `Arc`, so it outlives whatever thread calls it.
pub trait FsBackend: Send + Sync + 'static {
    /// Read up to `max_bytes`; larger files error out.
    fn read(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, FsError>;
    /// Shared result metadata: `{size, is_file, is_dir, mtime}` or `Null`.
    fn metadata(&self, path: &Path) -> Result<Value, FsError>;
    /// Directory tree as `[[name, filetype], ...]` in depth-first order.
    fn dir(&self, path: &Path, depth: u32) -> Result<Value, FsError>;
    fn exists(&self, path: &Path) -> Result<bool, FsError>;
    fn write(&self, path: &Path, content: &[u8]) -> Result<(), FsError>;
    fn append(&self, path: &Path, content: &[u8]) -> Result<(), FsError>;
    fn atomic_write(&self, path: &Path, content: &[u8]) -> Result<(), FsError>;
    fn remove(&self, path: &Path, recursive: bool) -> Result<(), FsError>;
    fn mkdir(&self, path: &Path, parents: bool) -> Result<(), FsError>;
    /// Absolute paths to matching files, sorted by mtime newest-first when
    /// `sort_mtime` is set.
    fn glob(
        &self,
        root: &Path,
        patterns: &[String],
        gitignore: bool,
        sort_mtime: bool,
        limit: Option<usize>,
    ) -> Result<Vec<PathBuf>, FsError>;
    /// Grep results with host-absolute `entry.path` values.
    fn grep(&self, params: GrepParams) -> Result<Vec<GrepFileEntry>, FsError>;
    /// Run a command, returning `(output, exit_code)`.
    fn exec(
        &self,
        command: &str,
        workdir: Option<&str>,
        timeout_secs: Option<u64>,
    ) -> Result<(String, i32), FsError>;

    /// Start {request} in the background: its sink gets every output line
    /// and, last, the exit code. Returns as soon as the job is running, never
    /// after it finished.
    ///
    /// The default runs the command to completion on a worker thread and
    /// reports the whole output at once, the only shape a backend that
    /// cannot stream can offer. `env` and a redirected stream go with it, and
    /// a command that cannot run at all reports the reason as output and an
    /// exit of 1. A backend that can spawn a process and stream it overrides
    /// this.
    fn exec_job(self: Arc<Self>, request: JobRequest) -> Result<JobHandle, FsError> {
        let JobRequest {
            command,
            workdir,
            sink,
            ..
        } = request;
        let line = command.display();
        let workdir = workdir.map(|dir| dir.to_string_lossy().into_owned());
        let reaped = Arc::new(AtomicBool::new(false));
        let gone = Arc::clone(&reaped);
        let backend = Arc::clone(&self);
        thread::Builder::new()
            .name("job-exec".into())
            .spawn(move || {
                let (output, code) = match backend.exec(&line, workdir.as_deref(), None) {
                    Ok(finished) => finished,
                    Err(e) => {
                        sink.line(JobStream::Stdout, format!("exec failed: {e}"));
                        (String::new(), 1)
                    }
                };
                for line in output.lines() {
                    sink.line(JobStream::Stdout, line.to_string());
                }
                gone.store(true, Ordering::Relaxed);
                sink.exit(code);
            })
            .map_err(|e| FsError::new(e.to_string()))?;
        Ok(JobHandle::spawned(0, reaped))
    }
}
