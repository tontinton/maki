use std::sync::{Arc, Mutex};

use flume::Sender;
use serde_json::Value;

use maki_config::providers::{
    Protocol, ProviderDef, ProvidersConfig, resolve_api_key_env, resolve_base_url, resolve_protocol,
};
use maki_storage::id::SessionRef;

use super::ResolvedAuth;
use super::openai::responses;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::manifest::ManifestRegistry;
use crate::model::{FastPricing, Model, ModelInfo, ModelPricing, ModelTier, ThinkingSupport};
use crate::provider::{BoxFuture, Provider, ProviderKind};
use crate::providers::Timeouts;
use crate::types::ThinkingConfig;
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse};

static CUSTOM_OPENAI_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    // Custom providers resolve their own base URL (including any override) from
    // config, so the compat-layer fallback slug is unused here.
    slug: "",
    api_key_env: "",
    base_url: "",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "custom",
};

fn protocol_kind(protocol: Protocol) -> ProviderKind {
    match protocol {
        Protocol::Openai | Protocol::OpenaiResponses => ProviderKind::OpenAi,
        Protocol::Anthropic => ProviderKind::Anthropic,
        Protocol::Google => ProviderKind::Google,
    }
}

/// Builtins win their slug in `from_spec`/`create`, so every custom path skips
/// them. Key off the manifest (every builtin), not `builtin_provider`, which
/// omits the `opencode` slugs and would let them shadow the builtin.
fn is_builtin_slug(slug: &str) -> bool {
    ManifestRegistry::get(slug).is_some()
}

pub fn base_kind(slug: &str) -> Option<ProviderKind> {
    let config = ProvidersConfig::load();
    Some(protocol_kind(config.get(slug)?.protocol?))
}

fn resolve_custom_auth(slug: &str) -> Result<ResolvedAuth, AgentError> {
    let config = ProvidersConfig::load();
    let def = config.get(slug).ok_or_else(|| AgentError::Config {
        message: format!("unknown custom provider '{slug}'"),
    })?;

    let resolved_env = resolve_api_key_env(slug, Some(def));
    let env_var = def.api_key_env.as_deref().unwrap_or(&resolved_env);
    let pool = super::KeyPool::resolve(slug, env_var)?;

    Ok(
        ResolvedAuth::bearer(slug, pool.current())?
            .with_base_url(resolve_base_url(slug, Some(def))),
    )
}

pub fn create(slug: &str, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    let kind = base_kind(slug).ok_or_else(|| AgentError::Config {
        message: format!("unknown custom provider '{slug}'"),
    })?;
    let resolved = resolve_custom_auth(slug)?;
    let auth = Arc::new(Mutex::new(resolved));

    let config = ProvidersConfig::load();
    let protocol = resolve_protocol(slug, config.get(slug)).unwrap_or(Protocol::Openai);

    match kind {
        ProviderKind::Anthropic => Ok(Box::new(super::anthropic::Anthropic::with_auth(
            auth, timeouts,
        ))),
        ProviderKind::OpenAi => Ok(Box::new(CustomOpenAiProvider {
            compat: OpenAiCompatProvider::new(&CUSTOM_OPENAI_CONFIG, timeouts),
            auth,
            protocol,
        })),
        ProviderKind::Google => Ok(Box::new(super::google::Google::with_auth(auth, timeouts))),
        _ => Err(AgentError::Config {
            message: format!(
                "unsupported protocol for custom provider '{slug}', only openai/anthropic/google are supported"
            ),
        }),
    }
}

pub fn lookup_model(slug: &str, model_id: &str) -> Option<Model> {
    if is_builtin_slug(slug) {
        return None;
    }
    let config = ProvidersConfig::load();
    let def = config.get(slug)?;
    let kind = protocol_kind(def.protocol?);
    Some(model_from_def(def, kind, slug, model_id))
}

/// Build a model from an already-loaded provider definition so tier resolution
/// and id lookup can share one `providers.toml` read instead of loading twice.
/// The model id to price a subsidised-but-unpriced model from the Anthropic
/// catalog under, or `None` when the fallback does not apply. Gated on the
/// provider's protocol: only Anthropic-protocol providers serve Anthropic
/// models, so any other kind must not get Anthropic catalog rates. `-1m`
/// context variants price the same as their base model.
fn catalog_fallback_id<'a>(
    kind: ProviderKind,
    pricing: &ModelPricing,
    subsidised: bool,
    model_id: &'a str,
) -> Option<&'a str> {
    (pricing.is_zero() && subsidised && matches!(kind, ProviderKind::Anthropic))
        .then(|| model_id.strip_suffix("-1m").unwrap_or(model_id))
}

fn model_from_def(def: &ProviderDef, kind: ProviderKind, slug: &str, model_id: &str) -> Model {
    let subsidy_source = def.subsidised_by.as_deref();
    let declared = def.models.iter().find(|m| m.id == model_id);
    let tier = declared
        .map(|m| ModelTier::from(m.tier))
        .unwrap_or(ModelTier::Medium);
    let discovered = crate::model_registry::discovered(slug, model_id);
    let discovered = discovered.as_ref();
    let max_output_tokens = declared
        .and_then(|m| m.max_output_tokens)
        .or_else(|| discovered.and_then(|d| d.max_output_tokens))
        .or_else(|| kind.fallback_max_output());
    // The builtin manifest path (model.rs::from_manifest) applies this so
    // `<id>-1m` resolves to 1M context regardless of what /v1/models said.
    // Custom Anthropic slugs (cliproxy on Claude Max) need the same behaviour
    // or every -1m variant reads back as the 200K protocol default.
    let context_window = declared
        .and_then(|m| m.context_window)
        .or_else(|| discovered.and_then(|d| d.context_window))
        .or_else(|| super::anthropic::shared::long_context_window(model_id))
        .unwrap_or_else(|| kind.fallback_context_window());
    let supports_tool_examples_override = declared.and_then(|m| m.supports_tool_examples);
    let thinking_override = ThinkingSupport::from_flags(
        declared
            .and_then(|m| m.supports_thinking)
            .or_else(|| ManifestRegistry::get(&kind.to_string()).map(|m| m.supports_thinking)),
        declared.and_then(|m| m.requires_thinking).unwrap_or(false),
    );
    let supports_vision_override = declared.and_then(|m| m.supports_vision);
    let pricing = declared
        .filter(|m| m.has_pricing())
        .map(|m| ModelPricing {
            input: m.pricing_input.unwrap_or(0.0),
            output: m.pricing_output.unwrap_or(0.0),
            cache_write: m.pricing_cache_write.unwrap_or(0.0),
            cache_read: m.pricing_cache_read.unwrap_or(0.0),
            fast: declared
                .filter(|d| d.has_fast_pricing())
                .map(|d| FastPricing {
                    input: d.pricing_fast_input.unwrap_or(0.0),
                    output: d.pricing_fast_output.unwrap_or(0.0),
                }),
            subsidised_by: None,
        })
        .unwrap_or_default();
    // A subsidised provider (Claude Max via cliproxy) rarely quotes its own
    // rates -- it is free at the point of use -- so fall back to the
    // published list price purely as the reference shown alongside the $0
    // bill.
    let mut pricing = pricing;
    if let Some(base_id) = catalog_fallback_id(kind, &pricing, subsidy_source.is_some(), model_id)
        && let Some(meta) = crate::providers::catalog::model_meta_if_available("anthropic", base_id)
    {
        pricing = ModelPricing::per_token(
            meta.input_price,
            meta.output_price,
            meta.cache_write,
            meta.cache_read,
        );
    }
    if let Some(source) = subsidy_source {
        pricing.subsidised_by = Some(Arc::from(source));
    }
    Model {
        id: model_id.to_string(),
        provider: Arc::from(slug),
        tier,
        family: kind.family(),
        supports_tool_examples_override,
        thinking_override,
        supports_vision_override,
        pricing,
        discovered_free: false,
        max_output_tokens,
        context_window,
        thinking_fields: None,
    }
}

/// Specs declared statically in `providers.toml` (no HTTP).
pub fn declared_model_specs() -> Vec<String> {
    declared_specs_from(&ProvidersConfig::load())
}

fn declared_specs_from(config: &ProvidersConfig) -> Vec<String> {
    let mut specs = Vec::new();
    for (slug, def) in &config.providers {
        if is_builtin_slug(slug) {
            continue;
        }
        if resolve_protocol(slug, Some(def)).is_none() {
            continue;
        }
        for m in &def.models {
            specs.push(format!("{slug}/{}", m.id));
        }
    }
    specs
}

/// Outcome of resolving a tier against `providers.toml` in a single read.
pub enum TierLookup {
    Model(Model),
    /// Provider exists but declares no model at this tier; carries the base kind
    /// so the caller can inherit the base protocol's default.
    NoModelForTier(ProviderKind),
    Unknown,
}

pub fn resolve_tier(slug: &str, tier: ModelTier) -> TierLookup {
    // Builtins are never overridden through providers.toml (from_spec/create
    // check builtin first); keep the tier path consistent with that.
    if is_builtin_slug(slug) {
        return TierLookup::Unknown;
    }
    let config = ProvidersConfig::load();
    let Some(def) = config.get(slug) else {
        return TierLookup::Unknown;
    };
    let Some(protocol) = def.protocol else {
        return TierLookup::Unknown;
    };
    let kind = protocol_kind(protocol);
    match def.models.iter().find(|m| ModelTier::from(m.tier) == tier) {
        Some(declared) => TierLookup::Model(model_from_def(def, kind, slug, &declared.id)),
        None => TierLookup::NoModelForTier(kind),
    }
}

/// Skip definitions handled by [`declared_model_specs`]; only HTTP `/models`
/// goes through here, so an empty `discover_models = false` provider returns
/// nothing and never hits the network.
pub fn discover_models(timeouts: Timeouts) -> Vec<String> {
    let config = ProvidersConfig::load();
    let mut all_specs = Vec::new();
    for slug in config.providers.keys() {
        if is_builtin_slug(slug) {
            continue;
        }
        let def = config.get(slug).unwrap();
        if !def.discover_models {
            continue;
        }
        if resolve_protocol(slug, Some(def)).is_none() {
            continue;
        }
        match create(slug, timeouts) {
            Ok(provider) => {
                let slug_c = slug.clone();
                let result = smol::block_on(provider.list_models());
                match result {
                    Ok(mut models) => {
                        overlay_declared_tiers(def, &mut models);
                        crate::model_registry::set_known_models(&slug_c, models.clone());
                        for m in models {
                            all_specs.push(format!("{slug_c}/{}", m.id));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(slug, error = %e, "failed to list models for custom provider");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(slug, error = %e, "failed to create custom provider");
            }
        }
    }
    all_specs
}

/// Discovery via the openai compat layer never reports tiers, so stored models
/// would only resolve positionally in `spec_for_tier`, shadowing tiers declared
/// in `providers.toml`. Copying declared tiers onto the discovered entries lets
/// the metadata candidate win and keeps declared config authoritative.
fn overlay_declared_tiers(def: &ProviderDef, models: &mut [ModelInfo]) {
    for model in models {
        if let Some(declared) = def.models.iter().find(|m| m.id == model.id) {
            model.tier = Some(ModelTier::from(declared.tier));
        }
    }
}

struct CustomOpenAiProvider {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    protocol: Protocol,
}

impl Provider for CustomOpenAiProvider {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        _session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();

            if self.protocol == Protocol::OpenaiResponses {
                let body = responses::build_body(model, messages, system, tools);
                // TODO: wire thinking budget into responses API when llama.cpp supports it
                return responses::do_stream(
                    self.compat.client(),
                    model,
                    &body,
                    event_tx,
                    &auth,
                    self.compat.stream_timeout(),
                )
                .await;
            }

            let mut body = self.compat.build_body(model, messages, system, tools);
            if matches!(opts.thinking, ThinkingConfig::Off) {
                body["thinking"] = serde_json::json!({"type": "disabled"});
            }
            self.compat
                .do_stream(model, &[], &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        let auth = self.auth.lock().unwrap().clone();
        Box::pin(async move { self.compat.do_list_models(&auth).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openai_def(model_id: &str) -> ProviderDef {
        serde_json::from_str(&format!(
            r#"{{"protocol":"openai","models":[{{"id":"{model_id}"}}]}}"#
        ))
        .unwrap()
    }

    // `opencode` is a builtin whose slug is absent from the `builtin_provider`
    // inventory; the old guard leaked it into the picker, where it then resolved
    // as the builtin and silently dropped the custom model. Listing must skip
    // every builtin slug so a providers.toml entry can never shadow one.
    #[test]
    fn declared_specs_skip_builtin_named_entries_but_keep_custom() {
        let mut config = ProvidersConfig::default();
        config.upsert("opencode".to_string(), openai_def("shadow-model"));
        config.upsert("my-custom".to_string(), openai_def("real-model"));

        let specs = declared_specs_from(&config);
        assert!(
            !specs.iter().any(|s| s.starts_with("opencode/")),
            "builtin slug must be skipped in custom listing: {specs:?}"
        );
        assert!(specs.contains(&"my-custom/real-model".to_string()));

        // Resolution owns the builtin slug regardless of the providers.toml entry.
        let model = Model::from_spec("opencode/shadow-model").unwrap();
        assert_eq!(model.provider.as_ref(), "opencode");
    }

    // The exact regression this fixes: discovery parsed context_window but
    // never stored it, so custom models always got the protocol fallback.
    #[test]
    fn discovered_metadata_flows_into_custom_model_from_def() {
        let slug = "custom-discovery-metadata-test";
        let model_id = "vllm-model";
        let expected_window: u32 = 131_072;
        let expected_output: u32 = 8_192;

        crate::model_registry::set_known_models(
            slug,
            vec![ModelInfo {
                context_window: Some(expected_window),
                max_output_tokens: Some(expected_output),
                ..ModelInfo::id_only(model_id.to_string())
            }],
        );

        let def = openai_def(model_id);
        let model = model_from_def(&def, ProviderKind::OpenAi, slug, model_id);
        assert_eq!(model.context_window, expected_window);
        assert_eq!(model.max_output_tokens, Some(expected_output));
    }

    #[test]
    fn overlay_declared_tiers_sets_tier_for_declared_models_only() {
        let def: ProviderDef = serde_json::from_str(
            r#"{"protocol":"openai","models":[{"id":"declared","tier":"strong"}]}"#,
        )
        .unwrap();
        let mut models = vec![
            ModelInfo::id_only("declared".to_string()),
            ModelInfo::id_only("undeclared".to_string()),
        ];

        overlay_declared_tiers(&def, &mut models);
        assert_eq!(models[0].tier, Some(ModelTier::Strong));
        assert_eq!(models[1].tier, None);
    }

    use crate::TokenUsage;

    fn subsidised_def(protocol: &str, model_id: &str) -> ProviderDef {
        serde_json::from_str(&format!(
            r#"{{"protocol":"{protocol}","subsidised_by":"Max","models":[{{"id":"{model_id}"}}]}}"#
        ))
        .unwrap()
    }

    #[test]
    fn catalog_fallback_only_for_unpriced_subsidised_anthropic() {
        let zero = ModelPricing::ZERO;
        assert_eq!(
            catalog_fallback_id(ProviderKind::Anthropic, &zero, true, "claude-x"),
            Some("claude-x")
        );
        // `-1m` context variants price the same as their base model.
        assert_eq!(
            catalog_fallback_id(ProviderKind::Anthropic, &zero, true, "claude-x-1m"),
            Some("claude-x")
        );
        // Another protocol must never pick up Anthropic catalog rates.
        assert_eq!(
            catalog_fallback_id(ProviderKind::OpenAi, &zero, true, "claude-x"),
            None
        );
        // Declared/discovered rates win over the catalog.
        let priced = ModelPricing::per_token(3.0, 15.0, 0.0, 0.0);
        assert_eq!(
            catalog_fallback_id(ProviderKind::Anthropic, &priced, true, "claude-x"),
            None
        );
        // No subsidy, no reference price to backfill.
        assert_eq!(
            catalog_fallback_id(ProviderKind::Anthropic, &zero, false, "claude-x"),
            None
        );
    }

    // The subsidy is stamped whichever path supplied the rates, and an
    // unpriced non-Anthropic model must not report a false $0 bill.
    #[test]
    fn subsidised_def_stamps_declared_pricing() {
        let mut def = subsidised_def("openai", "my-model");
        def.models[0].pricing_input = Some(3.0);
        def.models[0].pricing_output = Some(15.0);
        let model = model_from_def(&def, ProviderKind::OpenAi, "my-proxy", "my-model");
        assert_eq!(model.subsidy_source(), Some("Max"));
        assert_eq!(model.pricing.input, 3.0);
        let usage = TokenUsage {
            input: 1_000_000,
            output: 0,
            cache_creation: 0,
            cache_read: 0,
            ..Default::default()
        };
        assert_eq!(model.billed_cost(&usage, false), Some(0.0));
    }

    #[test]
    fn subsidised_def_without_pricing_stays_unpriced() {
        let def = subsidised_def("openai", "my-model");
        let model = model_from_def(&def, ProviderKind::OpenAi, "my-proxy", "my-model");
        assert_eq!(model.subsidy_source(), Some("Max"));
        assert!(model.pricing.is_zero());
        let usage = TokenUsage {
            input: 1_000_000,
            output: 0,
            cache_creation: 0,
            cache_read: 0,
            ..Default::default()
        };
        assert_eq!(model.billed_cost(&usage, false), None);
    }
}
