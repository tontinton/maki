use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use flume::Sender;
use serde_json::Value;
use strum::{Display, EnumIter, EnumString};
use tracing::{debug, warn};

use maki_config::ModelPolicy;
use maki_storage::id::SessionRef;

use crate::model::{Model, ModelFamily, ModelInfo};
use crate::providers::Timeouts;
use crate::providers::anthropic::Anthropic;
use crate::providers::anthropic::bedrock;
use crate::providers::aperture::Aperture;
use crate::providers::catalog::{
    OPENCODE_FAMILY_SLUGS, available_if_warm, catalog_providers, catalog_providers_if_available,
};
use crate::providers::commandcode::CommandCode;
use crate::providers::copilot::Copilot;
use crate::providers::deepseek::DeepSeek;
use crate::providers::dynamic;
use crate::providers::google::Google;
use crate::providers::local::{LLAMACPP, LocalEndpoint, OLLAMA};
use crate::providers::mistral::Mistral;
use crate::providers::openai::OpenAi;
use crate::providers::opencode::Opencode;
use crate::providers::openrouter::OpenRouter;
use crate::providers::synthetic::Synthetic;
use crate::providers::tensorx::TensorX;
use crate::providers::xai::Xai;
use crate::providers::zai::Zai;
use crate::{AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, EnumIter)]
#[strum(serialize_all = "kebab-case")]
pub enum ProviderKind {
    Anthropic,
    #[strum(serialize = "openai")]
    OpenAi,
    Google,
    Copilot,
    Ollama,
    LlamaCpp,
    Mistral,
    Zai,
    #[strum(serialize = "deepseek")]
    DeepSeek,
    #[strum(serialize = "openrouter")]
    OpenRouter,
    Synthetic,
    #[strum(serialize = "tensorx")]
    TensorX,
    #[strum(serialize = "opencode")]
    Opencode,
    #[strum(serialize = "xai")]
    Xai,
    Aperture,
    #[strum(serialize = "command-code")]
    CommandCode,
}

impl ProviderKind {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
            Self::Google => "Google",
            Self::Copilot => "Copilot",
            Self::Ollama => "Ollama",
            Self::LlamaCpp => "LlamaCpp",
            Self::Mistral => "Mistral",
            Self::Zai => "Z.AI",
            Self::DeepSeek => "DeepSeek",
            Self::OpenRouter => "OpenRouter",
            Self::Synthetic => "Synthetic",
            Self::TensorX => "TensorX",
            Self::Opencode => "Opencode Zen",
            Self::Xai => "xAI",
            Self::Aperture => "Aperture",
            Self::CommandCode => "Command Code",
        }
    }

    pub const fn api_key_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
            Self::Google => "GEMINI_API_KEY",
            Self::Copilot => "GH_COPILOT_TOKEN",
            Self::Ollama => "OLLAMA_API_KEY",
            Self::LlamaCpp => "LLAMA_CPP_API_KEY",
            Self::Mistral => "MISTRAL_API_KEY",
            Self::Zai => "ZHIPU_API_KEY",
            Self::DeepSeek => "DEEPSEEK_API_KEY",
            Self::OpenRouter => "OPENROUTER_API_KEY",
            Self::Synthetic => "SYNTHETIC_API_KEY",
            Self::TensorX => "TENSORX_API_KEY",
            Self::Opencode => "OPENCODE_API_KEY",
            Self::Xai => "XAI_API_KEY",
            Self::Aperture => "",
            Self::CommandCode => "COMMAND_CODE_API_KEY",
        }
    }

    pub const fn base_url(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com/v1/messages",
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Google => "https://generativelanguage.googleapis.com/v1beta",
            Self::Copilot => {
                "https://api.githubcopilot.com (or GraphQL-discovered Copilot API endpoint)"
            }
            Self::Ollama => "http://localhost:11434/v1",
            Self::LlamaCpp => "http://localhost:8080/v1",
            Self::Mistral => "https://api.mistral.ai/v1",
            Self::Zai => "https://api.z.ai/api/paas/v4",
            Self::DeepSeek => "https://api.deepseek.com",
            Self::OpenRouter => "https://openrouter.ai/api/v1",
            Self::Synthetic => "https://api.synthetic.new/openai/v1",
            Self::TensorX => "https://api.tensorx.ai/v1",
            Self::Opencode => "https://opencode.ai/zen/v1",
            Self::Xai => "https://api.x.ai/v1",
            Self::Aperture => "Aperture gateway (set APERTURE_HOST)",
            Self::CommandCode => "https://api.commandcode.ai",
        }
    }

    pub const fn features(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => {
                Some("Prompt caching, thinking mode (adaptive/budgeted), advanced tool use")
            }
            Self::Google => Some("Native Gemini API with thinking support"),
            Self::Copilot => Some("Native Copilot Chat HTTP API with model endpoint discovery"),
            Self::Ollama => {
                Some("Local or remote inference via OLLAMA_HOST, cloud fallback via OLLAMA_API_KEY")
            }
            Self::LlamaCpp => Some(
                "Local or remote inference via LLAMA_CPP_HOST, set optional key via LLAMA_CPP_API_KEY",
            ),
            Self::Synthetic => {
                Some("Reasoning effort support (low/medium/high), open-weight models")
            }
            Self::TensorX => Some("Open-weight models, zero data retention, prompt caching"),
            Self::DeepSeek => Some("Thinking mode toggle (on/off), open-weight models"),
            Self::OpenRouter => {
                Some("300+ models from all providers, prompt caching, provider routing")
            }
            Self::Opencode => Some(
                "Dynamically discovered models via [models.dev](https://models.dev/) + all the models provided by Opencode Zen API",
            ),
            Self::Xai => Some(
                "OAuth login, account-specific model catalog, Grok reasoning (low/medium/high/xhigh)",
            ),
            Self::Aperture => Some(
                "Tailscale Aperture LLM gateway; set APERTURE_HOST or configure in providers.toml",
            ),
            Self::CommandCode => Some(
                "Token-plan (GOAT/Pro/Max/Team) access to the whole Command Code catalog, per-model reasoning effort",
            ),
            _ => None,
        }
    }

    pub const fn family(self) -> ModelFamily {
        match self {
            Self::Anthropic => ModelFamily::Claude,
            Self::OpenAi => ModelFamily::Gpt,
            Self::Google => ModelFamily::Gemini,
            Self::Copilot => ModelFamily::Generic,
            Self::Ollama => ModelFamily::Generic,
            Self::LlamaCpp => ModelFamily::Generic,
            Self::Mistral => ModelFamily::Generic,
            Self::Zai => ModelFamily::Glm,
            Self::DeepSeek => ModelFamily::Generic,
            Self::OpenRouter => ModelFamily::Generic,
            Self::Synthetic => ModelFamily::Synthetic,
            Self::TensorX => ModelFamily::Generic,
            Self::Opencode => ModelFamily::Generic,
            Self::Xai => ModelFamily::Generic,
            Self::Aperture => ModelFamily::Generic,
            Self::CommandCode => ModelFamily::Generic,
        }
    }

    /// `None` when we honestly don't know the output window: llama.cpp
    /// serves whatever model the user loaded, and TensorX rejects explicit
    /// max_tokens (see tensorx.rs). Unknown means "don't limit", never
    /// "assume small"; a `0` sentinel here once silently capped llama.cpp
    /// thinking budgets at the floor.
    pub const fn fallback_max_output(self) -> Option<u32> {
        match self {
            Self::Anthropic => Some(128_000),
            Self::OpenAi => Some(100_000),
            Self::Google => Some(65_536),
            Self::Copilot => Some(100_000),
            Self::Ollama => Some(16_384),
            Self::LlamaCpp => None,
            Self::Mistral => None,
            Self::Zai => Some(16_000),
            Self::DeepSeek => Some(384_000),
            Self::OpenRouter => Some(128_000),
            Self::Synthetic => Some(32_000),
            Self::TensorX => None,
            Self::Opencode => Some(128_000),
            Self::Xai => Some(131_072),
            Self::Aperture => Some(16_384),
            Self::CommandCode => Some(64_000),
        }
    }

    pub const fn fallback_context_window(self) -> u32 {
        match self {
            Self::Anthropic => 200_000,
            Self::OpenAi => 200_000,
            Self::Google => 1_000_000,
            Self::Copilot => 200_000,
            Self::Ollama => 128_000,
            Self::LlamaCpp => 128_000,
            Self::Mistral => 128_000,
            Self::Zai => 128_000,
            Self::DeepSeek => 1_000_000,
            Self::OpenRouter => 200_000,
            Self::Synthetic => 128_000,
            Self::TensorX => 200_000,
            Self::Opencode => 256_000,
            Self::Xai => 500_000,
            Self::Aperture => 128_000,
            Self::CommandCode => 200_000,
        }
    }

    pub fn create(self, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
        match self {
            Self::Anthropic => {
                if bedrock::is_enabled() {
                    Ok(Box::new(bedrock::Bedrock::new(timeouts)?))
                } else {
                    Ok(Box::new(Anthropic::new(timeouts)?))
                }
            }
            Self::OpenAi => Ok(Box::new(OpenAi::new(timeouts)?)),
            Self::Google => Ok(Box::new(Google::new(timeouts)?)),
            Self::Copilot => Ok(Box::new(Copilot::new(timeouts)?)),
            Self::Ollama => Ok(Box::new(LocalEndpoint::new(&OLLAMA, timeouts)?)),
            Self::LlamaCpp => Ok(Box::new(LocalEndpoint::new(&LLAMACPP, timeouts)?)),
            Self::Mistral => Ok(Box::new(Mistral::new(timeouts)?)),
            Self::Zai => Ok(Box::new(Zai::new(timeouts)?)),
            Self::DeepSeek => Ok(Box::new(DeepSeek::new(timeouts)?)),
            Self::OpenRouter => Ok(Box::new(OpenRouter::new(timeouts)?)),
            Self::Synthetic => Ok(Box::new(Synthetic::new(timeouts)?)),
            Self::TensorX => Ok(Box::new(TensorX::new(timeouts)?)),
            Self::Opencode => Ok(Box::new(Opencode::new(timeouts)?)),
            Self::Xai => Ok(Box::new(Xai::new(timeouts)?)),
            Self::Aperture => Ok(Box::new(Aperture::new(timeouts)?)),
            Self::CommandCode => Ok(Box::new(CommandCode::new(timeouts)?)),
        }
    }
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait Provider: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>>;

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>>;

    /// Fetch provider-side usage quota (remaining percentage / reset times).
    /// `Ok(None)` means the provider does not expose a programmatic usage endpoint.
    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async { Ok(None) })
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async { Ok(()) })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async { Ok(()) })
    }

    fn rotate_key(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async { Ok(false) })
    }

    fn adjust_model(&self, _model: &mut Model) {}
}

pub fn provider_for_slug(slug: &str, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    if let Ok(kind) = ProviderKind::from_str(slug) {
        return kind.create(timeouts);
    }
    if dynamic::display_name(slug).is_some() {
        return dynamic::create(slug, timeouts);
    }
    if crate::providers::custom::base_kind(slug).is_some() {
        return crate::providers::custom::create(slug, timeouts);
    }
    if let Some(catalog) = crate::providers::catalog::try_create(slug, timeouts) {
        return catalog;
    }
    Err(AgentError::Config {
        message: format!("unknown provider '{slug}'"),
    })
}

pub fn provider_available(slug: &str) -> bool {
    provider_for_slug(slug, Timeouts::default()).is_ok()
}

/// Non-blocking variant of [`provider_available`] for offline model discovery:
/// catalog-backed slugs consult only the already-warm catalog, so a cold cache
/// reports them unavailable instead of blocking on a network fetch.
fn provider_available_offline(slug: &str) -> bool {
    if ProviderKind::from_str(slug).is_ok()
        || dynamic::display_name(slug).is_some()
        || crate::providers::custom::base_kind(slug).is_some()
    {
        return provider_available(slug);
    }
    available_if_warm(slug)
}

pub fn from_model(model: &mut Model, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    let provider = provider_for_slug(&model.provider, timeouts)?;
    provider.adjust_model(model);
    debug!(provider = %model.provider, model = %model.id, "provider created");
    Ok(provider)
}

/// Adjust a model against its provider's static table without retaining the
/// provider. Used to reconcile a resumed model so it matches one started
/// fresh (e.g. inherited thinking support for a routed Aperture model).
pub fn adjust_model(model: &mut Model, timeouts: Timeouts) -> Result<(), AgentError> {
    // Script-backed providers adjust nothing but run their auth script at
    // construction; resumed-session callers sit on the UI thread and must
    // not wait on that.
    if dynamic::display_name(&model.provider).is_some() {
        return Ok(());
    }
    provider_for_slug(&model.provider, timeouts)?.adjust_model(model);
    Ok(())
}

pub fn from_model_fallback(model: &mut Model, timeouts: Timeouts) -> Box<dyn Provider> {
    match from_model(model, timeouts) {
        Ok(provider) => provider,
        Err(e) => {
            warn!(error = %e, "provider creation failed, using unconfigured provider");
            Box::new(UnconfiguredProvider)
        }
    }
}

struct UnconfiguredProvider;

const NOT_CONFIGURED: &str = "no provider configured — run /login or `maki auth login`";

impl Provider for UnconfiguredProvider {
    fn stream_message<'a>(
        &'a self,
        _model: &'a Model,
        _messages: &'a [Message],
        _system: &'a str,
        _tools: &'a Value,
        _event_tx: &'a Sender<ProviderEvent>,
        _opts: RequestOptions,
        _session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async {
            Err(AgentError::Config {
                message: NOT_CONFIGURED.to_string(),
            })
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async {
            Err(AgentError::Config {
                message: NOT_CONFIGURED.to_string(),
            })
        })
    }
}

pub async fn from_model_async(
    model: &mut Model,
    timeouts: Timeouts,
) -> Result<Box<dyn Provider>, AgentError> {
    let slug = Arc::clone(&model.provider);
    let id = model.id.clone();
    let provider = smol::unblock(move || provider_for_slug(&slug, timeouts)).await?;
    provider.adjust_model(model);
    debug!(provider = %model.provider, model = %id, "provider created");
    Ok(provider)
}

pub struct ModelBatch {
    pub models: Vec<String>,
    pub warnings: Vec<String>,
}

/// Offline version of model discovery: returns specs from static tables
/// and configured dynamic providers. See [`fetch_all_models`] for live lookups.
/// Never blocks on catalog download; catalog-backed providers appear only once
/// the catalog has warmed in the background.
pub fn available_model_specs(policy: &ModelPolicy) -> Vec<String> {
    let mut specs: Vec<String> = crate::manifest::ManifestRegistry::builtins()
        .iter()
        .filter(|m| provider_available_offline(m.slug))
        .flat_map(|m| {
            m.models
                .iter()
                .flat_map(|entry| entry.prefixes.iter())
                .map(move |p| format!("{}/{}", m.slug, p))
        })
        .collect();
    for slug in dynamic::discovered_slugs() {
        specs.extend(dynamic::dynamic_model_specs_for(slug));
    }
    for spec in crate::providers::custom::declared_model_specs() {
        if !specs.contains(&spec) {
            specs.push(spec);
        }
    }
    if let Some(catalog) = catalog_providers_if_available() {
        for cat in catalog {
            if ProviderKind::from_str(&cat.slug).is_ok()
                || dynamic::base_for_slug(&cat.slug).is_some()
                || crate::providers::custom::base_kind(&cat.slug).is_some()
                || OPENCODE_FAMILY_SLUGS.contains(&cat.slug.as_str())
            {
                continue;
            }
            if !provider_available(&cat.slug) {
                continue;
            }
            for model_id in cat.models.keys() {
                let spec = format!("{}/{}", cat.slug, model_id);
                if !specs.contains(&spec) {
                    specs.push(spec);
                }
            }
        }
    }
    specs.retain(|spec| policy.allows(spec));
    specs
}

pub async fn fetch_all_models(
    policy: &ModelPolicy,
    mut on_ready: impl FnMut(ModelBatch),
    on_done: Option<Box<dyn FnOnce() + Send>>,
) {
    let (tx, rx) = flume::unbounded();
    let timeouts = Timeouts::default();

    for manifest in crate::manifest::ManifestRegistry::builtins() {
        let slug = manifest.slug;
        let Ok(provider) = smol::unblock(move || provider_for_slug(slug, timeouts)).await else {
            warn!(provider = slug, "failed to create provider, skipping");
            continue;
        };
        let display_name = manifest.display_name;
        let tx = tx.clone();
        smol::spawn(async move {
            let batch = match provider.list_models().await {
                Ok(models) => {
                    let mut specs: Vec<String> =
                        models.iter().map(|m| format!("{slug}/{}", m.id)).collect();
                    crate::model_registry::set_known_models(slug, models);
                    for entry in manifest.models {
                        for prefix in entry.prefixes {
                            let spec = format!("{slug}/{prefix}");
                            if !specs.contains(&spec) {
                                specs.push(spec);
                            }
                        }
                    }
                    ModelBatch {
                        models: specs,
                        warnings: Vec::new(),
                    }
                }
                Err(e) => {
                    warn!(provider = slug, error = %e, "failed to list models, using static fallback");
                    let fallback: Vec<String> = manifest
                        .models
                        .iter()
                        .flat_map(|entry| entry.prefixes.iter())
                        .map(|p| format!("{slug}/{p}"))
                        .collect();
                    ModelBatch {
                        models: fallback,
                        warnings: vec![format!(
                            "{display_name}: {e} (using static fallback)"
                        )],
                    }
                }
            };
            let _ = tx.send_async(batch).await;
        })
        .detach();
    }

    for slug in dynamic::discovered_slugs() {
        let tx = tx.clone();
        let slug = slug.to_string();
        smol::spawn(async move {
            let static_fallback = |reason: String| {
                warn!(
                    slug,
                    error = reason,
                    "dynamic model listing failed, using static fallback"
                );
                ModelBatch {
                    models: dynamic::dynamic_model_specs_for(&slug),
                    warnings: vec![format!("{slug}: {reason} (using static fallback)")],
                }
            };
            let batch = match dynamic::create(&slug, timeouts) {
                Ok(provider) => match provider.list_models().await {
                    Ok(models) => ModelBatch {
                        models: models.iter().map(|m| format!("{slug}/{}", m.id)).collect(),
                        warnings: Vec::new(),
                    },
                    Err(e) => static_fallback(e.to_string()),
                },
                Err(e) => static_fallback(e.to_string()),
            };
            let _ = tx.send_async(batch).await;
        })
        .detach();
    }

    let tx_catalog = tx.clone();
    smol::spawn(async move {
        let catalog = smol::unblock(catalog_providers).await;
        for cat in catalog {
            if ProviderKind::from_str(&cat.slug).is_ok()
                || dynamic::base_for_slug(&cat.slug).is_some()
                || OPENCODE_FAMILY_SLUGS.contains(&cat.slug.as_str())
            {
                continue;
            }
            if !provider_available(&cat.slug) {
                continue;
            }
            let slug = cat.slug;
            let models: Vec<String> = cat.models.keys().map(|id| format!("{slug}/{id}")).collect();
            let _ = tx_catalog
                .send_async(ModelBatch {
                    models,
                    warnings: Vec::new(),
                })
                .await;
        }
    })
    .detach();

    let custom_timeouts = timeouts;
    let tx_custom = tx.clone();
    smol::spawn(async move {
        let declared = crate::providers::custom::declared_model_specs();
        if !declared.is_empty() {
            let _ = tx_custom
                .send_async(ModelBatch {
                    models: declared,
                    warnings: Vec::new(),
                })
                .await;
        }
        let custom_specs =
            smol::unblock(move || crate::providers::custom::discover_models(custom_timeouts)).await;
        if !custom_specs.is_empty() {
            let _ = tx_custom
                .send_async(ModelBatch {
                    models: custom_specs,
                    warnings: Vec::new(),
                })
                .await;
        }
    })
    .detach();

    drop(tx);

    while let Ok(mut batch) = rx.recv_async().await {
        batch.models.retain(|spec| policy.allows(spec));
        on_ready(batch);
    }
    if let Some(done) = on_done {
        done();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allowed: &[&str], excluded: &[&str]) -> ModelPolicy {
        ModelPolicy::new(
            &allowed
                .iter()
                .map(|pattern| (*pattern).into())
                .collect::<Vec<_>>(),
            &excluded
                .iter()
                .map(|pattern| (*pattern).into())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn available_specs_apply_model_policy() {
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-test-model-policy") };
        let policy = policy(&["openai/*"], &["*/gpt-5.6-terra"]);

        let specs = available_model_specs(&policy);
        unsafe { std::env::remove_var("OPENAI_API_KEY") };

        assert!(!specs.is_empty());
        assert!(specs.iter().all(|spec| spec.starts_with("openai/")));
        assert!(!specs.iter().any(|spec| spec == "openai/gpt-5.6-terra"));
    }

    #[test]
    fn provider_for_slug_unknown_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let _catalog = crate::providers::catalog::warm_empty_catalog_for_tests(
            maki_storage::StateDir::from_path(tmp.path().to_path_buf()),
        );
        let result = provider_for_slug("nonexistent-provider-xyz", Timeouts::default());
        match result {
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("unknown provider"),
                    "expected 'unknown provider' message, got: {msg}"
                );
            }
            Ok(_) => panic!("expected error for unknown provider"),
        }
    }
}
