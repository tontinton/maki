use std::sync::{Arc, Mutex};

use flume::Sender;
use serde_json::{Value, json};

use crate::model::{Model, ModelEntry};
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig};

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

#[derive(Debug)]
struct OpenRouterModelInfo {
    reasoning_mandatory: bool,
    reasoning_default_enabled: bool,
    reasoning_efforts: Vec<String>,
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

fn map_effort_to_supported<'a>(requested: &'a str, supported: &'a [String]) -> &'a str {
    const EFFORT_ORDER: &[&str] = &["max", "xhigh", "high", "medium", "low", "minimal", "none"];

    if supported.iter().any(|s| s == requested) {
        return requested;
    }
    let req_idx = EFFORT_ORDER
        .iter()
        .position(|&e| e == requested)
        .unwrap_or(0);
    for effort in EFFORT_ORDER.iter().skip(req_idx) {
        if supported.contains(&effort.to_string()) {
            return effort;
        }
    }
    supported.last().map(|s| s.as_str()).unwrap_or(requested)
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

            let reasoning_info: Option<Arc<OpenRouterModelInfo>> = {
                let guard = crate::model_registry::model_registry().read().unwrap();
                guard
                    .discovered(model.provider, &model.id)
                    .and_then(|d| d.provider_info.clone())
                    .map(|arc| {
                        Arc::downcast::<OpenRouterModelInfo>(arc).expect("wrong provider info type")
                    })
            };

            let (mandatory, default_enabled) = reasoning_info
                .as_ref()
                .map(|r| (r.reasoning_mandatory, r.reasoning_default_enabled))
                .unwrap_or((false, false));

            // Determine if and how to send reasoning config for OpenRouter.
            // Models have three states:
            // 1. mandatory: true - reasoning always on, can't be disabled.
            // 2. default_enabled: true - reasoning on by default, disable with effort: "none".
            // 3. default off - reasoning off by default, enabled with any reasoning object.
            let reasoning_body = if model.supports_thinking() {
                let effort = match opts.thinking {
                    ThinkingConfig::Off => "none",
                    // FIXME: Should probably use default_effort if provided instead of high
                    ThinkingConfig::Adaptive => "high",
                    ThinkingConfig::Budget(n) => ThinkingConfig::budget_to_effort(n),
                };
                match opts.thinking {
                    ThinkingConfig::Off if mandatory => None,
                    ThinkingConfig::Off if default_enabled => Some(json!({"effort": "none"})),
                    ThinkingConfig::Off => None,
                    _ => {
                        let final_effort = if let Some(info) = &reasoning_info {
                            map_effort_to_supported(effort, &info.reasoning_efforts)
                        } else {
                            effort
                        };
                        Some(json!({"effort": final_effort}))
                    }
                }
            } else {
                None
            };

            if let Some(reasoning) = reasoning_body {
                body["reasoning"] = reasoning;
            }

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

                    let supports_vision =
                        input_modalities.iter().any(|m| m.as_str() == Some("image"));

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

                    let reasoning = m.get("reasoning").and_then(|v| v.as_object()).map(|v| {
                        OpenRouterModelInfo {
                            reasoning_mandatory: v.get("mandatory").and_then(Value::as_bool)
                                == Some(true),
                            reasoning_default_enabled: v
                                .get("default_enabled")
                                .and_then(Value::as_bool)
                                == Some(true),
                            reasoning_efforts: v
                                .get("supported_efforts")
                                .and_then(Value::as_array)
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_str().map(String::from))
                                        .collect()
                                })
                                .unwrap_or_default(),
                        }
                    });

                    let supports_thinking = reasoning.is_some()
                        || m.get("supported_parameters")
                            .and_then(|v| v.as_array())
                            .is_some_and(|v| v.iter().any(|v| v.as_str() == Some("reasoning")));

                    Some(crate::model::ModelInfo {
                        id: id.to_string(),
                        context_window,
                        max_output_tokens: None,
                        pricing: Some(pricing),
                        supports_thinking: Some(supports_thinking),
                        supports_vision: Some(supports_vision),
                        provider_info: reasoning
                            .map(|r| Arc::new(r) as Arc<dyn std::any::Any + Send + Sync>),
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
