use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use maki_config::providers::{Protocol, ProviderPlan};
use maki_storage::id::SessionRef;
use reqwest::header::HeaderMap;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;

use crate::model::{FactSource, Model, ModelFamily, ThinkingSupport};
use crate::provider::{BoxFuture, Provider};
use crate::providers::aperture::NO_PATH_PREFIX;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GeneratedDocs, LoginConfig, Native, ProviderSpec,
};
use crate::{
    AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse, UsageLimit,
    dialect,
};

use super::{KeyHeader, KeyPool, KeyRotation, ResolvedAuth, Timeouts};

const SLUG: &str = "zai";
const DISPLAY_NAME: &str = "Z.AI";
const ENV_VAR: &str = "ZHIPU_API_KEY";
const BASE_URL: &str = "https://api.z.ai/api/paas/v4";
const CODING_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";
const DEFAULT_MODEL: &str = "zai/glm-5.3";
const CODING_MODEL: &str = "zai/glm-5.4";
const LOGIN_URL: &str = "https://z.ai/manage-apikey/apikey-list";
const MAX_TOKENS_FIELD: &str = "max_tokens";
const AUTH_NOTE: &str = "(shared across both endpoints)";
const MODELS_NOTE: &str = "The model list comes from the `/models` endpoint of the plan you log in with, so a model Z.AI adds shows up without a Maki update. A model missing from the table above reads its limits, vision and rates from models.dev, and its tier from the closest row (`glm-5.5` is strong like `glm-5`). The coding plan's models.dev entry lends limits and vision to a model only that plan serves so far, and never its zero rates.";

static CONFIG_STANDARD: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: Cow::Borrowed(SLUG),
    api_key_env: Cow::Borrowed(ENV_VAR),
    base_url: Cow::Borrowed(BASE_URL),
    max_tokens_field: Cow::Borrowed(MAX_TOKENS_FIELD),
    include_stream_usage: false,
    usage_cost: false,
    provider_name: Cow::Borrowed(DISPLAY_NAME),
};

const PLANS: &[(&str, ProviderPlan)] = &[
    (
        "standard",
        ProviderPlan {
            display_name: "Pay-as-you-go",
            base_url: BASE_URL,
            default_model: Some(DEFAULT_MODEL),
            login_url: None,
        },
    ),
    (
        "coding",
        ProviderPlan {
            display_name: "Coding plan",
            base_url: CODING_BASE_URL,
            default_model: Some(CODING_MODEL),
            login_url: None,
        },
    ),
];

/// The gateway gets no path prefix: Z.AI's API has no `/v1` segment at all
/// (`/api/paas/v4/chat/completions`), so the upstream base url must carry the
/// whole path and any prefix would double it up.
pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Glm,
    supports_thinking: false,
    supports_deferred_tools: false,
    sends_thinking: |_, choice| dialect::GLM.sends(choice),
    thinking_fields_fallback: |_| None,
    accepts_arbitrary_models: false,
    catalog_is_authoritative: false,
    fallback_max_output: Some(16_000),
    fallback_context_window: 128_000,
    models_toml: include_str!("../../../models/zai.toml"),
    pricing_schedule: None,
    build: Build::Native(Native {
        new: create,
        with_auth: create_with_auth,
    }),
    aperture: Some(ApertureRoute {
        path_prefix: NO_PATH_PREFIX,
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
        api_urls: &[BASE_URL, CODING_BASE_URL],
        features: None,
        auth: AuthDoc::EnvVarWith(AUTH_NOTE),
        catalog: CatalogDoc::Table,
        trailing_notes: &[MODELS_NOTE],
    },
};

fn create(timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    Ok(Box::new(Zai::new(timeouts)?))
}

fn create_with_auth(
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
) -> Box<dyn Provider> {
    Box::new(Zai::with_auth(auth, timeouts).with_system_prefix(system_prefix))
}

const QUOTA_LIMIT_URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";

/// First GLM that takes thinking parameters at all. A floor instead of a prefix
/// allowlist, so the next GLM gets thinking without us shipping a release.
const THINKING_SINCE: (u32, u32) = (5, 2);
const GLM_PREFIX: &str = "glm-";

#[derive(Deserialize)]
struct QuotaResponse {
    data: QuotaData,
}

#[derive(Deserialize, Default)]
struct QuotaData {
    #[serde(default)]
    limits: Vec<QuotaLimit>,
    #[serde(default)]
    level: Option<String>,
}

#[derive(Deserialize)]
struct QuotaLimit {
    #[serde(rename = "type")]
    kind: String,
    unit: u32,
    percentage: u32,
    #[serde(default, rename = "nextResetTime")]
    next_reset_time: Option<u64>,
}

fn quota_label(kind: &str, unit: u32) -> String {
    match (kind, unit) {
        ("TOKENS_LIMIT", 3) => "5-hour tokens".into(),
        ("TOKENS_LIMIT", 6) => "Weekly tokens".into(),
        ("TIME_LIMIT", _) => "Subscription time".into(),
        _ => format!("{kind} #{unit}"),
    }
}

impl From<QuotaResponse> for ProviderUsage {
    fn from(resp: QuotaResponse) -> Self {
        ProviderUsage {
            plan: resp.data.level,
            limits: resp
                .data
                .limits
                .into_iter()
                .map(|l| UsageLimit {
                    label: quota_label(&l.kind, l.unit),
                    percentage: Some(l.percentage),
                    reset_at: l.next_reset_time,
                    detail: None,
                })
                .collect(),
            by_model_today: vec![],
        }
    }
}

inventory::submit!(SPEC.config_row());

pub struct Zai {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

impl Zai {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve("zai", &CONFIG_STANDARD.api_key_env)?;
        let mut auth = ResolvedAuth::bearer("zai", pool.current())?;
        let provider_config = maki_config::providers::ProvidersConfig::load();
        if let Some(url) =
            maki_config::providers::resolve_base_url("zai", provider_config.get("zai"))
        {
            auth.base_url = Some(url);
        }
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG_STANDARD, timeouts),
            auth: Arc::new(Mutex::new(auth)),
            key_pool: Some(pool),
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: super::Timeouts) -> Self {
        Self {
            compat: OpenAiCompatProvider::new(&CONFIG_STANDARD, timeouts),
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

impl Provider for Zai {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a UnboundedSender<ProviderEvent>,
        opts: RequestOptions,
        _session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let mut body =
                self.compat
                    .build_body(model, messages, system, tools, opts.thinking, auth.top_p);
            if model.supports_thinking() {
                opts.thinking
                    .apply_reasoning_effort(&mut body, &dialect::GLM, model);
            }
            match self
                .compat
                .do_stream(model, &HeaderMap::new(), &body, event_tx, &auth)
                .await
            {
                Err(AgentError::Api {
                    status, message, ..
                }) if (status == 429 || status >= 500)
                    && (message.contains("1113") || message.contains("nsufficien")) =>
                {
                    warn!(status, "insufficient funds, bailing out");
                    Err(AgentError::api(402, message))
                }
                result => result,
            }
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
            let body = self.compat.get_text(&auth, QUOTA_LIMIT_URL).await?;
            let parsed: QuotaResponse = serde_json::from_str(&body)?;
            Ok(Some(parsed.into()))
        })
    }

    fn keys(&self) -> Option<KeyRotation<'_>> {
        KeyRotation::over(&self.key_pool, &self.auth, KeyHeader::Bearer)
    }

    fn adjust_model(&self, model: &mut Model) {
        adjust_model(model);
    }
}

/// `glm-5.3-flash` -> `(5, 3)`, `glm-5-code` -> `(5, 0)`, `glm-5.4v` ->
/// `(5, 4)`. Only the version token counts, so a date stamp never reads as a
/// minor (`glm-5-20260101` is `(5, 0)`).
fn glm_version(model_id: &str) -> Option<(u32, u32)> {
    let version = model_id.strip_prefix(GLM_PREFIX)?.split('-').next()?;
    let (major, minor) = version.split_once('.').unwrap_or((version, "0"));
    let number = |part: &str| part.trim_end_matches(char::is_alphabetic).parse().ok();
    Some((number(major)?, number(minor)?))
}

fn adjust_model(model: &mut Model) {
    let Some(version) = glm_version(&model.id) else {
        return;
    };
    if version >= THINKING_SINCE {
        model.thinking_override = Some(ThinkingSupport::Yes);
        model.sources.thinking = FactSource::Default;
    }
}

#[cfg(test)]
mod tests {
    use maki_storage::StateDir;
    use test_case::test_case;

    use super::*;
    use crate::model::ModelTier;
    use crate::model_registry;
    use crate::providers::catalog::seed_catalog_for_tests;

    const SAMPLE_BODY: &str = r#"{"code":200,"data":{"limits":[
        {"type":"TOKENS_LIMIT","unit":3,"percentage":16,"nextResetTime":1777819631597},
        {"type":"TOKENS_LIMIT","unit":6,"percentage":4,"nextResetTime":1778262784969},
        {"type":"TIME_LIMIT","unit":5,"percentage":0,"nextResetTime":1780336384978}
    ],"level":"lite"}}"#;

    const LONG_CONTEXT: u32 = 1_000_000;
    const GLM_5_CONTEXT: u32 = 200_000;
    const UNCURATED_TEXT_MODEL: &str = "glm-5.5";
    const UNCURATED_VISION_MODEL: &str = "glm-5.5-flash";
    const UNCURATED_TEXT_CONTEXT: u32 = 256_000;
    const UNCURATED_OUTPUT: u32 = 64_000;

    #[test_case("zai/glm-5.3", Some(ThinkingSupport::Yes) ; "glm_5_3_supports_thinking")]
    #[test_case("zai/glm-5.3-flash", Some(ThinkingSupport::Yes) ; "glm_5_3_flash_supports_thinking")]
    #[test_case("zai/glm-5.4", Some(ThinkingSupport::Yes) ; "glm_5_4_supports_thinking")]
    #[test_case("zai/glm-5.2", Some(ThinkingSupport::Yes) ; "glm_5_2_supports_thinking")]
    #[test_case("zai/glm-5.1", None ; "glm_5_1_no_thinking")]
    #[test_case("zai/glm-5-code", None ; "glm_5_code_reads_as_5_0")]
    #[test_case("zai/glm-4.7", None ; "glm_4_7_no_thinking")]
    fn adjust_model_sets_thinking_support(spec: &str, expected: Option<ThinkingSupport>) {
        let mut model = Model::from_spec(spec).unwrap();
        adjust_model(&mut model);
        assert_eq!(model.thinking_override, expected);
    }

    #[test_case("glm-5.3", Some((5, 3)) ; "main_line")]
    #[test_case("glm-5.3-flash", Some((5, 3)) ; "variant")]
    #[test_case("glm-5-code", Some((5, 0)) ; "bare_major_with_variant")]
    #[test_case("glm-6", Some((6, 0)) ; "bare_major")]
    #[test_case("glm-5.4v", Some((5, 4)) ; "vision_suffix")]
    #[test_case("glm-5.1-20260101", Some((5, 1)) ; "date_stamp_is_not_the_minor")]
    #[test_case("glm-5-20260101", Some((5, 0)) ; "date_stamp_after_bare_major")]
    #[test_case("claude-opus-4-8", None ; "foreign_id")]
    fn glm_version_parses(id: &str, expected: Option<(u32, u32)>) {
        assert_eq!(glm_version(id), expected);
    }

    #[test_case("zai/glm-5.3", LONG_CONTEXT ; "glm_5_3_1m_context")]
    #[test_case("zai/glm-5.3-flash", LONG_CONTEXT ; "glm_5_3_flash_1m_context")]
    #[test_case("zai/glm-5.1", GLM_5_CONTEXT ; "glm_5_1_keeps_200k")]
    #[test_case("zai/glm-5.4", LONG_CONTEXT ; "glm_5_4_has_its_own_1m_row")]
    #[test_case("zai/glm-5", GLM_5_CONTEXT ; "glm_5_keeps_its_own_200k_row")]
    fn model_entry_context_window(spec: &str, expected: u32) {
        assert_eq!(Model::from_spec(spec).unwrap().context_window, expected);
    }

    /// A GLM nobody curated yet, listed only under the coding plan, the way
    /// glm-5.4 launched. Its limits and vision come from models.dev, and the
    /// provider adjusts nothing the catalog states.
    #[test_case(UNCURATED_TEXT_MODEL, UNCURATED_TEXT_CONTEXT, false ; "text_only_release")]
    #[test_case(UNCURATED_VISION_MODEL, LONG_CONTEXT, true ; "multimodal_release")]
    fn uncurated_glm_reads_the_coding_plan_catalog(id: &str, context: u32, vision: bool) {
        let tmp = tempfile::tempdir().unwrap();
        let index = serde_json::from_str(&uncurated_plan_catalog()).unwrap();
        seed_catalog_for_tests(index, StateDir::from_path(tmp.path().to_path_buf()));

        let mut model = Model::from_spec(&format!("{SLUG}/{id}")).unwrap();
        adjust_model(&mut model);
        assert_eq!(model.context_window, context);
        assert_eq!(model.max_output_tokens, Some(UNCURATED_OUTPUT));
        assert_eq!(model.supports_vision(), vision);
        assert!(
            !model.pricing.is_zero(),
            "a prepaid plan's zero is not a rate"
        );
    }

    fn uncurated_plan_catalog() -> String {
        format!(
            r#"{{"zai-coding-plan": {{"name": "Z.AI Coding Plan", "npm": "@ai-sdk/openai-compatible",
            "api": "{CODING_BASE_URL}", "models": {{
                "{UNCURATED_TEXT_MODEL}": {{"limit": {{"context": {UNCURATED_TEXT_CONTEXT}, "output": {UNCURATED_OUTPUT}}},
                    "cost": {{"input": 0, "output": 0}}, "modalities": {{"input": ["text"]}}}},
                "{UNCURATED_VISION_MODEL}": {{"limit": {{"context": {LONG_CONTEXT}, "output": {UNCURATED_OUTPUT}}},
                    "cost": {{"input": 0, "output": 0}}, "modalities": {{"input": ["text", "image"]}}}}
            }}}}}}"#
        )
    }

    /// No published per-token price, so the row states none, and the model is
    /// neither free nor metered until models.dev prices it.
    #[test]
    fn unpriced_row_is_unknown_rather_than_free() {
        let model = Model::from_spec("zai/glm-5.4").unwrap();
        assert!(model.pricing.is_zero());
        assert_eq!(model.free(), None);
    }

    /// Both plans share these, so each must be callable by a pay-as-you-go
    /// key: the coding-plan-only glm-5.4 is none of them.
    #[test_case(ModelTier::Strong, "zai/glm-5.3" ; "strong_default")]
    #[test_case(ModelTier::Medium, "zai/glm-5.3-flash" ; "medium_default")]
    #[test_case(ModelTier::Weak, "zai/glm-4.7-flash" ; "weak_default")]
    fn slot_defaults(tier: ModelTier, expected: &str) {
        assert_eq!(
            model_registry::spec_for_tier("zai", tier),
            Some(expected.into())
        );
    }

    /// The longest prefix wins, so without its own entry the cheap flash model
    /// would answer as the strong `glm-5.3` and get billed like it.
    #[test]
    fn glm_5_3_flash_does_not_ride_the_glm_5_3_entry() {
        let flash = Model::from_spec("zai/glm-5.3-flash").unwrap();
        assert_eq!(flash.tier, ModelTier::Medium);
        assert!(flash.supports_vision());
    }

    /// These two shared `glm-5`'s row and so billed at its cheaper rate. That
    /// hurt: `glm-5.1` is the provider default, so a plain session under-
    /// reported what it spent.
    #[test_case("zai/glm-5.1" ; "glm_5_1_bills_above_glm_5")]
    #[test_case("zai/glm-5.2" ; "glm_5_2_bills_above_glm_5")]
    fn glm_5_x_does_not_ride_the_glm_5_entry(spec: &str) {
        let glm_5 = Model::from_spec("zai/glm-5").unwrap().pricing;
        let pricing = Model::from_spec(spec).unwrap().pricing;
        assert!(pricing.input > glm_5.input);
        assert!(pricing.output > glm_5.output);
        assert!(pricing.cache_read > glm_5.cache_read);
    }

    #[test]
    fn parse_quota_response() {
        let parsed: QuotaResponse = serde_json::from_str(SAMPLE_BODY).unwrap();
        let usage: ProviderUsage = parsed.into();
        assert_eq!(usage.plan.as_deref(), Some("lite"));
        assert_eq!(usage.limits.len(), 3);
        assert_eq!(usage.limits[0].label, "5-hour tokens");
        assert_eq!(usage.limits[0].percentage, Some(16));
        assert_eq!(usage.limits[0].reset_at, Some(1777819631597));
        assert_eq!(usage.limits[1].label, "Weekly tokens");
        assert_eq!(usage.limits[2].label, "Subscription time");
        assert_eq!(usage.limits[2].reset_at, Some(1780336384978));
    }

    #[test]
    fn parse_quota_unknown_unit_falls_back() {
        let body = r#"{"code":200,"data":{"limits":[
            {"type":"TOKENS_LIMIT","unit":9,"percentage":50}
        ]}}"#;
        let parsed: QuotaResponse = serde_json::from_str(body).unwrap();
        let usage: ProviderUsage = parsed.into();
        assert!(usage.plan.is_none());
        assert_eq!(usage.limits[0].label, "TOKENS_LIMIT #9");
        assert_eq!(usage.limits[0].percentage, Some(50));
        assert_eq!(usage.limits[0].reset_at, None);
    }
}
