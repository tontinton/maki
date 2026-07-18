use std::sync::{Arc, Mutex};

use flume::Sender;
use serde_json::Value;

use maki_config::providers::{
    Protocol, ProvidersConfig, builtin_provider, resolve_api_key_env, resolve_base_url,
    resolve_protocol,
};
use maki_storage::id::SessionRef;

use super::ResolvedAuth;
use super::openai::responses;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::model::{FastPricing, Model, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider, ProviderKind};
use crate::providers::Timeouts;
use crate::types::ThinkingConfig;
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse};

static CUSTOM_OPENAI_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "",
    base_url: "",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "custom",
};

fn resolve_provider_kind(slug: &str) -> Option<ProviderKind> {
    let config = ProvidersConfig::load();
    let def = config.get(slug)?;
    match def.protocol? {
        Protocol::Openai | Protocol::OpenaiResponses => Some(ProviderKind::OpenAi),
        Protocol::Anthropic => Some(ProviderKind::Anthropic),
        Protocol::Google => Some(ProviderKind::Google),
    }
}

fn resolve_custom_auth(slug: &str) -> Result<ResolvedAuth, AgentError> {
    let config = ProvidersConfig::load();
    let def = config.get(slug).ok_or_else(|| AgentError::Config {
        message: format!("unknown custom provider '{slug}'"),
    })?;

    let resolved_env = resolve_api_key_env(slug, Some(def));
    let env_var = def.api_key_env.as_deref().unwrap_or(&resolved_env);
    let pool = super::KeyPool::resolve(slug, env_var)?;

    let base_url = resolve_base_url(slug, Some(def));
    let mut auth = ResolvedAuth::bearer(pool.current());
    auth.base_url = base_url;
    Ok(auth)
}

pub fn create(slug: &str, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    let kind = resolve_provider_kind(slug).ok_or_else(|| AgentError::Config {
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
    let config = ProvidersConfig::load();
    let def = config.get(slug)?;
    let kind = match def.protocol? {
        Protocol::Openai | Protocol::OpenaiResponses => ProviderKind::OpenAi,
        Protocol::Anthropic => ProviderKind::Anthropic,
        Protocol::Google => ProviderKind::Google,
    };
    let declared = def.models.iter().find(|m| m.id == model_id);
    let tier = declared
        .map(|m| ModelTier::from(m.tier))
        .unwrap_or(ModelTier::Medium);
    let max_output_tokens = declared
        .and_then(|m| m.max_output_tokens)
        .or_else(|| kind.fallback_max_output());
    let context_window = declared
        .and_then(|m| m.context_window)
        .unwrap_or_else(|| kind.fallback_context_window());
    let supports_tool_examples_override = declared.and_then(|m| m.supports_tool_examples);
    let supports_thinking_override = declared.and_then(|m| m.supports_thinking);
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
        })
        .unwrap_or_default();
    Some(Model {
        id: model_id.to_string(),
        provider: kind,
        dynamic_slug: Some(slug.to_string()),
        tier,
        family: kind.family(),
        supports_tool_examples_override,
        supports_thinking_override,
        supports_vision_override,
        pricing,
        max_output_tokens,
        context_window,
    })
}

/// Specs declared statically in `providers.toml` (no HTTP).
pub fn declared_model_specs() -> Vec<String> {
    let config = ProvidersConfig::load();
    let mut specs = Vec::new();
    for (slug, def) in &config.providers {
        if builtin_provider(slug).is_some() {
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

pub fn find_model_for_tier(slug: &str, tier: ModelTier) -> Option<Model> {
    let config = ProvidersConfig::load();
    let def = config.get(slug)?;
    let declared = def
        .models
        .iter()
        .find(|m| ModelTier::from(m.tier) == tier)?;
    lookup_model(slug, &declared.id)
}

/// Skip definitions handled by [`declared_model_specs`]; only HTTP `/models`
/// goes through here, so an empty `discover_models = false` provider returns
/// nothing and never hits the network.
pub fn discover_models(timeouts: Timeouts) -> Vec<String> {
    let config = ProvidersConfig::load();
    let mut all_specs = Vec::new();
    for slug in config.providers.keys() {
        if builtin_provider(slug).is_some() {
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
                    Ok(models) => {
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
