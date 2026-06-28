use std::sync::{Arc, Mutex};

use flume::Sender;
use serde_json::{Value, json};

use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyPool, ResolvedAuth};

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "MISTRAL_API_KEY",
    base_url: "https://api.mistral.ai/v1",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "Mistral",
};

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: "mistral",
    display_name: "Mistral",
    protocol: maki_config::providers::Protocol::Openai,
    default_base_url: "https://api.mistral.ai/v1",
    default_api_key_env: "MISTRAL_API_KEY",
    default_model: "mistral/mistral-medium-latest",
    plans: Some(&[
        (
            "standard",
            maki_config::providers::ProviderPlan {
                display_name: "Standard",
                base_url: "https://api.mistral.ai/v1",
                default_model: Some("mistral/mistral-medium-latest"),
                login_url: None,
            }
        ),
        (
            "coding",
            maki_config::providers::ProviderPlan {
                display_name: "Vibe / Coding",
                base_url: "https://api.mistral.ai/v1",
                default_model: Some("mistral/mistral-vibe-cli-latest"),
                login_url: Some("https://console.mistral.ai/codestral/cli"),
            }
        ),
    ]),
    login_url: Some("https://admin.mistral.ai/organization/api-keys"),
    needs_url: false,
});

pub(crate) fn models() -> &'static [ModelEntry] {
    &[
        ModelEntry {
            prefixes: &[
                "mistral-medium-latest",
                "mistral-medium-3.5",
                "mistral-medium-2604",
            ],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            default: true,
            pricing: ModelPricing {
                input: 1.5,
                output: 7.5,
                cache_write: 0.00,
                cache_read: 0.00,
                fast: None,
            },
            max_output_tokens: 262_144,
            context_window: 262_144,
        },
        ModelEntry {
            prefixes: &["mistral-small-latest", "mistral-small-2603"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            default: true,
            pricing: ModelPricing {
                input: 0.15,
                output: 0.60,
                cache_write: 0.00,
                cache_read: 0.00,
                fast: None,
            },
            max_output_tokens: 262_144,
            context_window: 262_144,
        },
        ModelEntry {
            prefixes: &["ministral-14b-latest", "ministral-14b-2512"],
            tier: ModelTier::Weak,
            family: ModelFamily::Generic,
            default: true,
            pricing: ModelPricing {
                input: 0.20,
                output: 0.20,
                cache_write: 0.00,
                cache_read: 0.00,
                fast: None,
            },
            max_output_tokens: 262_144,
            context_window: 262_144,
        },
    ]
}

pub struct Mistral {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

impl Mistral {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve("mistral", CONFIG.api_key_env)?;
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth: Arc::new(Mutex::new(ResolvedAuth::bearer(pool.current()))),
            key_pool: Some(pool),
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: super::Timeouts) -> Self {
        Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
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

impl Provider for Mistral {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let mut body = self.compat.build_body(model, messages, system, tools);

            if !matches!(opts.thinking, ThinkingConfig::Off) {
                body["reasoning_effort"] = json!("high");
            }

            let mut extra_headers = vec![];
            if let Some(session_id) = session_id {
                extra_headers.push(("x-affinity", session_id));
            }
            self.compat
                .do_stream(model, &extra_headers, &body, event_tx, &auth)
                .await
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
