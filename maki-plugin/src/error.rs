use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("plugin not found: {0}")]
    NotFound(String),
    #[error("marketplace not found: {0}")]
    MarketplaceNotFound(String),
    #[error("plugin already installed: {0}")]
    AlreadyInstalled(String),
    #[error("marketplace already registered: {0}")]
    MarketplaceAlreadyRegistered(String),
    #[error("invalid manifest at {path}: {reason}")]
    InvalidManifest { path: PathBuf, reason: String },
    #[error("network error: {0}")]
    Network(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Storage(#[from] maki_storage::StorageError),
    #[error("toml parse error: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("toml serialize error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    #[error("json parse error: {0}")]
    JsonParse(#[from] serde_json::Error),
}
