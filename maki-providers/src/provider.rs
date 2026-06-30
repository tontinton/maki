use std::future::Future;
use std::pin::Pin;

use flume::Sender;
use serde_json::Value;
use strum::{Display, EnumIter, EnumString, IntoEnumIterator};
use tracing::{debug, warn};

use crate::model::{Model, ModelFamily, ModelInfo, models_for_provider};
use crate::providers::Timeouts;
use crate::providers::anthropic::Anthropic;
use crate::providers::anthropic::bedrock;
use crate::providers::copilot::Copilot;
use crate::providers::deepseek::DeepSeek;
use crate::providers::dynamic;
use crate::providers::google::Google;
use crate::providers::local::{LLAMACPP, LocalEndpoint, OLLAMA};
use crate::providers::mistral::Mistral;
use crate::providers::openai::OpenAi;
use crate::providers::openrouter::OpenRouter;
use crate::providers::synthetic::Synthetic;
use crate::providers::tensorx::TensorX;
use crate::providers::zai::Zai;
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse};

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
        }
    }

    pub const fn reasoning(self) -> crate::model::ReasoningSupport {
        use crate::model::ReasoningSupport;
        match self {
            Self::Anthropic => ReasoningSupport::Anthropic,
            Self::OpenAi
            | Self::Google
            | Self::Mistral
            | Self::DeepSeek
            | Self::Synthetic
            | Self::OpenRouter
            | Self::LlamaCpp
            | Self::TensorX => ReasoningSupport::OpenAiEffort,
            Self::Copilot | Self::Ollama | Self::Zai => ReasoningSupport::None,
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
        }
    }

    pub const fn accepts_arbitrary_models(self) -> bool {
        matches!(
            self,
            Self::Ollama
                | Self::LlamaCpp
                | Self::Google
                | Self::Copilot
                | Self::OpenRouter
                | Self::TensorX
                | Self::Mistral
        )
    }

    pub const fn fallback_max_output(self) -> u32 {
        match self {
            Self::Anthropic => 128_000,
            Self::OpenAi => 100_000,
            Self::Google => 65_536,
            Self::Copilot => 100_000,
            Self::Ollama => 16_384,
            Self::LlamaCpp => 0,
            Self::Mistral => 32_000,
            Self::Zai => 16_000,
            Self::DeepSeek => 384_000,
            Self::OpenRouter => 128_000,
            Self::Synthetic => 32_000,
            // FIXME: See comment in tensorx.rs
            Self::TensorX => 0,
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
        }
    }

    pub fn is_available(self) -> bool {
        self.create(Timeouts::default()).is_ok()
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
        session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>>;

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>>;

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

fn provider_for_slug(slug: &str, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    if dynamic::display_name(slug).is_some() {
        dynamic::create(slug, timeouts)
    } else {
        crate::providers::custom::create(slug, timeouts)
    }
}

pub fn from_model(model: &mut Model, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    if let Some(slug) = &model.dynamic_slug {
        debug!(slug, model = %model.id, "slug provider created");
        return provider_for_slug(slug, timeouts);
    }
    let provider = model.provider.create(timeouts)?;
    provider.adjust_model(model);
    debug!(provider = %model.provider, model = %model.id, "provider created");
    Ok(provider)
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
        _session_id: Option<&'a str>,
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
    let slug = model.dynamic_slug.clone();
    let kind = model.provider;
    let id = model.id.clone();
    let provider = smol::unblock(move || {
        if let Some(slug) = &slug {
            provider_for_slug(slug, timeouts)
        } else {
            kind.create(timeouts)
        }
    })
    .await?;
    if model.dynamic_slug.is_none() {
        provider.adjust_model(model);
    }
    debug!(provider = %kind, model = %id, "provider created");
    Ok(provider)
}

pub struct ModelBatch {
    pub models: Vec<String>,
    pub warnings: Vec<String>,
}

/// Offline version of model discovery: returns specs from static tables
/// and configured dynamic providers. See [`fetch_all_models`] for live lookups.
pub fn available_model_specs() -> Vec<String> {
    let mut specs: Vec<String> = ProviderKind::iter()
        .filter(|kind| kind.is_available())
        .flat_map(|kind| {
            models_for_provider(kind)
                .iter()
                .flat_map(|entry| entry.prefixes.iter())
                .map(move |p| format!("{kind}/{p}"))
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
    specs
}

pub async fn fetch_all_models(
    mut on_ready: impl FnMut(ModelBatch),
    on_done: Option<Box<dyn FnOnce() + Send>>,
) {
    let (tx, rx) = flume::unbounded();
    let timeouts = Timeouts::default();

    for kind in ProviderKind::iter() {
        let Ok(provider) = smol::unblock(move || kind.create(timeouts)).await else {
            warn!(provider = %kind, "failed to create provider, skipping");
            continue;
        };
        let tx = tx.clone();
        smol::spawn(async move {
            let batch = match provider.list_models().await {
                Ok(models) => {
                    if kind.accepts_arbitrary_models() {
                        crate::model_registry::model_registry()
                            .write()
                            .unwrap()
                            .set_known_models(kind, models.clone());
                    }
                    let mut specs: Vec<String> =
                        models.iter().map(|m| format!("{kind}/{}", m.id)).collect();
                    for entry in models_for_provider(kind) {
                        for prefix in entry.prefixes {
                            let spec = format!("{kind}/{prefix}");
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
                    warn!(provider = %kind, error = %e, "failed to list models, using static fallback");
                    let fallback: Vec<String> = models_for_provider(kind)
                        .iter()
                        .flat_map(|entry| entry.prefixes.iter())
                        .map(|p| format!("{kind}/{p}"))
                        .collect();
                    ModelBatch {
                        models: fallback,
                        warnings: vec![format!(
                            "{}: {e} (using static fallback)",
                            kind.display_name()
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

    while let Ok(batch) = rx.recv_async().await {
        on_ready(batch);
    }
    if let Some(done) = on_done {
        done();
    }
}
