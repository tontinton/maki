use flume::Sender;
use serde_json::Value;
use tracing::warn;

use crate::model::Model;
use crate::model::{ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, StreamResponse, ThinkingConfig};

use super::ResolvedAuth;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

static CONFIG_STANDARD: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "ZHIPU_API_KEY",
    base_url: "https://api.z.ai/api/paas/v4",
    max_tokens_field: "max_tokens",
    include_stream_usage: false,
    provider_name: "Z.AI",
};

static CONFIG_CODING: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "ZHIPU_API_KEY",
    base_url: "https://api.z.ai/api/coding/paas/v4",
    max_tokens_field: "max_tokens",
    include_stream_usage: false,
    provider_name: "Z.AI Coding",
};

pub(crate) fn models() -> &'static [ModelEntry] {
    &[
        ModelEntry {
            prefixes: &["glm-5-code"],
            tier: ModelTier::Strong,
            family: ModelFamily::Glm,
            default: true,
            pricing: ModelPricing {
                input: 1.20,
                output: 5.00,
                cache_write: 0.00,
                cache_read: 0.30,
            },
            max_output_tokens: 131072,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["glm-5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Glm,
            default: false,
            pricing: ModelPricing {
                input: 1.00,
                output: 3.20,
                cache_write: 0.00,
                cache_read: 0.20,
            },
            max_output_tokens: 131072,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["glm-4.7-flash"],
            tier: ModelTier::Weak,
            family: ModelFamily::Glm,
            default: true,
            pricing: ModelPricing {
                input: 0.00,
                output: 0.00,
                cache_write: 0.00,
                cache_read: 0.00,
            },
            max_output_tokens: 131072,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["glm-4.7", "glm-4.6"],
            tier: ModelTier::Medium,
            family: ModelFamily::Glm,
            default: true,
            pricing: ModelPricing {
                input: 0.60,
                output: 2.20,
                cache_write: 0.00,
                cache_read: 0.11,
            },
            max_output_tokens: 131072,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["glm-4.5-flash"],
            tier: ModelTier::Weak,
            family: ModelFamily::Glm,
            default: false,
            pricing: ModelPricing {
                input: 0.00,
                output: 0.00,
                cache_write: 0.00,
                cache_read: 0.00,
            },
            max_output_tokens: 98304,
            context_window: 131_072,
        },
        ModelEntry {
            prefixes: &["glm-4.5-air"],
            tier: ModelTier::Weak,
            family: ModelFamily::Glm,
            default: false,
            pricing: ModelPricing {
                input: 0.20,
                output: 1.10,
                cache_write: 0.00,
                cache_read: 0.03,
            },
            max_output_tokens: 98304,
            context_window: 131_072,
        },
        ModelEntry {
            prefixes: &["glm-4.5"],
            tier: ModelTier::Medium,
            family: ModelFamily::Glm,
            default: false,
            pricing: ModelPricing {
                input: 0.60,
                output: 2.20,
                cache_write: 0.00,
                cache_read: 0.11,
            },
            max_output_tokens: 98304,
            context_window: 131_072,
        },
    ]
}

#[derive(Debug, Clone, Copy)]
pub enum ZaiPlan {
    Standard,
    Coding,
}

pub struct Zai {
    compat: OpenAiCompatProvider,
    auth: ResolvedAuth,
}

impl Zai {
    pub fn new(plan: ZaiPlan) -> Result<Self, AgentError> {
        let config = match plan {
            ZaiPlan::Standard => &CONFIG_STANDARD,
            ZaiPlan::Coding => &CONFIG_CODING,
        };
        let api_key = std::env::var(config.api_key_env).map_err(|_| AgentError::Config {
            message: format!("{} not set", config.api_key_env),
        })?;
        Ok(Self {
            compat: OpenAiCompatProvider::new(config),
            auth: ResolvedAuth::bearer(&api_key),
        })
    }
}

impl Provider for Zai {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        _thinking: ThinkingConfig,
        _session_id: Option<&str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let body = self.compat.build_body(model, messages, system, tools);
            match self
                .compat
                .do_stream(model, &[], &body, event_tx, &self.auth)
                .await
            {
                Err(AgentError::Api { status, message })
                    if (status == 429 || status >= 500)
                        && (message.contains("1113") || message.contains("nsufficien")) =>
                {
                    warn!(status, "insufficient funds, bailing out");
                    Err(AgentError::Api {
                        status: 402,
                        message,
                    })
                }
                result => result,
            }
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(self.compat.do_list_models(&self.auth))
    }
}
