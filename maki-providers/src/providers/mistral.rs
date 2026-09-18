use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_storage::id::SessionRef;
use serde_json::{Value, json};

use maki_config::providers::{Protocol, ProviderPlan};

use crate::model::{Model, ModelFamily, ThinkingSupport};
use crate::provider::{BoxFuture, Provider};
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, CatalogDoc, GeneratedDocs, LoginConfig, Native, ProviderSpec,
};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, dialect};

use super::openai_compat::{MODELS_PATH, OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyHeader, KeyPool, KeyRotation, ResolvedAuth, Timeouts};

const SLUG: &str = "mistral";
const DISPLAY_NAME: &str = "Mistral";
const ENV_VAR: &str = "MISTRAL_API_KEY";
const BASE_URL: &str = "https://api.mistral.ai/v1";
const DEFAULT_MODEL: &str = "mistral/mistral-medium-latest";
const CODING_MODEL: &str = "mistral/mistral-vibe-cli-latest";
const LOGIN_URL: &str = "https://admin.mistral.ai/organization/api-keys";
const MAX_TOKENS_FIELD: &str = "max_tokens";

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: SLUG,
    api_key_env: ENV_VAR,
    base_url: BASE_URL,
    max_tokens_field: MAX_TOKENS_FIELD,
    include_stream_usage: true,
    provider_name: DISPLAY_NAME,
};

const PLANS: &[(&str, ProviderPlan)] = &[
    (
        "standard",
        ProviderPlan {
            display_name: "Standard",
            base_url: BASE_URL,
            default_model: Some(DEFAULT_MODEL),
            login_url: None,
        },
    ),
    (
        "coding",
        ProviderPlan {
            display_name: "Vibe / Coding",
            base_url: BASE_URL,
            default_model: Some(CODING_MODEL),
            login_url: Some("https://console.mistral.ai/codestral/cli"),
        },
    ),
];

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: None,
    fallback_context_window: 128_000,
    models_toml: include_str!("../../models/mistral.toml"),
    pricing_schedule: None,
    native: Some(Native {
        new: create,
        with_auth: create_with_auth,
        aperture: Some(ApertureRoute {
            path_prefix: DEFAULT_PATH_PREFIX,
        }),
    }),
    login: Some(LoginConfig {
        protocol: Protocol::Openai,
        default_base_url: BASE_URL,
        default_model: DEFAULT_MODEL,
        plans: Some(PLANS),
        login_url: Some(LOGIN_URL),
        needs_url: false,
    }),
    docs: GeneratedDocs {
        api_urls: &[BASE_URL],
        features: None,
        auth: AuthDoc::EnvVar,
        catalog: CatalogDoc::Table,
        trailing_notes: &[],
    },
};

fn create(timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    Ok(Box::new(Mistral::new(timeouts)?))
}

fn create_with_auth(
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
) -> Box<dyn Provider> {
    Box::new(Mistral::with_auth(auth, timeouts).with_system_prefix(system_prefix))
}

inventory::submit!(SPEC.config_row());

pub struct Mistral {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

fn convert_assistant_messages_in_place(messages: &mut Value) {
    if let Some(msgs) = messages.as_array_mut() {
        for msg in msgs {
            if let Some(obj) = msg.as_object_mut()
                && obj.get("role").and_then(Value::as_str) == Some("assistant")
            {
                let Some(reasoning_val) = obj.remove("reasoning_content") else {
                    continue;
                };
                let Some(reasoning_text) = reasoning_val.as_str() else {
                    continue;
                };

                let thinking_block = json!({
                    "type": "thinking",
                    "thinking": [{"type": "text", "text": reasoning_text}]
                });

                if let Some(content) = obj.get_mut("content") {
                    if let Some(content_str) = content.as_str()
                        && !content_str.is_empty()
                    {
                        // Has text content, create array with both
                        let text_content = json!({"type": "text", "text": content_str});
                        *content = json!([thinking_block, text_content]);
                    } else if content.is_string() {
                        // Empty string content, just use thinking
                        *content = json!([thinking_block]);
                    } else if let Some(arr) = content.as_array_mut() {
                        // Already an array, prepend thinking
                        arr.insert(0, thinking_block);
                    } else {
                        *content = json!([thinking_block]);
                    }
                } else {
                    obj.insert("content".to_string(), json!([thinking_block]));
                }
            }
        }
    }
}

impl Mistral {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve("mistral", CONFIG.api_key_env)?;
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth: Arc::new(Mutex::new(ResolvedAuth::bearer("mistral", pool.current())?)),
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
        session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let mut body = self.compat.build_body(model, messages, system, tools);
            opts.thinking
                .apply_reasoning_effort(&mut body, &dialect::HIGH_ONLY, model);
            // Convert assistant messages to Mistral's expected format with thinking content
            convert_assistant_messages_in_place(body.get_mut("messages").unwrap());

            let mut extra_headers = vec![];
            if let Some(session_id) = session_id {
                extra_headers.push(("x-affinity", session_id.as_str()));
            }
            self.compat
                .do_stream(model, &extra_headers, &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            self.compat
                .fetch_and_parse_models(&auth, MODELS_PATH, |m| {
                    // Filter: only completion_chat capable models
                    let has_completion_chat = m
                        .get("capabilities")
                        .and_then(Value::as_object)
                        .and_then(|c| c.get("completion_chat"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if !has_completion_chat {
                        return None;
                    }

                    // Parse with Mistral-specific field names
                    let id = m["id"].as_str()?;
                    let context_window = m["max_context_length"]
                        .as_u64()
                        .and_then(|v| u32::try_from(v).ok());
                    let supports_thinking = m
                        .get("capabilities")
                        .and_then(Value::as_object)
                        .and_then(|c| c.get("reasoning"))
                        .and_then(Value::as_bool);
                    let supports_vision = m
                        .get("capabilities")
                        .and_then(Value::as_object)
                        .and_then(|c| c.get("vision"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    Some(crate::model::ModelInfo {
                        id: id.to_string(),
                        context_window,
                        max_output_tokens: None,
                        pricing: None,
                        supports_thinking,
                        supports_vision: Some(supports_vision),
                        tier: None,
                        provider_info: None,
                    })
                })
                .await
        })
    }

    fn keys(&self) -> Option<KeyRotation<'_>> {
        Some(KeyRotation::new(
            self.key_pool.as_ref()?,
            &self.auth,
            KeyHeader::Bearer,
        ))
    }

    fn adjust_model(&self, model: &mut Model) {
        adjust_model(model);
    }
}

fn adjust_model(model: &mut Model) {
    if model.id.starts_with("ministral-") {
        model.thinking_override = Some(ThinkingSupport::No);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use test_case::test_case;

    #[test_case(
        json!([
            {"role": "system", "content": "sys"},
            {"role": "assistant", "content": "text", "reasoning_content": "thinking"}
        ]),
        json!([
            {"role": "system", "content": "sys"},
            {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": [{"type": "text", "text": "thinking"}]},
                    {"type": "text", "text": "text"}
                ]
            }
        ])
        ; "assistant_text_and_thinking"
    )]
    #[test_case(
        json!([
            {"role": "system", "content": "sys"},
            {"role": "assistant", "content": "", "reasoning_content": "thinking"}
        ]),
        json!([
            {"role": "system", "content": "sys"},
            {
                "role": "assistant",
                "content": [{"type": "thinking", "thinking": [{"type": "text", "text": "thinking"}]}]
            }
        ])
        ; "assistant_empty_content_with_thinking"
    )]
    #[test_case(
        json!([
            {"role": "system", "content": "sys"},
            {"role": "assistant", "reasoning_content": "thinking"}
        ]),
        json!([
            {"role": "system", "content": "sys"},
            {
                "role": "assistant",
                "content": [{"type": "thinking", "thinking": [{"type": "text", "text": "thinking"}]}]
            }
        ])
        ; "assistant_no_content_with_thinking"
    )]
    #[test_case(
        json!([
            {"role": "system", "content": "sys"},
            {"role": "assistant", "content": "text"}
        ]),
        json!([
            {"role": "system", "content": "sys"},
            {"role": "assistant", "content": "text"}
        ])
        ; "assistant_text_only_no_thinking"
    )]
    fn convert_assistant_messages_in_place_test(input: Value, expected: Value) {
        let mut input_clone = input.clone();
        convert_assistant_messages_in_place(&mut input_clone);
        assert_eq!(input_clone, expected);
    }

    #[test_case("mistral/ministral-14b-latest", false ; "ministral_no_thinking")]
    #[test_case("mistral/mistral-medium-latest", true ; "mistral_medium_supports_thinking")]
    fn adjust_model_sets_thinking_support(spec: &str, expected: bool) {
        let mut model = Model::from_spec(spec).unwrap();
        adjust_model(&mut model);
        assert_eq!(model.supports_thinking(), expected);
    }
}
