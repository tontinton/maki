pub(crate) mod error;
pub mod model;
pub mod provider;
pub(crate) mod providers;
pub mod retry;
pub(crate) mod types;

pub use error::AgentError;
pub use model::{
    Model, ModelEntry, ModelError, ModelFamily, ModelPricing, ModelSet, ModelTier, TokenUsage,
    models_for_provider,
};
pub use providers::dynamic;
pub use providers::openai_auth;
pub fn set_openai_plan_codex_cli_version(version: &str) {
    providers::openai::set_plan_codex_cli_version(version);
}
pub use types::{
    ContentBlock, ImageMediaType, ImageSource, Message, ProviderEvent, Role, StopReason,
    StreamResponse, ThinkingConfig,
};
