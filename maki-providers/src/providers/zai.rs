use std::sync::Arc;

use flume::Sender;
use serde_json::Value;
use tracing::warn;

use crate::model::{Model, ModelEntry, ModelPricing, ModelTier};
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

fn glm(
    id: &str,
    tier: ModelTier,
    default: bool,
    pricing: (f64, f64, f64),
    limits: (u32, u32),
) -> ModelEntry {
    ModelEntry {
        id: id.into(),
        tier,
        default,
        pricing: ModelPricing {
            input: pricing.0,
            output: pricing.1,
            cache_write: 0.00,
            cache_read: pricing.2,
        },
        max_output_tokens: limits.0,
        context_window: limits.1,
        supports_thinking: false,
        supports_tool_examples: false,
        uses_responses_api: false,
    }
}

const GLM_LIMITS: (u32, u32) = (131072, 200_000);
const GLM_LIMITS_LEGACY: (u32, u32) = (98304, 131_072);

pub(crate) fn models() -> Arc<Vec<ModelEntry>> {
    static BUILT: std::sync::OnceLock<Arc<Vec<ModelEntry>>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            Arc::new(vec![
                glm(
                    "glm-5-code",
                    ModelTier::Strong,
                    true,
                    (1.20, 5.00, 0.30),
                    GLM_LIMITS,
                ),
                glm(
                    "glm-5",
                    ModelTier::Strong,
                    false,
                    (1.00, 3.20, 0.20),
                    GLM_LIMITS,
                ),
                glm(
                    "glm-4.7-flash",
                    ModelTier::Weak,
                    true,
                    (0.00, 0.00, 0.00),
                    GLM_LIMITS,
                ),
                glm(
                    "glm-4.7",
                    ModelTier::Medium,
                    true,
                    (0.60, 2.20, 0.11),
                    GLM_LIMITS,
                ),
                glm(
                    "glm-4.6",
                    ModelTier::Medium,
                    false,
                    (0.60, 2.20, 0.11),
                    GLM_LIMITS,
                ),
                glm(
                    "glm-4.5-flash",
                    ModelTier::Weak,
                    false,
                    (0.00, 0.00, 0.00),
                    GLM_LIMITS_LEGACY,
                ),
                glm(
                    "glm-4.5-air",
                    ModelTier::Weak,
                    false,
                    (0.20, 1.10, 0.03),
                    GLM_LIMITS_LEGACY,
                ),
                glm(
                    "glm-4.5",
                    ModelTier::Medium,
                    false,
                    (0.60, 2.20, 0.11),
                    GLM_LIMITS_LEGACY,
                ),
            ])
        })
        .clone()
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
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let body = self.compat.build_body(model, messages, system, tools);
            match self
                .compat
                .do_stream(model, &body, event_tx, &self.auth)
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
