use flume::Sender;
use serde_json::{Value, json};

use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, StreamResponse, ThinkingConfig};

use super::ResolvedAuth;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "SYNTHETIC_API_KEY",
    base_url: "https://api.synthetic.new/openai/v1",
    max_tokens_field: "max_completion_tokens",
    include_stream_usage: false,
    provider_name: "Synthetic",
};

pub(crate) fn models() -> &'static [ModelEntry] {
    &[
        ModelEntry {
            prefixes: &["hf:moonshotai/Kimi-K2.5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Synthetic,
            default: true,
            pricing: ModelPricing {
                input: 0.45,
                output: 3.40,
                cache_write: 0.00,
                cache_read: 0.00,
            },
            max_output_tokens: 131072,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["hf:deepseek-ai/DeepSeek-V3.2"],
            tier: ModelTier::Medium,
            family: ModelFamily::Synthetic,
            default: true,
            pricing: ModelPricing {
                input: 0.56,
                output: 1.68,
                cache_write: 0.00,
                cache_read: 0.00,
            },
            max_output_tokens: 131072,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["hf:zai-org/GLM-4.7-Flash"],
            tier: ModelTier::Weak,
            family: ModelFamily::Synthetic,
            default: true,
            pricing: ModelPricing {
                input: 0.10,
                output: 0.50,
                cache_write: 0.00,
                cache_read: 0.00,
            },
            max_output_tokens: 131072,
            context_window: 200_000,
        },
    ]
}

pub struct Synthetic {
    compat: OpenAiCompatProvider,
    auth: ResolvedAuth,
}

impl Synthetic {
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

impl Provider for Synthetic {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        thinking: ThinkingConfig,
        _session_id: Option<&str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let mut body = self.compat.build_body(model, messages, system, tools);
            match thinking {
                ThinkingConfig::Off => {}
                ThinkingConfig::Adaptive => {
                    body["reasoning_effort"] = json!("medium");
                }
                ThinkingConfig::Budget(n) => {
                    let effort = if n < 2048 {
                        "low"
                    } else if n < 8192 {
                        "medium"
                    } else {
                        "high"
                    };
                    body["reasoning_effort"] = json!(effort);
                }
            };
            self.compat
                .do_stream(model, &[], &body, event_tx, &self.auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(self.compat.do_list_models(&self.auth))
    }
}
