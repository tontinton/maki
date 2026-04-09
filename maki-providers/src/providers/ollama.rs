use flume::Sender;
use serde_json::Value;

use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, StreamResponse, ThinkingConfig};

use super::ResolvedAuth;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

pub(crate) const DEFAULT_MAX_OUTPUT: u32 = 16384;
pub(crate) const DEFAULT_CONTEXT: u32 = 128_000;

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "",
    base_url: "http://localhost:11434/v1",
    max_tokens_field: "max_tokens",
    include_stream_usage: false,
    provider_name: "Ollama",
};

pub(crate) fn models() -> &'static [ModelEntry] {
    &[
        ModelEntry {
            prefixes: &["qwen3:1.7b"],
            tier: ModelTier::Weak,
            family: ModelFamily::Generic,
            default: true,
            pricing: ModelPricing::ZERO,
            max_output_tokens: DEFAULT_MAX_OUTPUT,
            context_window: DEFAULT_CONTEXT,
        },
        ModelEntry {
            prefixes: &["qwen3:8b"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            default: true,
            pricing: ModelPricing::ZERO,
            max_output_tokens: DEFAULT_MAX_OUTPUT,
            context_window: DEFAULT_CONTEXT,
        },
        ModelEntry {
            prefixes: &["qwen3"],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            default: true,
            pricing: ModelPricing::ZERO,
            max_output_tokens: DEFAULT_MAX_OUTPUT,
            context_window: DEFAULT_CONTEXT,
        },
    ]
}

pub struct Ollama {
    compat: OpenAiCompatProvider,
    auth: ResolvedAuth,
}

impl Ollama {
    pub fn new() -> Self {
        let base_url = std::env::var("OLLAMA_HOST")
            .ok()
            .map(|host| format!("{host}/v1"));
        Self {
            compat: OpenAiCompatProvider::new(&CONFIG),
            auth: ResolvedAuth {
                base_url,
                headers: Vec::new(),
            },
        }
    }
}

impl Provider for Ollama {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        _thinking: ThinkingConfig,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let body = self.compat.build_body(model, messages, system, tools);
            self.compat
                .do_stream(model, &body, event_tx, &self.auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(self.compat.do_list_models(&self.auth))
    }
}
