use std::future::Future;
use std::pin::Pin;

use flume::Sender;
use serde_json::Value;
use strum::{Display, EnumIter, EnumString, IntoEnumIterator};
use tracing::{debug, warn};

use crate::model::{Model, ModelFamily, models_for_provider};
use crate::providers::Timeouts;
use crate::providers::anthropic::Anthropic;
use crate::providers::dynamic;
use crate::providers::google::Google;
use crate::providers::mistral::Mistral;
use crate::providers::ollama::Ollama;
use crate::providers::openai::OpenAi;
use crate::providers::synthetic::Synthetic;
use crate::providers::zai::{Zai, ZaiPlan};
use crate::{AgentError, Message, ProviderEvent, StreamResponse, ThinkingConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, EnumIter)]
#[strum(serialize_all = "kebab-case")]
pub enum ProviderKind {
    Anthropic,
    #[strum(serialize = "openai")]
    OpenAi,
    Google,
    Ollama,
    Mistral,
    Zai,
    ZaiCodingPlan,
    Synthetic,
}

impl ProviderKind {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
            Self::Google => "Google",
            Self::Ollama => "Ollama",
            Self::Mistral => "Mistral",
            Self::Zai => "Z.AI",
            Self::ZaiCodingPlan => "Z.AI Coding",
            Self::Synthetic => "Synthetic",
        }
    }

    pub const fn api_key_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
            Self::Google => "GEMINI_API_KEY",
            Self::Ollama => "OLLAMA_API_KEY",
            Self::Mistral => "MISTRAL_API_KEY",
            Self::Zai | Self::ZaiCodingPlan => "ZHIPU_API_KEY",
            Self::Synthetic => "SYNTHETIC_API_KEY",
        }
    }

    pub const fn base_url(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com/v1/messages",
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Google => "https://generativelanguage.googleapis.com/v1beta",
            Self::Ollama => "http://localhost:11434/v1",
            Self::Mistral => "https://api.mistral.ai/v1",
            Self::Zai => "https://api.z.ai/api/paas/v4",
            Self::ZaiCodingPlan => "https://api.z.ai/api/coding/paas/v4",
            Self::Synthetic => "https://api.synthetic.new/openai/v1",
        }
    }

    pub const fn supports_thinking(self) -> bool {
        matches!(
            self,
            Self::Anthropic | Self::Google | Self::Mistral | Self::Synthetic
        )
    }

    pub const fn features(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => {
                Some("Prompt caching, thinking mode (adaptive/budgeted), advanced tool use")
            }
            Self::Google => Some("Native Gemini API with thinking support"),
            Self::Ollama => Some("Local inference via OLLAMA_HOST, or cloud via OLLAMA_API_KEY"),
            Self::Synthetic => {
                Some("Reasoning effort support (low/medium/high), open-weight models")
            }
            _ => None,
        }
    }

    pub const fn family(self) -> ModelFamily {
        match self {
            Self::Anthropic => ModelFamily::Claude,
            Self::OpenAi => ModelFamily::Gpt,
            Self::Google => ModelFamily::Gemini,
            Self::Ollama => ModelFamily::Generic,
            Self::Mistral => ModelFamily::Generic,
            Self::Zai | Self::ZaiCodingPlan => ModelFamily::Glm,
            Self::Synthetic => ModelFamily::Synthetic,
        }
    }

    pub const fn accepts_arbitrary_models(self) -> bool {
        matches!(self, Self::Ollama | Self::Google)
    }

    pub fn create(self, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
        match self {
            Self::Anthropic => Ok(Box::new(Anthropic::new(timeouts)?)),
            Self::OpenAi => Ok(Box::new(OpenAi::new(timeouts)?)),
            Self::Google => Ok(Box::new(Google::new(timeouts)?)),
            Self::Ollama => Ok(Box::new(Ollama::new(timeouts)?)),
            Self::Mistral => Ok(Box::new(Mistral::new(timeouts)?)),
            Self::Zai => Ok(Box::new(Zai::new(ZaiPlan::Standard, timeouts)?)),
            Self::ZaiCodingPlan => Ok(Box::new(Zai::new(ZaiPlan::Coding, timeouts)?)),
            Self::Synthetic => Ok(Box::new(Synthetic::new(timeouts)?)),
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
        thinking: ThinkingConfig,
        session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>>;

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>>;

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async { Ok(()) })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async { Ok(()) })
    }
}

pub fn from_model(model: &Model, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    if let Some(slug) = &model.dynamic_slug {
        let provider = dynamic::create(slug, timeouts)?;
        debug!(slug, model = %model.id, "dynamic provider created");
        return Ok(provider);
    }
    let provider = model.provider.create(timeouts)?;
    debug!(provider = %model.provider, model = %model.id, "provider created");
    Ok(provider)
}

pub async fn from_model_async(
    model: &Model,
    timeouts: Timeouts,
) -> Result<Box<dyn Provider>, AgentError> {
    let slug = model.dynamic_slug.clone();
    let kind = model.provider;
    let id = model.id.clone();
    let provider = smol::unblock(move || {
        if let Some(slug) = &slug {
            dynamic::create(slug, timeouts)
        } else {
            kind.create(timeouts)
        }
    })
    .await?;
    debug!(provider = %kind, model = %id, "provider created");
    Ok(provider)
}

pub struct ModelBatch {
    pub models: Vec<String>,
    pub warnings: Vec<String>,
}

pub async fn fetch_all_models(mut on_ready: impl FnMut(ModelBatch)) {
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
                Ok(ids) => {
                    if kind.accepts_arbitrary_models() {
                        crate::tier_map::tier_map()
                            .write()
                            .unwrap()
                            .set_known_models(kind, ids.clone());
                    }
                    ModelBatch {
                        models: ids.into_iter().map(|id| format!("{kind}/{id}")).collect(),
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

    let dynamic_specs = dynamic::dynamic_model_specs();
    if !dynamic_specs.is_empty() {
        let _ = tx.send(ModelBatch {
            models: dynamic_specs,
            warnings: Vec::new(),
        });
    }

    drop(tx);

    while let Ok(batch) = rx.recv_async().await {
        on_ready(batch);
    }
}
