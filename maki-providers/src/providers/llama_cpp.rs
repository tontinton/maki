use crate::model::ModelEntry;

use super::local::LLAMACPP;

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: LLAMACPP.slug,
    display_name: LLAMACPP.display_name,
    protocol: maki_config::providers::Protocol::Openai,
    default_base_url: LLAMACPP.default_host,
    default_api_key_env: LLAMACPP.api_key_env,
    default_model: LLAMACPP.default_model,
    plans: None,
    login_url: None,
    needs_url: true,
});

pub(crate) const fn models() -> &'static [ModelEntry] {
    &[]
}
