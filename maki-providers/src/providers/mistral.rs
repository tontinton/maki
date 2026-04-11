use flume::Sender;
use serde_json::Value;

use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, StreamResponse, ThinkingConfig};

use super::ResolvedAuth;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "MISTRAL_API_KEY",
    base_url: "https://api.mistral.ai/v1",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "Mistral",
};

pub(crate) fn models() -> &'static [ModelEntry] {
    &[
        ModelEntry {
            prefixes: &["devstral-latest", "devstral-medium-latest", "devstral-2512"],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            default: true,
            pricing: ModelPricing {
                input: 0.4,
                output: 2.0,
                cache_write: 0.00,
                cache_read: 0.00,
            },
            max_output_tokens: 262_144,
            context_window: 262_144,
        },
        ModelEntry {
            prefixes: &["mistral-large-latest", "mistral-large-2512"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            default: true,
            pricing: ModelPricing {
                input: 0.5,
                output: 1.5,
                cache_write: 0.00,
                cache_read: 0.00,
            },
            max_output_tokens: 262_144,
            context_window: 262_144,
        },
        ModelEntry {
            prefixes: &["mistral-small-latest", "mistral-small-2603"],
            tier: ModelTier::Weak,
            family: ModelFamily::Generic,
            default: true,
            pricing: ModelPricing {
                input: 0.15,
                output: 0.60,
                cache_write: 0.00,
                cache_read: 0.00,
            },
            max_output_tokens: 262_144,
            context_window: 262_144,
        },
    ]
}

pub struct Mistral {
    compat: OpenAiCompatProvider,
    auth: ResolvedAuth,
}

impl Mistral {
    pub fn new() -> Result<Self, AgentError> {
        let api_key = std::env::var(CONFIG.api_key_env).map_err(|_| AgentError::Config {
            message: format!("{} not set", CONFIG.api_key_env),
        })?;
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG),
            auth: ResolvedAuth::bearer(&api_key),
        })
    }
}

impl Provider for Mistral {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        _thinking: ThinkingConfig,
        session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let body = self.compat.build_body(model, messages, system, tools);
            let mut extra_headers = vec![];
            if let Some(session_id) = session_id {
                extra_headers.push(("x-affinity".to_string(), session_id.to_string()));
            }
            self.compat
                .do_stream(model, &extra_headers, &body, event_tx, &self.auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(self.compat.do_list_models(&self.auth))
    }
}
