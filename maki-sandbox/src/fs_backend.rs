//! [`maki_fs::FsBackend`] implementation over a [`Sandbox`].
//!
//! Translates every host path to its sandbox-side equivalent before sending
//! an [`FsOp`] or exec, and translates glob/grep result paths back. The child
//! sees only sandbox-side absolute paths while callers only ever pass and
//! receive host paths.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use maki_fs::grep::{GrepFileEntry, GrepParams};
use maki_fs::{FsBackend, FsError};
use serde_json::Value;

use crate::ipc::{FsOp, FsReply};
use crate::namespace::NamespaceConfig;
use crate::sandbox::Sandbox;

fn fs_err(e: impl ToString) -> FsError {
    FsError::new(e.to_string())
}

/// Host-path view of the sandboxed filesystem.
pub struct SandboxFs {
    sandbox: Arc<Sandbox>,
}

impl SandboxFs {
    pub fn new(sandbox: Arc<Sandbox>) -> Self {
        Self { sandbox }
    }

    fn config(&self) -> Result<NamespaceConfig, FsError> {
        self.sandbox.config().map_err(fs_err)
    }

    /// Translate a host path to its sandbox-side form used in IPC.
    fn sandbox_path(&self, path: &Path, config: &NamespaceConfig) -> String {
        config.host_to_sandbox(path).to_string_lossy().into_owned()
    }

    /// Translate sandbox-side reply paths (glob results, grep entries) back
    /// to the host view.
    fn host_path(&self, sandbox: &str, config: &NamespaceConfig) -> PathBuf {
        config.sandbox_to_host(Path::new(sandbox))
    }

    /// Run one FsOp and decode its reply into the expected payload.
    fn run_op(&self, op: FsOp) -> Result<FsReply, FsError> {
        self.sandbox.fs(op).map_err(fs_err)
    }
}

impl FsBackend for SandboxFs {
    fn read(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, FsError> {
        let config = self.config()?;
        let reply = self.run_op(FsOp::Read {
            path: self.sandbox_path(path, &config),
            max_bytes,
        })?;
        match reply {
            FsReply::Bytes { data } => STANDARD
                .decode(data)
                .map_err(|e| FsError::new(format!("decode read bytes: {e}"))),
            other => Err(fs_err(format!("read: unexpected reply {other:?}"))),
        }
    }

    fn metadata(&self, path: &Path) -> Result<Value, FsError> {
        let config = self.config()?;
        let reply = self.run_op(FsOp::Metadata {
            path: self.sandbox_path(path, &config),
        })?;
        match reply {
            FsReply::Value { payload } => Ok(payload),
            other => Err(fs_err(format!("metadata: unexpected reply {other:?}"))),
        }
    }

    fn dir(&self, path: &Path, depth: u32) -> Result<Value, FsError> {
        let config = self.config()?;
        let reply = self.run_op(FsOp::Dir {
            path: self.sandbox_path(path, &config),
            depth,
        })?;
        match reply {
            FsReply::Value { payload } => Ok(payload),
            other => Err(fs_err(format!("dir: unexpected reply {other:?}"))),
        }
    }

    fn exists(&self, path: &Path) -> Result<bool, FsError> {
        let config = self.config()?;
        let reply = self.run_op(FsOp::Exists {
            path: self.sandbox_path(path, &config),
        })?;
        match reply {
            FsReply::Value { payload } => payload
                .as_bool()
                .ok_or_else(|| FsError::new("exists: reply was not a bool")),
            other => Err(fs_err(format!("exists: unexpected reply {other:?}"))),
        }
    }

    fn write(&self, path: &Path, content: &[u8]) -> Result<(), FsError> {
        let config = self.config()?;
        self.run_op(FsOp::Write {
            path: self.sandbox_path(path, &config),
            content: STANDARD.encode(content),
        })
        .map(|_| ())
    }

    fn append(&self, path: &Path, content: &[u8]) -> Result<(), FsError> {
        let config = self.config()?;
        self.run_op(FsOp::Append {
            path: self.sandbox_path(path, &config),
            content: STANDARD.encode(content),
        })
        .map(|_| ())
    }

    fn atomic_write(&self, path: &Path, content: &[u8]) -> Result<(), FsError> {
        let config = self.config()?;
        self.run_op(FsOp::AtomicWrite {
            path: self.sandbox_path(path, &config),
            content: STANDARD.encode(content),
        })
        .map(|_| ())
    }

    fn remove(&self, path: &Path, recursive: bool) -> Result<(), FsError> {
        let config = self.config()?;
        self.run_op(FsOp::Remove {
            path: self.sandbox_path(path, &config),
            recursive,
        })
        .map(|_| ())
    }

    fn mkdir(&self, path: &Path, parents: bool) -> Result<(), FsError> {
        let config = self.config()?;
        self.run_op(FsOp::Mkdir {
            path: self.sandbox_path(path, &config),
            parents,
        })
        .map(|_| ())
    }

    fn glob(
        &self,
        root: &Path,
        patterns: &[String],
        gitignore: bool,
        sort_mtime: bool,
        limit: Option<usize>,
    ) -> Result<Vec<PathBuf>, FsError> {
        let config = self.config()?;
        let reply = self.run_op(FsOp::Glob {
            path: self.sandbox_path(root, &config),
            patterns: patterns.to_vec(),
            gitignore,
            sort_mtime,
            limit,
        })?;
        let FsReply::Value { payload } = reply else {
            return Err(fs_err(format!("glob: unexpected reply {reply:?}")));
        };
        let paths: Vec<&str> = payload
            .as_array()
            .ok_or_else(|| FsError::new("glob: reply was not an array"))?
            .iter()
            .filter_map(Value::as_str)
            .collect();
        Ok(paths
            .into_iter()
            .map(|p| self.host_path(p, &config))
            .collect())
    }

    fn grep(&self, params: GrepParams) -> Result<Vec<GrepFileEntry>, FsError> {
        let config = self.config()?;
        let path = params
            .path
            .as_deref()
            .map(|p| self.sandbox_path(Path::new(p), &config));
        let reply = self.run_op(FsOp::Grep {
            path,
            pattern: params.pattern,
            include: params.include,
            context_before: params.context_before,
            context_after: params.context_after,
            limit: params.limit,
            max_line_bytes: params.max_line_bytes,
        })?;
        let FsReply::Value { payload } = reply else {
            return Err(fs_err(format!("grep: unexpected reply {reply:?}")));
        };
        let entries: Vec<GrepFileEntry> = serde_json::from_value(payload)
            .map_err(|e| FsError::new(format!("grep: decode reply: {e}")))?;
        Ok(entries
            .into_iter()
            .map(|mut e| {
                e.path = self
                    .host_path(&e.path, &config)
                    .to_string_lossy()
                    .into_owned();
                e
            })
            .collect())
    }

    fn exec(
        &self,
        command: &str,
        workdir: Option<&str>,
        timeout_secs: Option<u64>,
    ) -> Result<(String, i32), FsError> {
        let config = self.config()?;
        let workdir = workdir.map(|w| self.sandbox_path(Path::new(w), &config));
        self.sandbox
            .exec(command, workdir.as_deref(), timeout_secs)
            .map_err(fs_err)
    }
}
