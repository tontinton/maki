use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_config::providers::{BuiltInProvider, Protocol, ProviderPlan};
use serde_json::Value;
use tracing::warn;

use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ModelTier, ReasoningSupport};
use crate::provider::{BoxFuture, Provider};
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse};

use super::{KeyPool, ResolvedAuth};

static CONFIG_STANDARD: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "ZHIPU_API_KEY",
    base_url: "https://api.z.ai/api/paas/v4",
    max_tokens_field: "max_tokens",
    include_stream_usage: false,
    provider_name: "Z.AI",
};

inventory::submit!(BuiltInProvider {
    slug: "zai",
    display_name: "Z.AI",
    protocol: Protocol::Openai,
    default_base_url: "https://api.z.ai/api/paas/v4",
    default_api_key_env: "ZHIPU_API_KEY",
    default_model: "zai/glm-5.1",
    plans: Some(&[
        (
            "standard",
            ProviderPlan {
                display_name: "Pay-as-you-go",
                base_url: "https://api.z.ai/api/paas/v4",
                default_model: Some("zai/glm-5.1"),
                login_url: None,
            }
        ),
        (
            "coding",
            ProviderPlan {
                display_name: "Coding plan",
                base_url: "https://api.z.ai/api/coding/paas/v4",
                default_model: Some("zai/glm-5-code"),
                login_url: None,
            }
        ),
    ]),
    login_url: Some("https://z.ai/manage-apikey/apikey-list"),
    needs_url: false,
});

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
                fast: None,
            },
            max_output_tokens: 131072,
            context_window: 200_000,
            reasoning: ReasoningSupport::None,
        },
        ModelEntry {
            prefixes: &["glm-5.2"],
            tier: ModelTier::Strong,
            family: ModelFamily::Glm,
            default: false,
            pricing: ModelPricing {
                input: 1.00,
                output: 3.20,
                cache_write: 0.00,
                cache_read: 0.20,
                fast: None,
            },
            max_output_tokens: 131072,
            context_window: 1_000_000,
            reasoning: ReasoningSupport::GlmEffort,
        },
        ModelEntry {
            prefixes: &["glm-5.1", "glm-5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Glm,
            default: false,
            pricing: ModelPricing {
                input: 1.00,
                output: 3.20,
                cache_write: 0.00,
                cache_read: 0.20,
                fast: None,
            },
            max_output_tokens: 131072,
            context_window: 200_000,
            reasoning: ReasoningSupport::None,
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
                fast: None,
            },
            max_output_tokens: 131072,
            context_window: 200_000,
            reasoning: ReasoningSupport::None,
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
                fast: None,
            },
            max_output_tokens: 131072,
            context_window: 200_000,
            reasoning: ReasoningSupport::None,
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
                fast: None,
            },
            max_output_tokens: 98304,
            context_window: 131_072,
            reasoning: ReasoningSupport::None,
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
                fast: None,
            },
            max_output_tokens: 98304,
            context_window: 131_072,
            reasoning: ReasoningSupport::None,
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
                fast: None,
            },
            max_output_tokens: 98304,
            context_window: 131_072,
            reasoning: ReasoningSupport::None,
        },
    ]
}

pub struct Zai {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

impl Zai {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve("zai", CONFIG_STANDARD.api_key_env)?;
        let mut auth = ResolvedAuth::bearer(pool.current());
        let provider_config = maki_config::providers::ProvidersConfig::load();
        if let Some(url) =
            maki_config::providers::resolve_base_url("zai", provider_config.get("zai"))
        {
            auth.base_url = Some(url);
        }
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG_STANDARD, timeouts),
            auth: Arc::new(Mutex::new(auth)),
            key_pool: Some(pool),
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: super::Timeouts) -> Self {
        Self {
            compat: OpenAiCompatProvider::new(&CONFIG_STANDARD, timeouts),
            auth,
            key_pool: None,
            system_prefix: None,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
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
        opts: RequestOptions,
        _session_id: Option<&str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let mut body = self.compat.build_body(model, messages, system, tools);
            match model.reasoning {
                crate::model::ReasoningSupport::GlmEffort => {
                    opts.thinking.apply_glm_effort(&mut body);
                }
                crate::model::ReasoningSupport::OpenAiEffort => {
                    opts.thinking.apply_reasoning_effort(&mut body);
                }
                _ => {}
            }
            match self
                .compat
                .do_stream(model, &[], &body, event_tx, &auth)
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

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            self.compat.do_list_models(&auth).await
        })
    }

    fn rotate_key(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async {
            Ok(self
                .key_pool
                .as_ref()
                .is_some_and(|p| p.rotate_auth(&self.auth, ResolvedAuth::bearer)))
        })
    }
}
