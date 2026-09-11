use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_storage::id::SessionRef;
use serde_json::{Value, json};
use tracing::warn;

use crate::model::{Model, ModelEntry, ModelInfo, ModelPricing};
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, dialect};

use super::openai_compat::{MODELS_PATH, OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyPool, ResolvedAuth};

const REFERER: &str = "https://maki.sh";
const APP_TITLE: &str = "maki";
const PER_MILLION: f64 = 1_000_000.0;
/// Requesty's own curated routing policies, with short stable ids like
/// `claude-sonnet-4-5` that spread across several upstream providers. Listed
/// before the raw `<vendor>/<model>` catalog at [`MODELS_PATH`].
const MANAGED_MODELS_PATH: &str = "/models/managed";
const CHAT_API: &str = "chat";

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "requesty",
    api_key_env: "REQUESTY_API_KEY",
    base_url: "https://router.requesty.ai/v1",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "Requesty",
};

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: "requesty",
    display_name: "Requesty",
    protocol: maki_config::providers::Protocol::Openai,
    default_base_url: "https://router.requesty.ai/v1",
    default_api_key_env: "REQUESTY_API_KEY",
    default_model: "requesty/openai/gpt-5.5",
    plans: None,
    login_url: Some("https://app.requesty.ai/api-keys"),
    needs_url: false,
});

pub(crate) const fn models() -> &'static [ModelEntry] {
    &[]
}

pub struct Requesty {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

impl Requesty {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve(CONFIG.slug, CONFIG.api_key_env)?;
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth: Arc::new(Mutex::new(ResolvedAuth::bearer(
                CONFIG.slug,
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

/// A catalog entry, read one field at a time. A missing or `null` field is the
/// normal "Requesty does not say" and stays quiet, while a field that is there
/// in a shape we cannot read is upstream drift: it gets a log line and costs us
/// that one value instead of the whole model.
struct Entry<'a> {
    id: &'a str,
    raw: &'a Value,
}

impl Entry<'_> {
    fn read<T>(&self, name: &str, parse: impl Fn(&Value) -> Option<T>) -> Option<T> {
        let value = self.raw.get(name).filter(|v| !v.is_null())?;
        let parsed = parse(value);
        if parsed.is_none() {
            warn!(model = self.id, field = name, value = %value, "requesty: unreadable field, ignoring it");
        }
        parsed
    }

    /// Requesty sends `0` for a limit it does not know. Left as `Some(0)` it
    /// would beat the manifest fallback and go out as `"max_tokens": 0`.
    fn limit(&self, name: &str) -> Option<u32> {
        self.read(name, |v| u32::try_from(v.as_u64()?).ok())
            .filter(|n| *n > 0)
    }

    /// Prices arrive per token, `ModelPricing` wants $/M.
    fn price(&self, name: &str) -> Option<f64> {
        self.read(name, Value::as_f64).map(|v| v * PER_MILLION)
    }

    fn flag(&self, name: &str) -> bool {
        self.read(name, Value::as_bool) == Some(true)
    }
}

/// Managed policies and the full catalog share this shape, so one parser reads
/// both.
fn parse_model(m: &Value) -> Option<ModelInfo> {
    let id = m["id"].as_str()?;

    // The catalog also lists embedding and other non chat APIs.
    if m["api"].as_str().is_some_and(|api| api != CHAT_API) {
        return None;
    }

    let entry = Entry { id, raw: m };

    // Half a price is no price: without both sides it would read as free.
    let pricing = match (entry.price("input_price"), entry.price("output_price")) {
        (Some(input), Some(output)) => Some(ModelPricing {
            input,
            output,
            cache_write: entry.price("caching_price").unwrap_or(0.0),
            cache_read: entry.price("cached_price").unwrap_or(0.0),
            fast: None,
        }),
        _ => None,
    };

    Some(ModelInfo {
        id: id.to_string(),
        context_window: entry.limit("context_window"),
        max_output_tokens: entry.limit("max_output_tokens"),
        pricing,
        supports_thinking: Some(entry.flag("supports_reasoning")),
        supports_vision: Some(entry.flag("supports_vision")),
        tier: None,
        provider_info: None,
    })
}

/// Managed policies first, then the full catalog, deduplicated by id.
fn merge_models(managed: Vec<ModelInfo>, catalog: Vec<ModelInfo>) -> Vec<ModelInfo> {
    let mut merged = managed;
    for model in catalog {
        if !merged.iter().any(|m| m.id == model.id) {
            merged.push(model);
        }
    }
    merged
}

impl Provider for Requesty {
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

            // Requesty only inserts Anthropic cache breakpoints when asked to.
            // Without this flag Claude pays full input price every turn.
            body["requesty"] = json!({"auto_cache": true});

            if model.supports_thinking() {
                opts.thinking
                    .apply_reasoning_effort(&mut body, &dialect::PREFER_HIGH, model);
            }

            let extra_headers = [("HTTP-Referer", REFERER), ("X-Title", APP_TITLE)];
            self.compat
                .do_stream(model, &extra_headers, &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            // Both listings at once: the picker only shows up once the slowest
            // provider answers, so back to back round trips here cost everyone.
            let (managed, catalog) = futures_lite::future::zip(
                self.compat
                    .fetch_and_parse_models(&auth, MANAGED_MODELS_PATH, parse_model),
                self.compat
                    .fetch_and_parse_models(&auth, MODELS_PATH, parse_model),
            )
            .await;
            match (managed, catalog) {
                (Ok(managed), Ok(catalog)) => Ok(merge_models(managed, catalog)),
                (Ok(managed), Err(e)) => {
                    warn!(error = %e, "requesty: full catalog unavailable, listing managed models only");
                    Ok(managed)
                }
                (Err(e), Ok(catalog)) => {
                    warn!(error = %e, "requesty: managed models unavailable, listing full catalog only");
                    Ok(catalog)
                }
                (Err(e), Err(_)) => Err(e),
            }
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

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const SONNET_ID: &str = "anthropic/claude-sonnet-4-5";
    const UNKNOWN_PRICE_STAYS_UNKNOWN: &str = "a price we cannot read must not become a zero price";
    const EPSILON: f64 = 1e-9;

    fn sonnet_json() -> Value {
        json!({
            "id": SONNET_ID,
            "api": "chat",
            "context_window": 200_000,
            "max_output_tokens": 64_000,
            "input_price": 0.000003,
            "output_price": 0.000015,
            "caching_price": 0.00000375,
            "cached_price": 0.0000003,
            "supports_reasoning": true,
            "supports_vision": true,
        })
    }

    fn assert_price(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < EPSILON,
            "expected ${expected}/M, got ${actual}/M"
        );
    }

    #[test]
    fn parse_model_reads_an_entry_and_scales_prices_to_per_million() {
        let info = parse_model(&sonnet_json()).expect("model should parse");

        assert_eq!(info.id, SONNET_ID);
        assert_eq!(info.context_window, Some(200_000));
        assert_eq!(info.max_output_tokens, Some(64_000));
        assert_eq!(info.supports_vision, Some(true));
        assert_eq!(info.supports_thinking, Some(true));
        let pricing = info.pricing.expect("pricing should be parsed");
        assert_price(pricing.input, 3.0);
        assert_price(pricing.output, 15.0);
        assert_price(pricing.cache_write, 3.75);
        assert_price(pricing.cache_read, 0.3);
    }

    /// Half a price is no price: an all-zero `ModelPricing` reads as free
    /// everywhere downstream.
    #[test_case(json!(null), json!(null)     ; "no_prices")]
    #[test_case(json!(0.000003), json!(null) ; "no_output_price")]
    #[test_case(json!(null), json!(0.000015) ; "no_input_price")]
    fn parse_model_keeps_unusable_pricing_unknown(input: Value, output: Value) {
        let mut m = sonnet_json();
        m["input_price"] = input;
        m["output_price"] = output;

        let info = parse_model(&m).expect("model should parse");
        assert!(info.pricing.is_none(), "{UNKNOWN_PRICE_STAYS_UNKNOWN}");
    }

    /// Requesty reports `0` for a limit it does not know. Kept as `Some(0)`
    /// it would win over the manifest fallback and go out as `"max_tokens": 0`.
    #[test]
    fn parse_model_reads_zero_limits_as_unknown() {
        let mut m = sonnet_json();
        m["context_window"] = json!(0);
        m["max_output_tokens"] = json!(0);

        let info = parse_model(&m).expect("model should parse");
        assert_eq!(info.context_window, None);
        assert_eq!(info.max_output_tokens, None);
    }

    /// One field changing shape upstream costs that field, not the model.
    #[test]
    fn parse_model_keeps_model_when_one_field_is_unreadable() {
        let mut m = sonnet_json();
        m["input_price"] = json!("0.000003");
        m["max_output_tokens"] = json!("64000");

        let info = parse_model(&m).expect("model should still parse");
        assert_eq!(info.context_window, Some(200_000));
        assert_eq!(info.max_output_tokens, None);
        assert!(info.pricing.is_none(), "{UNKNOWN_PRICE_STAYS_UNKNOWN}");
        assert_eq!(info.supports_thinking, Some(true));
    }

    #[test]
    fn parse_model_without_capability_flags_reports_none_supported() {
        let mut m = sonnet_json();
        m["supports_reasoning"] = json!(null);
        m["supports_vision"] = json!(null);

        let info = parse_model(&m).expect("model should parse");
        assert_eq!(info.supports_thinking, Some(false));
        assert_eq!(info.supports_vision, Some(false));
    }

    #[test_case(json!("embedding"), false ; "embedding_is_skipped")]
    #[test_case(json!("image"), false     ; "image_is_skipped")]
    #[test_case(json!(null), true         ; "unlabelled_is_kept")]
    fn parse_model_keeps_only_chat_entries(api: Value, kept: bool) {
        let mut m = sonnet_json();
        m["api"] = api;

        assert_eq!(parse_model(&m).is_some(), kept);
    }

    #[test]
    fn parse_model_without_id_is_skipped() {
        let mut m = sonnet_json();
        m["id"] = json!(null);

        assert!(parse_model(&m).is_none());
    }

    #[test]
    fn merge_models_lists_managed_first_and_dedupes() {
        let managed = vec![
            ModelInfo::id_only("claude-sonnet-4-5".into()),
            ModelInfo::id_only("gpt-5.4-mini".into()),
        ];
        let catalog = vec![
            ModelInfo::id_only(SONNET_ID.into()),
            ModelInfo::id_only("gpt-5.4-mini".into()),
            ModelInfo::id_only("openai/gpt-4o-mini".into()),
        ];

        let ids: Vec<String> = merge_models(managed, catalog)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(
            ids,
            [
                "claude-sonnet-4-5",
                "gpt-5.4-mini",
                SONNET_ID,
                "openai/gpt-4o-mini",
            ]
        );
    }
}
