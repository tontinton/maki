pub(crate) mod error;
pub(crate) mod image;
pub(crate) mod manifest;
pub mod model;
pub mod model_registry;
pub mod pricing;
pub mod provider;
pub(crate) mod providers;
pub mod retry;
pub mod spec;
/// One recorded server for two audiences: other crates get it behind the
/// feature, this crate's own tests get it without.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod tokens;
pub(crate) mod types;

pub use error::{AgentError, Overflow};
pub use maki_storage::sessions::add_cost;
pub use model::{
    FastPricing, Model, ModelEntry, ModelError, ModelFamily, ModelInfo, ModelPricing, ModelTier,
    ThinkingOption, ThinkingSupport, TokenUsage, format_tokens,
};
pub use pricing::{model_cost, settle_session};
pub use providers::catalog::ProviderData;
pub use providers::catalog::{
    catalog_provider, catalog_provider_if_available, catalog_providers,
    catalog_providers_if_available, refresh_catalog, warm_catalog,
};
pub use providers::copilot::auth as copilot_auth;
pub use providers::openai::auth as openai_auth;
pub use providers::plugin;
pub use providers::xai::auth as xai_auth;
pub use providers::{KeyHeader, KeyPool, KeyRotation, ResolvedAuth};
pub use providers::{Timeouts, user_agent};
/// The golden replay harness and the recorded cases of every ported provider,
/// published on the same terms as [`test_support`] and for the same reason:
/// the authoring that ships is a Lua plugin, which only `maki-lua` can stage,
/// and it answers to these goldens.
#[cfg(any(test, feature = "test-support"))]
pub use providers::{
    deepseek::fixtures as deepseek_fixtures, mistral::fixtures as mistral_fixtures,
    openrouter::fixtures as openrouter_fixtures, regolo::fixtures as regolo_fixtures, replay,
    requesty::fixtures as requesty_fixtures, synthetic::fixtures as synthetic_fixtures,
    tensorx::fixtures as tensorx_fixtures,
};
pub use tokens::{ContextGauge, estimate_message_tokens, estimate_prompt_tokens};
pub use types::{
    ContentBlock, EMPTY_RESPONSE_MARKER, Effort, EffortDialect, IMAGE_EVICTED_NOTE,
    IMAGE_OMITTED_NOTE, IMAGE_PLACEHOLDER, IMAGE_UNUSABLE_NOTE, ImageMediaType, ImageSource,
    Message, MessageKind, ModelUsageRow, ProviderEvent, ProviderUsage, RequestOptions, Role,
    StopReason, StreamResponse, THINKING_USAGE, ThinkingConfig, ThinkingFallback, UsageLimit,
    adapt_images_for_model, dialect,
};
