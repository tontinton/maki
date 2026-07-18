use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_storage::id::SessionRef;
use serde_json::{Value, json};

use crate::model::{Model, ModelEntry, ModelInfo, ModelPricing};
use crate::provider::{BoxFuture, Provider};
use crate::{
    AgentError, Effort, EffortDialect, Message, ProviderEvent, RequestOptions, StreamResponse,
    dialect,
};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyPool, ResolvedAuth};

const REFERER: &str = "https://maki.sh";
const APP_TITLE: &str = "maki";
const PER_MILLION: f64 = 1_000_000.0;

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
    reasoning_efforts: Vec<Effort>,
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

/// OpenRouter models come in three reasoning states, encoded here as a
/// dialect so `effort_str` can resolve them like any other provider:
/// 1. mandatory - always on; Off sends nothing (can't disable).
/// 2. default_enabled - on by default; Off sends effort "none".
/// 3. default off - Off sends nothing; any effort string turns it on.
fn effort_dialect(info: Option<&OpenRouterModelInfo>) -> EffortDialect<'_> {
    let Some(info) = info else {
        return dialect::PREFER_HIGH;
    };
    EffortDialect {
        supported: match info.reasoning_efforts.as_slice() {
            [] => dialect::PREFER_HIGH.supported,
            declared => declared,
        },
        off: (info.reasoning_default_enabled && !info.reasoning_mandatory).then_some(dialect::OFF),
        ..dialect::PREFER_HIGH
    }
}

fn parse_model(m: &Value) -> Option<ModelInfo> {
    // Filter: only text input/output models
    let architecture = m["architecture"].as_object()?;
    let input_modalities = architecture["input_modalities"].as_array()?;
    let output_modalities = architecture["output_modalities"].as_array()?;

    let has_text_input = input_modalities.iter().any(|m| m.as_str() == Some("text"));
    let has_text_output = output_modalities.iter().any(|m| m.as_str() == Some("text"));
    if !has_text_input || !has_text_output {
        return None;
    }

    let supports_vision = input_modalities.iter().any(|m| m.as_str() == Some("image"));

    // Parse with OpenRouter-specific pricing field names. OpenRouter reports
    // per-token prices; scale to $/M as `ModelPricing` expects.
    let id = m["id"].as_str()?;
    let context_window = m["context_length"]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok());
    let per_token =
        |p: &Value| -> Option<f64> { Some(p.as_str()?.parse::<f64>().ok()? * PER_MILLION) };
    let pricing = m["pricing"]
        .as_object()
        .and_then(|p| {
            Some(ModelPricing {
                input: per_token(p.get("prompt")?)?,
                output: per_token(p.get("completion")?)?,
                cache_write: p
                    .get("input_cache_write")
                    .and_then(per_token)
                    .unwrap_or(0.0),
                cache_read: p.get("input_cache_read").and_then(per_token).unwrap_or(0.0),
                fast: None,
            })
        })
        .unwrap_or_default();

    let reasoning = m
        .get("reasoning")
        .and_then(|v| v.as_object())
        .map(|v| OpenRouterModelInfo {
            reasoning_mandatory: v.get("mandatory").and_then(Value::as_bool) == Some(true),
            reasoning_default_enabled: v.get("default_enabled").and_then(Value::as_bool)
                == Some(true),
            reasoning_efforts: v
                .get("supported_efforts")
                .and_then(Value::as_array)
                .map(|arr| {
                    let mut efforts: Vec<Effort> = arr
                        .iter()
                        .filter_map(|v| v.as_str()?.parse().ok())
                        .collect();
                    efforts.sort_unstable();
                    efforts
                })
                .unwrap_or_default(),
        });

    let supports_thinking = reasoning.is_some()
        || m.get("supported_parameters")
            .and_then(|v| v.as_array())
            .is_some_and(|v| v.iter().any(|v| v.as_str() == Some("reasoning")));

    Some(ModelInfo {
        id: id.to_string(),
        context_window,
        max_output_tokens: None,
        pricing: Some(pricing),
        supports_thinking: Some(supports_thinking),
        supports_vision: Some(supports_vision),
        provider_info: reasoning.map(|r| Arc::new(r) as Arc<dyn std::any::Any + Send + Sync>),
    })
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
        session_id: Option<&'a SessionRef>,
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

            let effort_dialect = effort_dialect(reasoning_info.as_deref());
            if model.supports_thinking()
                && let Some(effort) = opts.thinking.effort_str(&effort_dialect, model)
            {
                body["reasoning"] = json!({"effort": effort});
            }

            if let Some(sid) = session_id {
                body["session_id"] = json!(sid.to_string());
            }

            let extra_headers = [("HTTP-Referer", REFERER), ("X-OpenRouter-Title", APP_TITLE)];
            self.compat
                .do_stream(model, &extra_headers, &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            self.compat.fetch_and_parse_models(&auth, parse_model).await
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

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::ThinkingConfig;

    fn kimi_k3_json() -> Value {
        json!({
            "id": "moonshotai/kimi-k3",
            "context_length": 1_048_576,
            "architecture": {
                "input_modalities": ["text", "image"],
                "output_modalities": ["text"],
            },
            "pricing": {
                "prompt": "0.000003",
                "completion": "0.000015",
                "input_cache_read": "0.0000003",
            },
            "supported_parameters": ["reasoning"],
        })
    }

    #[test]
    fn parse_model_scales_pricing_to_per_million() {
        let info = parse_model(&kimi_k3_json()).expect("model should parse");

        assert_eq!(info.id, "moonshotai/kimi-k3");
        assert_eq!(info.context_window, Some(1_048_576));
        assert_eq!(info.supports_vision, Some(true));
        assert_eq!(info.supports_thinking, Some(true));
        let pricing = info.pricing.expect("pricing should be parsed");
        assert_eq!(pricing.input, 3.0);
        assert_eq!(pricing.output, 15.0);
        assert_eq!(pricing.cache_read, 0.3);
        assert_eq!(pricing.cache_write, 0.0);
    }

    #[test]
    fn parse_model_scales_cache_write() {
        let mut m = kimi_k3_json();
        m["pricing"]["input_cache_write"] = json!("0.00000375");

        let pricing = parse_model(&m)
            .expect("model should parse")
            .pricing
            .expect("pricing should be parsed");
        assert_eq!(pricing.cache_write, 3.75);
    }

    #[test]
    fn parse_model_reasoning_efforts_skips_unknown_and_sorts() {
        let mut m = kimi_k3_json();
        m["reasoning"] = json!({
            "mandatory": false,
            "default_enabled": true,
            "supported_efforts": ["high", "bogus", "low", "none"],
        });

        let info = parse_model(&m).expect("model should parse");
        let provider_info = info.provider_info.expect("reasoning info should be set");
        let reasoning = provider_info
            .downcast_ref::<OpenRouterModelInfo>()
            .expect("wrong provider info type");
        assert!(reasoning.reasoning_default_enabled);
        assert!(!reasoning.reasoning_mandatory);
        assert_eq!(reasoning.reasoning_efforts, vec![Effort::Low, Effort::High]);
    }

    fn openrouter_model(info: Option<&OpenRouterModelInfo>) -> (EffortDialect<'_>, Model) {
        let model = Model {
            id: "test-model".into(),
            provider: crate::provider::ProviderKind::OpenRouter,
            dynamic_slug: None,
            tier: crate::model::ModelTier::Medium,
            family: crate::model::ModelFamily::Generic,
            supports_tool_examples_override: None,
            supports_thinking_override: None,
            supports_vision_override: None,
            pricing: ModelPricing::default(),
            max_output_tokens: Some(8192),
            context_window: 200_000,
        };
        (effort_dialect(info), model)
    }

    fn reasoning_info(efforts: &[Effort]) -> OpenRouterModelInfo {
        OpenRouterModelInfo {
            reasoning_mandatory: false,
            reasoning_default_enabled: false,
            reasoning_efforts: efforts.to_vec(),
        }
    }

    #[test_case(&[Effort::High, Effort::XHigh], ThinkingConfig::Effort(Effort::XHigh), "xhigh" ; "declared_xhigh_passes_through")]
    #[test_case(&[Effort::High, Effort::XHigh], ThinkingConfig::Effort(Effort::Max),   "xhigh" ; "max_snaps_to_declared_xhigh")]
    #[test_case(&[Effort::Minimal, Effort::Low], ThinkingConfig::Adaptive,             "low"   ; "adaptive_snaps_into_declared")]
    #[test_case(&[], ThinkingConfig::Effort(Effort::XHigh), "high" ; "no_declared_falls_back_to_static")]
    fn effort_dialect_snaps_once_against_declared_levels(
        efforts: &[Effort],
        config: ThinkingConfig,
        expected: &str,
    ) {
        let info = reasoning_info(efforts);
        let (dialect, model) = openrouter_model(Some(&info));
        assert_eq!(config.effort_str(&dialect, &model), Some(expected));
    }

    #[test]
    fn no_reasoning_info_still_requests_high_effort() {
        let (dialect, model) = openrouter_model(None);
        assert_eq!(
            ThinkingConfig::Adaptive.effort_str(&dialect, &model),
            Some("high")
        );
    }

    #[test_case(false, false, None         ; "default_off_sends_nothing")]
    #[test_case(true,  false, Some("none") ; "default_enabled_disables_with_none")]
    #[test_case(true,  true,  None         ; "mandatory_cannot_be_disabled")]
    fn off_resolves_per_reasoning_flags(
        default_enabled: bool,
        mandatory: bool,
        expected: Option<&str>,
    ) {
        let info = OpenRouterModelInfo {
            reasoning_mandatory: mandatory,
            reasoning_default_enabled: default_enabled,
            reasoning_efforts: vec![],
        };
        let (dialect, model) = openrouter_model(Some(&info));
        assert_eq!(ThinkingConfig::Off.effort_str(&dialect, &model), expected);
    }

    #[test_case(json!(["image"]), json!(["image"]); "image_only")]
    #[test_case(json!(["image"]), json!(["text"]); "image_input_only")]
    #[test_case(json!(["text"]), json!(["image"]); "image_output_only")]
    fn parse_model_skips_non_text_models(input: Value, output: Value) {
        let mut m = kimi_k3_json();
        m["architecture"]["input_modalities"] = input;
        m["architecture"]["output_modalities"] = output;

        assert!(parse_model(&m).is_none());
    }
}
