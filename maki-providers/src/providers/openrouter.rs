use std::sync::{Arc, Mutex};

use flume::Sender;
use serde_json::{Value, json};

use crate::model::{Model, ModelEntry};
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, EffortScale, Message, ProviderEvent, RequestOptions, StreamResponse};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyPool, ResolvedAuth};

const REFERER: &str = "https://maki.sh";
const APP_TITLE: &str = "maki";

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "OPENROUTER_API_KEY",
    base_url: "https://openrouter.ai/api/v1",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "OpenRouter",
};

pub(crate) fn models() -> &'static [ModelEntry] {
    &[]
}

pub struct OpenRouter {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

impl OpenRouter {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::from_env(CONFIG.api_key_env)?;
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

impl Provider for OpenRouter {
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

            body["cache_control"] = json!({"type": "ephemeral"});

            opts.thinking
                .apply_reasoning_effort(&mut body, EffortScale::PreferHigh);

            if let Some(sid) = session_id {
                body["session_id"] = json!(sid);
            }

            let extra_headers = [("HTTP-Referer", REFERER), ("X-OpenRouter-Title", APP_TITLE)];
            self.compat
                .do_stream(model, &extra_headers, &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            self.compat
                .fetch_and_parse_models(&auth, |m| {
                    // Filter: only text input/output models
                    let architecture = m["architecture"].as_object()?;
                    let input_modalities = architecture["input_modalities"].as_array()?;
                    let output_modalities = architecture["output_modalities"].as_array()?;

                    let has_text_input =
                        input_modalities.iter().any(|m| m.as_str() == Some("text"));
                    let has_text_output =
                        output_modalities.iter().any(|m| m.as_str() == Some("text"));
                    if !has_text_input || !has_text_output {
                        return None;
                    }

                    // Parse with OpenRouter-specific pricing field names
                    let id = m["id"].as_str()?;
                    let context_window = m["context_length"]
                        .as_u64()
                        .and_then(|v| u32::try_from(v).ok());
                    let pricing = m["pricing"]
                        .as_object()
                        .and_then(|p| {
                            Some(crate::model::ModelPricing {
                                input: p.get("prompt")?.as_str()?.parse().ok()?,
                                output: p.get("completion")?.as_str()?.parse().ok()?,
                                cache_write: p
                                    .get("input_cache_write")
                                    .and_then(|p| p.as_str()?.parse().ok())
                                    .unwrap_or(0.0),
                                cache_read: p
                                    .get("input_cache_read")
                                    .and_then(|p| p.as_str()?.parse().ok())
                                    .unwrap_or(0.0),
                                fast: None,
                            })
                        })
                        .unwrap_or_default();
                    Some(crate::model::ModelInfo {
                        id: id.to_string(),
                        context_window,
                        max_output_tokens: None,
                        pricing: Some(pricing),
                    })
                })
                .await
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
