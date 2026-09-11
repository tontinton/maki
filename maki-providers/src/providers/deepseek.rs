use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_storage::id::SessionRef;
use serde::Deserialize;
use serde_json::Value;
use tracing::warn;

use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::pricing::{PricingSchedule, PricingWindow};
use crate::provider::{BoxFuture, Provider};
use crate::types::{ProviderUsage, UsageLimit};
use crate::{
    AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, ThinkingConfig, dialect,
};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyPool, ResolvedAuth};

const PAD: &str = "";
const REASONER_ID: &str = "deepseek-reasoner";
const BALANCE_URL: &str = "https://api.deepseek.com/user/balance";

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "deepseek",
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
    default_model: "deepseek/deepseek-flash",
    plans: None,
    login_url: Some("https://platform.deepseek.com/api_keys"),
    needs_url: false,
});

/// Peak hours double every rate, and the tables below quote the off-peak ones.
/// The weekend stays off-peak around the clock.
/// <https://api-docs.deepseek.com/quick_start/pricing/>
pub(crate) const PEAK_HOURS: PricingSchedule =
    PricingSchedule::new(PEAK_WINDOWS, PEAK_MULTIPLIER).weekdays_only();

const PEAK_WINDOWS: &[PricingWindow] = &[PricingWindow::hours(1, 4), PricingWindow::hours(6, 10)];
const PEAK_MULTIPLIER: f64 = 2.0;

pub(crate) const fn models() -> &'static [ModelEntry] {
    &[
        // `deepseek-flash` is V4.1 Flash. `deepseek-v4-flash` is the retired
        // name the API still accepts, served by V4.1 Flash at its rates.
        ModelEntry {
            prefixes: &["deepseek-flash", "deepseek-v4-flash"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 0.15,
                output: 0.60,
                cache_write: 0.00,
                cache_read: 0.003,
                fast: None,
            },
            max_output_tokens: Some(384_000),
            context_window: 1_000_000,
        },
        ModelEntry {
            prefixes: &["deepseek-v4-pro"],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            vision: false,
            default: true,
            pricing: ModelPricing {
                input: 0.66,
                output: 1.98,
                cache_write: 0.00,
                cache_read: 0.022,
                fast: None,
            },
            max_output_tokens: Some(384_000),
            context_window: 1_000_000,
        },
    ]
}

#[derive(Deserialize)]
struct BalanceResponse {
    balance_infos: Vec<BalanceInfo>,
}

#[derive(Deserialize)]
struct BalanceInfo {
    currency: String,
    total_balance: String,
    granted_balance: String,
    topped_up_balance: String,
}

impl From<BalanceResponse> for ProviderUsage {
    fn from(resp: BalanceResponse) -> Self {
        let limits = resp
            .balance_infos
            .into_iter()
            .map(|b| {
                let symbol = match b.currency.as_str() {
                    "USD" => "$",
                    "CNY" => "¥",
                    _ => "",
                };

                UsageLimit {
                    label: "Balance".into(),
                    percentage: None,
                    reset_at: None,
                    detail: Some(format!(
                        "total: {}{}, topped-up: {}{}, granted: {}{}",
                        symbol,
                        b.total_balance,
                        symbol,
                        b.topped_up_balance,
                        symbol,
                        b.granted_balance
                    )),
                }
            })
            .collect();
        ProviderUsage {
            plan: None,
            limits,
            by_model_today: vec![],
        }
    }
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
            auth: Arc::new(Mutex::new(ResolvedAuth::bearer(
                "deepseek",
                pool.current(),
            )?)),
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
        _session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let mut body = self.compat.build_body(model, messages, system, tools);

            if opts.thinking.is_enabled() {
                body["thinking"] = serde_json::json!({"type": "enabled"});
                opts.thinking
                    .apply_reasoning_effort(&mut body, &dialect::DEEPSEEK, model);
                if matches!(opts.thinking, ThinkingConfig::Budget(_)) {
                    warn!("DeepSeek reasoning does not support token budgets");
                }
                pad_reasoning_content(&model.id, &mut body);
            } else {
                body["thinking"] = serde_json::json!({"type": "disabled"});
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

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let body = self.compat.get_text(&auth, BALANCE_URL).await?;
            let parsed: BalanceResponse = serde_json::from_str(&body)?;
            Ok(Some(parsed.into()))
        })
    }

    fn rotate_key(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async {
            Ok(self
                .key_pool
                .as_ref()
                .is_some_and(|p| p.rotate_bearer(&self.auth)))
        })
    }
}

/// Whether a model speaks the thinking protocol DeepSeek introduced with V4:
/// an explicit toggle, and `reasoning_content` echoed back on input. Only
/// `deepseek-reasoner` (R1) sits outside it, reasoning unconditionally and
/// refusing the field as input, so we name that one id rather than match a
/// version marker the next rename would break. Providers that resell DeepSeek
/// share the gate, after stripping their vendor prefix.
///
/// Ref: <https://api-docs.deepseek.com/guides/thinking_mode>
pub(crate) fn uses_v4_thinking_protocol(model_id: &str) -> bool {
    !model_id.starts_with(REASONER_ID)
}

/// V4 and later want `reasoning_content` on every assistant turn of a request
/// carrying `tools` (missing = 400), so we back-fill the turns that have none:
/// plain replies and tool-only turns. The API only checks the field exists, so
/// `""` is enough. Requests without tools are left alone, since nothing asks
/// for the field there and this runs for any id a DeepSeek-based custom
/// provider is pointed at.
fn pad_reasoning_content(model_id: &str, body: &mut Value) {
    if !uses_v4_thinking_protocol(model_id) || body.get("tools").is_none() {
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
    use crate::manifest::ManifestRegistry;
    use serde_json::json;
    use test_case::test_case;

    /// No version marker in the id, which is what the old substring gate missed.
    const FLASH: &str = "deepseek-flash";
    /// The hours, days and surcharge as the pricing page states them.
    const PUBLISHED_PEAK_HOURS: &str = "2x during 01:00-04:00, 06:00-10:00 UTC, Mon-Fri";

    /// `PEAK_HOURS` only reaches a bill through the manifest, and a schedule
    /// that never got hooked up looks exactly like off-peak all day. The
    /// published hours are pinned here too, since either drifting bills every
    /// DeepSeek turn at the wrong rate.
    #[test]
    fn the_manifest_bills_the_published_peak_hours() {
        let schedule = ManifestRegistry::get(CONFIG.slug)
            .expect("deepseek is a builtin")
            .pricing_schedule
            .expect("deepseek bills by the clock");
        assert_eq!(schedule.to_string(), PEAK_HOURS.to_string());
        assert_eq!(PEAK_HOURS.to_string(), PUBLISHED_PEAK_HOURS);
    }

    fn tool_call_body() -> Value {
        json!({
            "tools": [{"type": "function", "function": {"name": "read"}}],
            "messages": [
                {"role": "system",    "content": "sys"},
                {"role": "user",      "content": "hi"},
                {"role": "assistant", "content": "ok", "reasoning_content": "kept"},
                {"role": "assistant", "content": "",   "tool_calls": [{"id": "c1"}]},
                {"role": "tool",      "tool_call_id": "c1", "content": "out"},
            ],
        })
    }

    #[test]
    fn pads_only_assistant_turns_without_reasoning() {
        let mut body = tool_call_body();
        pad_reasoning_content(FLASH, &mut body);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[2]["reasoning_content"], "kept");
        assert_eq!(msgs[3]["reasoning_content"], PAD);
        for i in [0, 1, 4] {
            assert!(msgs[i].get("reasoning_content").is_none());
        }
    }

    /// Padding exists for one 400, raised on requests that carry `tools`, by
    /// models that echo the field. Everything else has to come back byte for
    /// byte, and the tool-less case matters because the gate now lets through
    /// any id a DeepSeek-based custom provider is pointed at.
    #[test_case(REASONER_ID, true; "the one model that refuses the field")]
    #[test_case(FLASH, false; "a request that never carried tools")]
    fn bodies_outside_the_workaround_are_untouched(model_id: &str, tools: bool) {
        let mut input = tool_call_body();
        if !tools {
            input.as_object_mut().unwrap().remove("tools");
        }
        let mut body = input.clone();
        pad_reasoning_content(model_id, &mut body);
        assert_eq!(body, input);
    }

    /// The gate the rename broke once already: it has to key off the one id that
    /// refuses the field, never off a version marker in the others.
    #[test_case(FLASH, true; "current flash")]
    #[test_case("deepseek-v9-turbo", true; "a release the table has never seen")]
    #[test_case(REASONER_ID, false; "the one model that refuses it")]
    fn only_the_legacy_reasoner_is_outside_the_v4_protocol(model_id: &str, expected: bool) {
        assert_eq!(uses_v4_thinking_protocol(model_id), expected);
    }
}
