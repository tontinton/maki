use std::sync::{Arc, Mutex};

use flume::Sender;
use serde_json::Value;
use tracing::warn;

use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ModelTier, ReasoningSupport};
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyPool, ResolvedAuth};

const PAD: &str = "";
const V4_MARKER: &str = "deepseek-v4";

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "DEEPSEEK_API_KEY",
    base_url: "https://api.deepseek.com",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "DeepSeek",
};

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: "deepseek",
    display_name: "DeepSeek",
    protocol: maki_config::providers::Protocol::Openai,
    default_base_url: "https://api.deepseek.com",
    default_api_key_env: "DEEPSEEK_API_KEY",
    default_model: "deepseek/deepseek-v4-flash",
    plans: None,
    login_url: Some("https://platform.deepseek.com/api_keys"),
    needs_url: false,
});

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
                fast: None,
            },
            max_output_tokens: 384_000,
            context_window: 1_000_000,
            reasoning: ReasoningSupport::OpenAiEffort,
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
                fast: None,
            },
            max_output_tokens: 384_000,
            context_window: 1_000_000,
            reasoning: ReasoningSupport::OpenAiEffort,
        },
    ]
}

pub struct DeepSeek {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

impl DeepSeek {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve("deepseek", CONFIG.api_key_env)?;
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

impl Provider for DeepSeek {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        _session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let mut body = self.compat.build_body(model, messages, system, tools);

            // DeepSeek enables reasoning by default; Adaptive and Budget
            // use the model's default reasoning behavior.
            match opts.thinking {
                ThinkingConfig::Off => {
                    body["thinking"] = serde_json::json!({"type": "disabled"});
                }
                ThinkingConfig::Adaptive => {
                    body["thinking"] = serde_json::json!({"type": "enabled"});
                    pad_reasoning_content(&model.id, &mut body);
                }
                ThinkingConfig::Budget(_) => {
                    body["thinking"] = serde_json::json!({"type": "enabled"});
                    body["reasoning_effort"] = serde_json::json!("max");
                    warn!("DeepSeek reasoning does not support token budgets");
                    pad_reasoning_content(&model.id, &mut body);
                }
            }

            self.compat
                .do_stream(model, &[], &body, event_tx, &auth)
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

/// DeepSeek's two reasoning models disagree about `reasoning_content`: V4 in
/// thinking mode wants it on every assistant turn (missing = 400), R1 refuses
/// it as input. So we gate on the V4 substring, same trick Vercel's AI SDK
/// uses, and back-fill the turns that have none (plain replies, tool-only
/// turns). The API only checks the field exists, so `""` is enough.
///
/// Ref: <https://api-docs.deepseek.com/guides/thinking_mode>
fn pad_reasoning_content(model_id: &str, body: &mut Value) {
    if !model_id.contains(V4_MARKER) {
        return;
    }
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some("assistant")
            || msg
                .get("reasoning_content")
                .and_then(Value::as_str)
                .is_some()
        {
            continue;
        }
        msg["reasoning_content"] = Value::String(PAD.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const V4: &str = "deepseek-v4-pro";
    const R1: &str = "deepseek-reasoner";

    #[test]
    fn v4_pads_only_assistant_turns_without_reasoning() {
        let mut body = json!({"messages": [
            {"role": "system",    "content": "sys"},
            {"role": "user",      "content": "hi"},
            {"role": "assistant", "content": "ok", "reasoning_content": "kept"},
            {"role": "assistant", "content": "",   "tool_calls": [{"id": "c1"}]},
            {"role": "tool",      "tool_call_id": "c1", "content": "out"},
        ]});
        pad_reasoning_content(V4, &mut body);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[2]["reasoning_content"], "kept");
        assert_eq!(msgs[3]["reasoning_content"], PAD);
        for i in [0, 1, 4] {
            assert!(msgs[i].get("reasoning_content").is_none());
        }
    }

    #[test]
    fn non_v4_model_is_untouched() {
        let input = json!({"messages": [
            {"role": "assistant", "content": "", "tool_calls": [{"id": "c1"}]},
            {"role": "assistant", "content": "hi"},
        ]});
        let mut body = input.clone();
        pad_reasoning_content(R1, &mut body);
        assert_eq!(body, input);
    }
}
