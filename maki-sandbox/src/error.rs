#[derive(Debug, thiserror::Error, serde::Serialize, serde::Deserialize)]
pub enum SandboxError {
    #[error("fork failed: {0}")]
    Fork(String),

    #[error("namespace setup failed: {0}")]
    Namespace(String),

    /// The host refused `unshare(CLONE_NEWNS)`, so no filesystem could be
    /// built. There is no degraded mode to fall back to: without the mounts
    /// the host/sandbox path mapping does not hold, and every fs call would
    /// silently miss. Callers should treat the sandbox as unavailable.
    #[error("filesystem isolation unavailable: unshare(CLONE_NEWNS) failed: {0}")]
    IsolationUnavailable(String),

    #[error("mount failed: {0}")]
    Mount(String),

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("environment setup failed: {0}")]
    Env(String),

    #[error("exec failed: {0}")]
    Exec(String),

    #[error("mutex poisoned: {0}")]
    MutexPoisoned(String),
}

impl SandboxError {
    /// Whether the host cannot sandbox at all, as opposed to a sandbox that
    /// broke. Callers use this to tell the user their system cannot isolate
    /// processes, rather than reporting an opaque failure.
    #[must_use]
    pub fn is_isolation_unavailable(&self) -> bool {
        matches!(self, Self::IsolationUnavailable(_))
    }
}
