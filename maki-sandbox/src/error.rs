#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("fork failed: {0}")]
    Fork(String),

    #[error("namespace setup failed: {0}")]
    Namespace(String),

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
