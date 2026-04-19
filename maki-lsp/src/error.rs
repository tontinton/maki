use thiserror::Error;

#[derive(Debug, Error)]
pub enum LspError {
    #[error("no LSP server configured for {language} files")]
    ServerNotConfigured { language: String },

    #[error("failed to start LSP server {server}: {reason}")]
    StartFailed { server: String, reason: String },

    #[error("LSP server {server} died")]
    ServerDied { server: String },

    #[error("LSP request failed ({server}): {message}")]
    RequestFailed { server: String, message: String },

    #[error("LSP request timed out ({server}, {timeout_ms}ms)")]
    Timeout { server: String, timeout_ms: u64 },

    #[error("invalid LSP response from {server}: {reason}")]
    InvalidResponse { server: String, reason: String },
}
