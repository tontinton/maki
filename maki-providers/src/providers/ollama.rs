use crate::model::ModelEntry;

use super::local::OLLAMA;

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: OLLAMA.slug,
    display_name: OLLAMA.display_name,
    protocol: maki_config::providers::Protocol::Openai,
    default_base_url: OLLAMA.default_host,
    default_api_key_env: OLLAMA.api_key_env,
    default_model: OLLAMA.default_model,
    plans: None,
    login_url: None,
    needs_url: true,
});

pub(crate) const fn models() -> &'static [ModelEntry] {
    &[]
}
