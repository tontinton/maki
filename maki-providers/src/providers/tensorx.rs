use std::sync::{Arc, Mutex};

use flume::Sender;
use serde_json::{Value, json};

use crate::model::{Model, ModelEntry, ModelInfo, ModelPricing};
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyPool, ResolvedAuth};

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "TENSORX_API_KEY",
    base_url: "https://api.tensorx.ai/v1",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "TensorX",
};

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: "tensorx",
    display_name: "TensorX",
    protocol: maki_config::providers::Protocol::Openai,
    default_base_url: "https://api.tensorx.ai/v1",
    default_api_key_env: "TENSORX_API_KEY",
    default_model: "tensorx/z-ai/glm-5.2",
    plans: None,
    login_url: Some("https://tensorx.ai"),
    needs_url: false,
});

pub(crate) fn models() -> &'static [ModelEntry] {
    &[]
}

pub struct TensorX {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

impl TensorX {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve("tensorx", CONFIG.api_key_env)?;
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

impl Provider for TensorX {
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

            // See https://docs.tensorx.ai/api-reference/chat-completions#reasoning
            if !matches!(opts.thinking, ThinkingConfig::Off)
                && model.id.starts_with("deepseek/deepseek-v4")
            {
                body["chat_template_kwargs"] = json!({"thinking": true});
            }
            // For other models it seems to be hardcoded, e.g. minimax m3 requests
            // fail when setting reasoning_effort=none

            self.compat
                .do_stream(model, &[], &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let url = format!("{}/model/info", CONFIG.base_url);
            let text = self.compat.get_text(&auth, &url).await?;
            let body: Value = serde_json::from_str(&text)?;

            let mut models: Vec<ModelInfo> = body["data"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|entry| {
                            let id = entry["model_name"].as_str()?;
                            let info = entry.get("model_info")?;

                            // Only include models with mode "chat" or mode null
                            let mode_ok = info
                                .get("mode")
                                .and_then(|v| v.as_str())
                                .is_none_or(|m| m == "chat");
                            if !mode_ok {
                                return None;
                            }

                            // Context window: prefer max_tokens, fall back to max_input_tokens
                            let context_window = info["max_tokens"]
                                .as_u64()
                                .or_else(|| info["max_input_tokens"].as_u64())
                                .and_then(|v| u32::try_from(v).ok());

                            // FIXME: API rejects requests if we request the maximum number of
                            // output tokens. It checks input+max_output<=context_window
                            // let max_output_tokens = info["max_output_tokens"]
                            //     .as_u64()
                            //     .and_then(|v| u32::try_from(v).ok());
                            let max_output_tokens = None;

                            // Convert per-token costs to per-million costs
                            let input_cost = info["input_cost_per_token"].as_f64();
                            let output_cost = info["output_cost_per_token"].as_f64();
                            let pricing = if input_cost.is_some() || output_cost.is_some() {
                                let per_million = 1_000_000.0;
                                Some(ModelPricing {
                                    input: input_cost.unwrap_or(0.0) * per_million,
                                    output: output_cost.unwrap_or(0.0) * per_million,
                                    cache_write: info["cache_creation_input_token_cost"]
                                        .as_f64()
                                        .unwrap_or(0.0)
                                        * per_million,
                                    cache_read: info["cache_read_input_token_cost"]
                                        .as_f64()
                                        .unwrap_or(0.0)
                                        * per_million,
                                    fast: None,
                                })
                            } else {
                                None
                            };

                            Some(ModelInfo {
                                id: id.to_string(),
                                context_window,
                                max_output_tokens,
                                pricing,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            models.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(models)
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
