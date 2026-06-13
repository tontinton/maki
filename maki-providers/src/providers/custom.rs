use std::sync::{Arc, Mutex};

use flume::Sender;
use serde_json::Value;

use maki_config::providers::{
    Protocol, ProvidersConfig, builtin_provider, resolve_api_key_env, resolve_base_url,
    resolve_protocol,
};

use super::ResolvedAuth;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::model::{Model, ModelTier};
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
        Protocol::Openai => Some(ProviderKind::OpenAi),
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

    match kind {
        ProviderKind::Anthropic => Ok(Box::new(super::anthropic::Anthropic::with_auth(
            auth, timeouts,
        ))),
        ProviderKind::OpenAi => Ok(Box::new(CustomOpenAiProvider {
            compat: OpenAiCompatProvider::new(&CUSTOM_OPENAI_CONFIG, timeouts),
            auth,
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
    let kind = resolve_provider_kind(slug)?;
    Some(Model {
        id: model_id.to_string(),
        provider: kind,
        dynamic_slug: Some(slug.to_string()),
        tier: ModelTier::Medium,
        family: kind.family(),
        supports_tool_examples_override: None,
        pricing: Default::default(),
        max_output_tokens: kind.fallback_max_output(),
        context_window: kind.fallback_context_window(),
    })
}

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
                            all_specs.push(format!("{slug_c}/{m}"));
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
        _session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        let auth = self.auth.lock().unwrap().clone();
        let mut body = self.compat.build_body(model, messages, system, tools);
        if matches!(opts.thinking, ThinkingConfig::Off) {
            body["thinking"] = serde_json::json!({"type": "disabled"});
        }
        Box::pin(async move {
            self.compat
                .do_stream(model, &[], &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        let auth = self.auth.lock().unwrap().clone();
        Box::pin(async move { self.compat.do_list_models(&auth).await })
    }
}
