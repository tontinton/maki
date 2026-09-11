use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_storage::id::SessionRef;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use crate::model::{Model, ModelEntry, ModelInfo, ModelPricing};
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, dialect};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyPool, ResolvedAuth};

const REFERER: &str = "https://maki.sh";
const APP_TITLE: &str = "maki";
const PER_MILLION: f64 = 1_000_000.0;
/// Curated, Requesty-maintained routing policies. Short stable ids
/// (`claude-sonnet-4-5`, `gpt-5.4-mini`, `gpt-5-mini@eu`) that route across
/// several upstream providers; listed first so users see them before the
/// raw `<vendor>/<model>` catalog.
const MANAGED_MODELS_PATH: &str = "/models/managed";
/// Full `<vendor>/<model>` catalog.
const MODELS_PATH: &str = "/models";
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

/// The subset of a Requesty `/models` entry maki cares about. Both the managed
/// and the full catalog share this shape. Prices are USD per token.
#[derive(Debug, Deserialize)]
struct RequestyModel {
    id: String,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    context_window: Option<u32>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    #[serde(default)]
    input_price: Option<f64>,
    #[serde(default)]
    output_price: Option<f64>,
    #[serde(default)]
    cached_price: Option<f64>,
    #[serde(default)]
    caching_price: Option<f64>,
    #[serde(default)]
    supports_reasoning: Option<bool>,
    #[serde(default)]
    supports_vision: Option<bool>,
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

    async fn fetch_models(
        &self,
        auth: &ResolvedAuth,
        path: &str,
    ) -> Result<Vec<ModelInfo>, AgentError> {
        let base = self.compat.base_url(auth);
        let body_text = self.compat.get_text(auth, &format!("{base}{path}")).await?;
        let body: Value = serde_json::from_str(&body_text)?;
        let mut models: Vec<ModelInfo> = body["data"]
            .as_array()
            .map(|arr| arr.iter().filter_map(parse_model).collect())
            .unwrap_or_default();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }
}

fn parse_model(m: &Value) -> Option<ModelInfo> {
    let model: RequestyModel = serde_json::from_value(m.clone()).ok()?;

    // Only chat models: the catalog also lists embedding and other APIs.
    if model.api.as_deref().is_some_and(|api| api != CHAT_API) {
        return None;
    }

    // Requesty reports per-token prices; scale to $/M as `ModelPricing`
    // expects. A missing price stays `None` so it never reads as free.
    let per_million = |p: Option<f64>| p.map(|v| v * PER_MILLION);
    let pricing = match (
        per_million(model.input_price),
        per_million(model.output_price),
    ) {
        (Some(input), Some(output)) => Some(ModelPricing {
            input,
            output,
            cache_write: per_million(model.caching_price).unwrap_or(0.0),
            cache_read: per_million(model.cached_price).unwrap_or(0.0),
            fast: None,
        }),
        _ => None,
    };

    Some(ModelInfo {
        id: model.id,
        context_window: model.context_window,
        max_output_tokens: model.max_output_tokens,
        pricing,
        supports_thinking: Some(model.supports_reasoning == Some(true)),
        supports_vision: Some(model.supports_vision == Some(true)),
        tier: None,
        provider_info: None,
    })
}

/// Managed policies first, then the full catalog, deduplicated by id. Either
/// list alone is still a usable answer, so one failing does not fail the other.
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
            let managed = self.fetch_models(&auth, MANAGED_MODELS_PATH).await;
            let catalog = self.fetch_models(&auth, MODELS_PATH).await;
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

    const UNKNOWN_PRICE_STAYS_UNKNOWN: &str = "a price we cannot read must not become a zero price";

    fn sonnet_json() -> Value {
        json!({
            "id": "anthropic/claude-sonnet-4-5",
            "api": "chat",
            "object": "model",
            "context_window": 200_000,
            "max_output_tokens": 64_000,
            "input_price": 0.000003,
            "output_price": 0.000015,
            "cached_price": 0.0000003,
            "supports_reasoning": true,
            "supports_vision": true,
            "supports_tool_calling": true,
        })
    }

    #[test]
    fn parse_model_scales_pricing_to_per_million() {
        let info = parse_model(&sonnet_json()).expect("model should parse");

        assert_eq!(info.id, "anthropic/claude-sonnet-4-5");
        assert_eq!(info.context_window, Some(200_000));
        assert_eq!(info.max_output_tokens, Some(64_000));
        assert_eq!(info.supports_vision, Some(true));
        assert_eq!(info.supports_thinking, Some(true));
        let pricing = info.pricing.expect("pricing should be parsed");
        assert!((pricing.input - 3.0).abs() < 1e-9);
        assert!((pricing.output - 15.0).abs() < 1e-9);
        assert!((pricing.cache_read - 0.3).abs() < 1e-9);
        assert_eq!(pricing.cache_write, 0.0);
    }

    #[test]
    fn parse_model_scales_cache_write() {
        let mut m = sonnet_json();
        m["caching_price"] = json!(0.00000375);

        let pricing = parse_model(&m)
            .expect("model should parse")
            .pricing
            .expect("pricing should be parsed");
        assert!((pricing.cache_write - 3.75).abs() < 1e-9);
    }

    /// A price we cannot read must not collapse to an all-zero `ModelPricing`,
    /// which downstream reads as "free". Unknown has to stay unknown.
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

    #[test]
    fn parse_model_without_capability_flags_reports_none_supported() {
        let mut m = sonnet_json();
        m["supports_reasoning"] = json!(null);
        m["supports_vision"] = json!(null);

        let info = parse_model(&m).expect("model should parse");
        assert_eq!(info.supports_thinking, Some(false));
        assert_eq!(info.supports_vision, Some(false));
    }

    #[test_case("embedding" ; "embedding")]
    #[test_case("image"     ; "image")]
    fn parse_model_skips_non_chat_apis(api: &str) {
        let mut m = sonnet_json();
        m["api"] = json!(api);

        assert!(parse_model(&m).is_none());
    }

    #[test]
    fn parse_model_without_api_field_is_kept() {
        let mut m = sonnet_json();
        m["api"] = json!(null);

        assert!(parse_model(&m).is_some());
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
            ModelInfo::id_only("anthropic/claude-sonnet-4-5".into()),
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
                "anthropic/claude-sonnet-4-5",
                "openai/gpt-4o-mini",
            ]
        );
    }
}
