use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_storage::id::SessionRef;
use serde_json::{Value, json};

use maki_config::providers::Protocol;

use crate::model::{Model, ModelFamily, ModelInfo, ModelPricing};
use crate::provider::{BoxFuture, Provider};
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, CatalogDoc, GENERIC_DISCOVERY_NOTE, GeneratedDocs, LoginConfig,
    NO_CURATED_MODELS, Native, ProviderSpec,
};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, dialect};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyHeader, KeyPool, KeyRotation, ResolvedAuth, Timeouts, deepseek};

/// TensorX namespaces resold models by vendor, so DeepSeek ids arrive as
/// `deepseek/deepseek-flash`.
const DEEPSEEK_VENDOR_PREFIX: &str = "deepseek/";

const SLUG: &str = "tensorx";
const DISPLAY_NAME: &str = "TensorX";
const ENV_VAR: &str = "TENSORX_API_KEY";
const BASE_URL: &str = "https://api.tensorx.ai/v1";
const DEFAULT_MODEL: &str = "tensorx/z-ai/glm-5.2";
const LOGIN_URL: &str = "https://tensorx.ai";
const MAX_TOKENS_FIELD: &str = "max_tokens";
const FEATURES: &str = "Open-weight models, zero data retention, prompt caching";

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: SLUG,
    api_key_env: ENV_VAR,
    base_url: BASE_URL,
    max_tokens_field: MAX_TOKENS_FIELD,
    include_stream_usage: true,
    provider_name: DISPLAY_NAME,
};

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: None,
    fallback_context_window: 200_000,
    models_toml: NO_CURATED_MODELS,
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
        plans: None,
        login_url: Some(LOGIN_URL),
        needs_url: false,
    }),
    docs: GeneratedDocs {
        api_urls: &[BASE_URL],
        features: Some(FEATURES),
        auth: AuthDoc::EnvVar,
        catalog: CatalogDoc::Discovered(GENERIC_DISCOVERY_NOTE),
        trailing_notes: &[],
    },
};

fn create(timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    Ok(Box::new(TensorX::new(timeouts)?))
}

fn create_with_auth(
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
) -> Box<dyn Provider> {
    Box::new(TensorX::with_auth(auth, timeouts).with_system_prefix(system_prefix))
}

inventory::submit!(SPEC.config_row());

#[derive(Debug)]
struct TensorXModelInfo {
    has_thinking: bool,
    has_reasoning_effort: bool,
}

pub struct TensorX {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

impl TensorX {
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

impl Provider for TensorX {
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

            let (has_thinking, has_reasoning_effort) =
                crate::model_registry::provider_info::<TensorXModelInfo>("tensorx", &model.id)
                    .map_or((false, false), |info| {
                        (info.has_thinking, info.has_reasoning_effort)
                    });

            if has_thinking {
                body["thinking"] = json!(opts.thinking.is_enabled());
            }
            if has_reasoning_effort {
                opts.thinking
                    .apply_reasoning_effort(&mut body, &dialect::TENSORX, model);
            }
            // DeepSeek takes the toggle through the chat template and TensorX
            // advertises neither knob for it. Sharing DeepSeek's own predicate
            // means a rename upstream cannot quietly turn thinking off here.
            else if !has_thinking
                && opts.thinking.is_enabled()
                && model
                    .id
                    .strip_prefix(DEEPSEEK_VENDOR_PREFIX)
                    .is_some_and(deepseek::uses_v4_thinking_protocol)
            {
                body["chat_template_kwargs"] = json!({"thinking": true});
            }

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
                .map(|arr| arr.iter().filter_map(model_info).collect())
                .unwrap_or_default();
            models.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(models)
        })
    }

    fn keys(&self) -> Option<KeyRotation<'_>> {
        Some(KeyRotation::new(
            self.key_pool.as_ref()?,
            &self.auth,
            KeyHeader::Bearer,
        ))
    }
}

/// One entry of `/model/info`. `None` for anything that is not a chat model.
fn model_info(entry: &Value) -> Option<ModelInfo> {
    let id = entry["model_name"].as_str()?;
    let info = entry.get("model_info")?;

    let mode_ok = info
        .get("mode")
        .and_then(|v| v.as_str())
        .is_none_or(|m| m == "chat");
    if !mode_ok {
        return None;
    }

    let context_window = info["max_tokens"]
        .as_u64()
        .or_else(|| info["max_input_tokens"].as_u64())
        .and_then(|v| u32::try_from(v).ok());

    // This endpoint enforces `input + max_output <= context_window`, which used
    // to make reporting the real cap fatal: the agent asked for whatever the
    // window had left, so any undercount of the prompt put the sum over. The
    // ask is a flat turn budget now, clamped down to this number, so the cap is
    // safe to report again.
    let max_output_tokens = info["max_output_tokens"]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok());

    let input_cost = info["input_cost_per_token"].as_f64();
    let output_cost = info["output_cost_per_token"].as_f64();
    let pricing = if input_cost.is_some() || output_cost.is_some() {
        let per_million = 1_000_000.0;
        Some(ModelPricing::per_million(
            input_cost.unwrap_or(0.0) * per_million,
            output_cost.unwrap_or(0.0) * per_million,
            info["cache_creation_input_token_cost"]
                .as_f64()
                .unwrap_or(0.0)
                * per_million,
            info["cache_read_input_token_cost"].as_f64().unwrap_or(0.0) * per_million,
        ))
    } else {
        None
    };

    let supports_vision = info
        .get("supports_vision")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let supports_thinking = info.get("supports_reasoning").and_then(Value::as_bool);

    let supported_params = info
        .get("supported_openai_params")
        .and_then(Value::as_array)
        .map(|params| TensorXModelInfo {
            has_thinking: params.iter().any(|v| v.as_str() == Some("thinking")),
            has_reasoning_effort: params
                .iter()
                .any(|v| v.as_str() == Some("reasoning_effort")),
        });

    Some(ModelInfo {
        id: id.to_string(),
        context_window,
        max_output_tokens,
        pricing,
        supports_thinking,
        supports_vision: Some(supports_vision),
        tier: None,
        provider_info: supported_params
            .map(|p| Arc::new(p) as Arc<dyn std::any::Any + Send + Sync>),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const WINDOW: u32 = 1_048_576;
    const INPUT_WINDOW: u32 = 131_072;
    const OUTPUT_CAP: u32 = 262_144;

    fn entry(info: Value) -> Value {
        json!({ "model_name": "kimi-k3", "model_info": info })
    }

    /// The output cap was hard-coded to `None` while reporting it was fatal,
    /// which left the model picker and the thinking math with no number at all.
    #[test_case(
        json!({ "mode": "chat", "max_tokens": WINDOW, "max_input_tokens": INPUT_WINDOW, "max_output_tokens": OUTPUT_CAP }),
        (Some(WINDOW), Some(OUTPUT_CAP))
        ; "max_tokens_wins_over_max_input_tokens"
    )]
    #[test_case(
        json!({ "max_input_tokens": INPUT_WINDOW }),
        (Some(INPUT_WINDOW), None)
        ; "an_unstated_window_falls_back_to_the_input_window"
    )]
    #[test_case(json!({}), (None, None) ; "nothing_stated_leaves_the_choice_to_the_provider")]
    fn the_window_and_the_cap_are_read_off_the_entry(
        info: Value,
        expected: (Option<u32>, Option<u32>),
    ) {
        let model = model_info(&entry(info)).expect("a chat model is listed");
        assert_eq!((model.context_window, model.max_output_tokens), expected);
    }

    #[test]
    fn models_that_do_not_chat_are_skipped() {
        assert!(model_info(&entry(json!({ "mode": "embedding" }))).is_none());
    }
}
