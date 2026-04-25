use std::sync::{Arc, Mutex};

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
    auth: Arc<Mutex<ResolvedAuth>>,
    system_prefix: Option<String>,
}

impl Synthetic {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let api_key = std::env::var(CONFIG.api_key_env).map_err(|_| AgentError::Config {
            message: format!("{} not set", CONFIG.api_key_env),
        })?;
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth: Arc::new(Mutex::new(ResolvedAuth::bearer(&api_key))),
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: super::Timeouts) -> Self {
        Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth,
            system_prefix: None,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
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
            let auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
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
                .do_stream(model, &[], &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            self.compat.do_list_models(&auth).await
        })
    }
}
