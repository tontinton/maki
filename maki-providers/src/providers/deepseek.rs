use std::sync::{Arc, Mutex};

use flume::Sender;
use serde_json::Value;
use tracing::warn;

use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, StreamResponse, ThinkingConfig};

use super::ResolvedAuth;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "DEEPSEEK_API_KEY",
    base_url: "https://api.deepseek.com",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "DeepSeek",
};

pub(crate) fn models() -> &'static [ModelEntry] {
    &[
        ModelEntry {
            prefixes: &["deepseek-v4-flash"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            default: true,
            pricing: ModelPricing {
                input: 0.14,
                output: 0.28,
                cache_write: 0.00,
                cache_read: 0.00,
            },
            max_output_tokens: 384_000,
            context_window: 1_000_000,
        },
        ModelEntry {
            prefixes: &["deepseek-v4-pro"],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            default: true,
            pricing: ModelPricing {
                input: 0.435,
                output: 0.87,
                cache_write: 0.00,
                cache_read: 0.00,
            },
            max_output_tokens: 384_000,
            context_window: 1_000_000,
        },
    ]
}

pub struct DeepSeek {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    system_prefix: Option<String>,
}

impl DeepSeek {
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

impl Provider for DeepSeek {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        thinking: ThinkingConfig,
        _session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let mut body = self.compat.build_body(model, messages, system, tools);

            // DeepSeek enables reasoning by default; Adaptive and Budget
            // use the model's default reasoning behavior.
            match thinking {
                ThinkingConfig::Off => {
                    body["thinking"] = serde_json::json!({"type": "disabled"});
                }
                ThinkingConfig::Adaptive => {
                    body["thinking"] = serde_json::json!({"type": "enabled"});
                }
                ThinkingConfig::Budget(_n) => {
                    body["thinking"] = serde_json::json!({"type": "enabled"});
                    body["reasoning_effort"] = serde_json::json!("max");
                    warn!("DeepSeek reasoning does not support token budgets");
                }
            }

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
