pub(crate) mod error;
pub mod manifest;
pub mod model;
pub mod model_registry;
pub mod models_cache;
pub mod pricing;
pub mod provider;
pub(crate) mod providers;
pub mod retry;
pub(crate) mod types;

pub use error::AgentError;
pub use maki_storage::sessions::add_cost;
pub use model::{
    FastPricing, Model, ModelEntry, ModelError, ModelFamily, ModelInfo, ModelPricing, ModelTier,
    ThinkingSupport, TokenUsage, format_tokens,
};
pub use pricing::{model_cost, settle_session};
pub use providers::Timeouts;
pub use providers::catalog::ProviderData;
pub use providers::catalog::{
    catalog_provider, catalog_provider_if_available, catalog_providers,
    catalog_providers_if_available, model_meta_if_available, warm_catalog,
};
pub use providers::copilot::auth as copilot_auth;
pub use providers::dynamic;
pub use providers::openai::auth as openai_auth;
pub use providers::xai::auth as xai_auth;
pub use types::{
    ContentBlock, EMPTY_RESPONSE_MARKER, Effort, EffortDialect, IMAGE_OMITTED_NOTE, ImageMediaType,
    ImageSource, Message, MessageKind, ModelUsageRow, ProviderEvent, ProviderUsage, RequestOptions,
    Role, StopReason, StreamResponse, THINKING_USAGE, ThinkingConfig, UsageLimit,
    adapt_images_for_model, dialect,
};
