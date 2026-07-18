pub(crate) mod error;
pub mod model;
pub mod model_registry;
pub mod provider;
pub(crate) mod providers;
pub mod retry;
pub(crate) mod types;

pub use error::AgentError;
pub use model::{
    FastPricing, Model, ModelEntry, ModelError, ModelFamily, ModelInfo, ModelPricing, ModelTier,
    TokenUsage, models_for_provider,
};
pub use providers::Timeouts;
pub use providers::copilot::auth as copilot_auth;
pub use providers::dynamic;
pub use providers::openai::auth as openai_auth;
pub use providers::opencode::{
    ProviderData, catalog_provider, catalog_providers, catalog_providers_if_available,
};
pub use types::{
    ContentBlock, Effort, EffortDialect, IMAGE_OMITTED_NOTE, ImageMediaType, ImageSource, Message,
    ProviderEvent, ProviderUsage, RequestOptions, Role, StopReason, StreamResponse, ThinkingConfig,
    UsageLimit, adapt_images_for_model, dialect,
};
