use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_storage::StateDir;
use maki_storage::id::SessionRef;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, warn};

use crate::model::Model;
use crate::provider::{BoxFuture, Provider};
use crate::types::EffortDialect;
use crate::{
    AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse, UsageLimit,
    dialect,
};

use super::auth;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::providers::{ResolvedAuth, refreshed_tokens};

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "openai",
    api_key_env: "OPENAI_API_KEY",
    base_url: "https://api.openai.com/v1",
    max_tokens_field: "max_completion_tokens",
    include_stream_usage: true,
    provider_name: "OpenAI",
};

// Non-codex models OpenAI offers for subscription usage via the Coding Plan.
// Codex models are matched by their `-codex` substring in
// `coding_plan_context_window`, so they never need listing here.
pub(crate) const PLAN_MODELS: &[&str] = &[
    "gpt-6-astra",
    "gpt-5.6-luna",
    "gpt-5.6-terra",
    "gpt-5.6-sol",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.2",
];

const CODEX_PLAN_CONTEXT_WINDOW: u32 = 272_000;
const GPT_5_6_PLAN_CONTEXT_WINDOW: u32 = 372_000;
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const EMPTY_USAGE_ERROR: &str =
    "OpenAI usage response contained no plan or rate limits; the endpoint schema likely changed";
const MILLIS_PER_SECOND: u64 = 1_000;
const SECONDS_PER_HOUR: u64 = 60 * 60;
const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;
const SECONDS_PER_WEEK: u64 = 7 * SECONDS_PER_DAY;

fn is_codex_model(model_id: &str) -> bool {
    coding_plan_context_window(model_id).is_some()
}

// Codex models match by substring so future releases route without a registry
// edit; the named non-codex plans match exactly to avoid catching near-misses
// like `gpt-5.6-terra-preview`.
fn coding_plan_context_window(model_id: &str) -> Option<u32> {
    if model_id.contains("-codex") {
        return Some(CODEX_PLAN_CONTEXT_WINDOW);
    }
    if !PLAN_MODELS.contains(&model_id) {
        return None;
    }
    Some(if model_id.starts_with("gpt-5.6-") {
        GPT_5_6_PLAN_CONTEXT_WINDOW
    } else {
        CODEX_PLAN_CONTEXT_WINDOW
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CodexUsage {
    plan_type: Option<String>,
    rate_limit: CodexRateLimit,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CodexRateLimit {
    primary_window: Option<CodexUsageWindow>,
    secondary_window: Option<CodexUsageWindow>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CodexUsageWindow {
    used_percent: Option<f64>,
    limit_window_seconds: Option<u64>,
    reset_at: Option<u64>,
}

pub struct OpenAi {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    storage: Option<StateDir>,
    system_prefix: Option<String>,
    /// Env / `providers.toml` override for the platform API, resolved once at
    /// construction. Used by the Responses (codex) path only; ChatGPT Coding
    /// Plan OAuth keeps its fixed backend URL.
    resolved_base_url: Option<String>,
}

impl OpenAi {
    pub fn new(timeouts: crate::providers::Timeouts) -> Result<Self, AgentError> {
        let storage = StateDir::resolve()?;
        let resolved = auth::resolve(&storage)?;
        let compat = OpenAiCompatProvider::new(&CONFIG, timeouts);
        Ok(Self {
            resolved_base_url: resolve_openai_base_url(),
            compat,
            auth: Arc::new(Mutex::new(resolved)),
            storage: Some(storage),
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<ResolvedAuth>>,
        timeouts: crate::providers::Timeouts,
    ) -> Self {
        Self {
            resolved_base_url: resolve_openai_base_url(),
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth,
            storage: None,
            system_prefix: None,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    fn current_auth(&self) -> ResolvedAuth {
        self.auth.lock().unwrap().clone()
    }

    fn is_oauth(&self) -> bool {
        self.storage.as_ref().is_some_and(auth::is_oauth)
    }

    async fn refresh_oauth(&self) -> Result<(), AgentError> {
        let storage = self.storage.clone().ok_or_else(|| AgentError::Config {
            message: "OAuth refresh not available for externally-managed auth".into(),
        })?;
        let rejected = self.auth.lock().unwrap().access_token().map(str::to_owned);
        let resolved = smol::unblock(move || {
            match refreshed_tokens(
                &storage,
                auth::PROVIDER,
                rejected.as_deref(),
                auth::refresh_tokens,
            ) {
                Ok(fresh) => auth::build_oauth_resolved(&fresh),
                Err(e) => {
                    warn!(error = %e, "OpenAI OAuth refresh failed, clearing stale tokens");
                    let _ = maki_storage::auth::delete_tokens(&storage, auth::PROVIDER);
                    Err(e)
                }
            }
        })
        .await?;
        *self.auth.lock().unwrap() = resolved;
        debug!("refreshed OpenAI OAuth token");
        Ok(())
    }

    async fn with_oauth_retry<T, F, Fut>(&self, f: F) -> Result<T, AgentError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, AgentError>>,
    {
        let result = f().await;
        if self.is_oauth()
            && matches!(&result, Err(e) if e.is_auth_error())
            && self.refresh_oauth().await.is_ok()
        {
            return f().await;
        }
        result
    }

    fn codex_auth(&self) -> Result<ResolvedAuth, AgentError> {
        // Prefer OAuth tokens for the ChatGPT Coding Plan backend.
        if let Some(storage) = self.storage.as_ref()
            && let Some(tokens) = maki_storage::auth::load_tokens(storage, auth::PROVIDER)
        {
            return auth::build_coding_plan_resolved(&tokens);
        }
        // Fall back to standard API key via the Responses API. Env /
        // providers.toml base_url overrides the platform API only, never the
        // ChatGPT backend above.
        let mut auth = self.current_auth();
        if auth.base_url.is_none() {
            auth.base_url = self
                .resolved_base_url
                .clone()
                .or_else(|| Some(CONFIG.base_url.into()));
        }
        Ok(auth)
    }
}

fn usage_percentage(percentage: f64) -> Option<u32> {
    percentage
        .is_finite()
        .then(|| percentage.round().clamp(0.0, 100.0) as u32)
}

fn usage_label(seconds: u64) -> String {
    if seconds == SECONDS_PER_WEEK {
        return "Weekly usage".into();
    }
    if seconds == SECONDS_PER_DAY {
        return "Daily usage".into();
    }
    if seconds.is_multiple_of(SECONDS_PER_DAY) {
        return format!("{}-day usage", seconds / SECONDS_PER_DAY);
    }
    if seconds.is_multiple_of(SECONDS_PER_HOUR) {
        return format!("{}-hour usage", seconds / SECONDS_PER_HOUR);
    }
    format!("{seconds}-second usage")
}

fn usage_limit(window: CodexUsageWindow) -> Option<UsageLimit> {
    Some(UsageLimit {
        label: usage_label(window.limit_window_seconds?),
        percentage: usage_percentage(window.used_percent?),
        reset_at: window
            .reset_at
            .and_then(|seconds| seconds.checked_mul(MILLIS_PER_SECOND)),
        detail: None,
    })
}

impl From<CodexUsage> for ProviderUsage {
    fn from(usage: CodexUsage) -> Self {
        let limits = [
            usage.rate_limit.primary_window,
            usage.rate_limit.secondary_window,
        ]
        .into_iter()
        .flatten()
        .filter_map(usage_limit)
        .collect();
        Self {
            plan: usage.plan_type,
            limits,
            by_model_today: vec![],
        }
    }
}

fn parse_usage(response: &str) -> Result<ProviderUsage, AgentError> {
    let usage: ProviderUsage = serde_json::from_str::<CodexUsage>(response)?.into();
    if usage.plan.is_none() && usage.limits.is_empty() {
        return Err(AgentError::Config {
            message: EMPTY_USAGE_ERROR.into(),
        });
    }
    Ok(usage)
}

fn resolve_openai_base_url() -> Option<String> {
    let config = maki_config::providers::ProvidersConfig::load();
    maki_config::providers::configured_base_url("openai", config.get("openai"))
}

// Codex models and GPT-6 drop `minimal` and never take an explicit "none", so
// they get their own dialects; the plain plan models keep both.
fn plan_dialect(model_id: &str) -> &'static EffortDialect<'static> {
    if !model_id.contains("-codex") {
        return if model_id.starts_with("gpt-6-") {
            &dialect::GPT_6
        } else if model_id.starts_with("gpt-5.6-") {
            &dialect::GPT_5_6
        } else {
            &dialect::CODING_PLAN
        };
    }
    if model_id.starts_with("gpt-5.1-codex") && !model_id.starts_with("gpt-5.1-codex-max") {
        &dialect::CODEX_5_1
    } else {
        &dialect::CODEX
    }
}

impl Provider for OpenAi {
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
            let mut buf = String::new();
            let system = super::super::with_prefix(&self.system_prefix, system, &mut buf);

            if is_codex_model(&model.id) {
                let mut body = super::responses::build_body(model, messages, system, tools);
                super::responses::apply_responses_reasoning(
                    &mut body,
                    opts.thinking,
                    model,
                    plan_dialect(&model.id),
                );
                let stream_timeout = self.compat.stream_timeout();
                return self
                    .with_oauth_retry(|| async {
                        let codex_auth = self.codex_auth()?;
                        super::responses::do_stream(
                            self.compat.client(),
                            model,
                            &body,
                            event_tx,
                            &codex_auth,
                            stream_timeout,
                        )
                        .await
                    })
                    .await;
            }

            let mut body = self.compat.build_body(model, messages, system, tools);
            opts.thinking
                .apply_reasoning_effort(&mut body, &dialect::STANDARD, model);
            self.with_oauth_retry(|| async {
                let auth = self.current_auth();
                self.compat
                    .do_stream(model, &[], &body, event_tx, &auth)
                    .await
            })
            .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async {
            if self.is_oauth() {
                let models = super::models()
                    .iter()
                    .flat_map(|e| e.prefixes.iter())
                    .filter(|id| is_codex_model(id))
                    .map(|&s| crate::model::ModelInfo::id_only(s.to_string()))
                    .collect();
                return Ok(models);
            }
            self.with_oauth_retry(|| async {
                let auth = self.current_auth();
                self.compat.do_list_models(&auth).await
            })
            .await
        })
    }

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async {
            if !self.is_oauth() {
                return Ok(None);
            }
            self.with_oauth_retry(|| async {
                let auth = self.codex_auth()?;
                let response = self.compat.get_text(&auth, USAGE_URL).await?;
                Ok(Some(parse_usage(&response)?))
            })
            .await
        })
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            if self.is_oauth() {
                self.refresh_oauth().await
            } else {
                Ok(())
            }
        })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            let Some(storage) = self.storage.clone() else {
                return Ok(());
            };
            let resolved = smol::unblock(move || auth::resolve(&storage)).await?;
            *self.auth.lock().unwrap() = resolved;
            debug!("reloaded OpenAI auth from storage");
            Ok(())
        })
    }

    fn adjust_model(&self, model: &mut Model) {
        if self.is_oauth()
            && let Some(context_window) = coding_plan_context_window(&model.id)
        {
            model.context_window = model.context_window.min(context_window);
        }
    }
}

#[cfg(test)]
mod tests {
    use maki_storage::sessions::Effort;
    use serde_json::json;
    use test_case::test_case;

    use super::super::responses;
    use super::*;
    use crate::ThinkingConfig;

    #[test_case("gpt-5.6-luna")]
    #[test_case("gpt-5.6-terra")]
    #[test_case("gpt-5.6-sol")]
    fn gpt_5_6_models_use_coding_plan(model_id: &str) {
        assert!(is_codex_model(model_id));
    }

    #[test_case("gpt-6-astra", Some(272_000))]
    #[test_case("gpt-5.6-luna", Some(372_000))]
    #[test_case("gpt-5.6-terra", Some(372_000))]
    #[test_case("gpt-5.6-sol", Some(372_000))]
    #[test_case("gpt-5.5", Some(272_000))]
    #[test_case("gpt-5.3-codex", Some(272_000))]
    #[test_case("gpt-5.7-codex", Some(272_000) ; "unlisted codex model still routes")]
    #[test_case("gpt-5.6-terra-preview", None ; "non-codex near-match is rejected")]
    #[test_case("gpt-5.4-nano", None)]
    fn coding_plan_context_window_resolves_plan_models(model_id: &str, expected: Option<u32>) {
        assert_eq!(coding_plan_context_window(model_id), expected);
    }

    #[test_case(ThinkingConfig::Adaptive, "gpt-5.3-codex", "medium" ; "adaptive")]
    #[test_case(ThinkingConfig::Effort(Effort::Minimal), "gpt-5.3-codex", "low" ; "minimal_snaps_to_low_on_codex")]
    #[test_case(ThinkingConfig::Effort(Effort::Low), "gpt-5.3-codex", "low" ; "low")]
    #[test_case(ThinkingConfig::Effort(Effort::Medium), "gpt-5.3-codex", "medium" ; "medium")]
    #[test_case(ThinkingConfig::Effort(Effort::High), "gpt-5.3-codex", "high" ; "high")]
    #[test_case(ThinkingConfig::Effort(Effort::XHigh), "gpt-5.3-codex", "xhigh" ; "xhigh")]
    #[test_case(ThinkingConfig::Effort(Effort::Max), "gpt-5.3-codex", "xhigh" ; "max_snaps_to_xhigh_on_codex")]
    #[test_case(ThinkingConfig::Effort(Effort::XHigh), "gpt-5.1-codex", "high" ; "xhigh_snaps_to_high_on_5_1")]
    #[test_case(ThinkingConfig::Effort(Effort::XHigh), "gpt-5.1-codex-max", "xhigh" ; "xhigh_passes_through_on_5_1_max")]
    #[test_case(ThinkingConfig::Effort(Effort::Minimal), "gpt-5.5", "minimal" ; "minimal_passes_through_on_5_5")]
    #[test_case(ThinkingConfig::Off, "gpt-5.5", "none" ; "off_is_explicit_on_5_5")]
    #[test_case(ThinkingConfig::Effort(Effort::Max), "gpt-5.5", "xhigh" ; "max_snaps_to_xhigh_on_5_5")]
    #[test_case(ThinkingConfig::Effort(Effort::Minimal), "gpt-5.6-sol", "minimal" ; "minimal_passes_through_on_5_6_sol")]
    #[test_case(ThinkingConfig::Off, "gpt-5.6-sol", "none" ; "off_is_explicit_on_5_6_sol")]
    #[test_case(ThinkingConfig::Effort(Effort::Max), "gpt-5.6-sol", "max" ; "max_passes_through_on_5_6_sol")]
    #[test_case(ThinkingConfig::Effort(Effort::Max), "gpt-5.6-terra", "max" ; "max_passes_through_on_5_6_terra")]
    #[test_case(ThinkingConfig::Effort(Effort::Max), "gpt-5.6-luna", "max" ; "max_passes_through_on_5_6_luna")]
    #[test_case(ThinkingConfig::Effort(Effort::Max), "gpt-6-astra", "max" ; "max_passes_through_on_6_astra")]
    #[test_case(ThinkingConfig::Effort(Effort::Minimal), "gpt-6-astra", "low" ; "minimal_snaps_to_low_on_6_astra")]
    #[test_case(ThinkingConfig::Adaptive, "gpt-6-astra", "medium" ; "adaptive_on_6_astra")]
    fn responses_reasoning_uses_responses_effort_object(
        thinking: ThinkingConfig,
        model_id: &str,
        expected: &str,
    ) {
        let model = Model::from_spec(&format!("openai/{model_id}")).unwrap();
        let mut body = json!({});
        responses::apply_responses_reasoning(&mut body, thinking, &model, plan_dialect(&model.id));
        assert_eq!(body["reasoning"]["effort"], expected);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn plan_models_have_a_reviewed_dialect() {
        const EXPECTED: &[(&str, &EffortDialect)] = &[
            ("gpt-6-astra", &dialect::GPT_6),
            ("gpt-5.6-luna", &dialect::GPT_5_6),
            ("gpt-5.6-terra", &dialect::GPT_5_6),
            ("gpt-5.6-sol", &dialect::GPT_5_6),
            ("gpt-5.5", &dialect::CODING_PLAN),
            ("gpt-5.4", &dialect::CODING_PLAN),
            ("gpt-5.4-mini", &dialect::CODING_PLAN),
            ("gpt-5.2", &dialect::CODING_PLAN),
        ];
        const UNREVIEWED: &str = "new PLAN_MODELS entry needs an effort dialect decision";

        for model_id in PLAN_MODELS {
            let (_, expected) = EXPECTED
                .iter()
                .find(|(id, _)| id == model_id)
                .unwrap_or_else(|| panic!("{UNREVIEWED}: {model_id}"));
            assert_eq!(plan_dialect(model_id), *expected, "{model_id}");
        }
    }

    #[test_case("gpt-5.3-codex")]
    #[test_case("gpt-6-astra")]
    fn responses_reasoning_omits_effort_when_disabled(model_id: &str) {
        let model = Model::from_spec(&format!("openai/{model_id}")).unwrap();
        let mut body = json!({});
        responses::apply_responses_reasoning(
            &mut body,
            ThinkingConfig::Off,
            &model,
            plan_dialect(&model.id),
        );
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn codex_usage_parses_quota_windows() {
        const RESPONSE: &str = r#"{
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 12.6,
                    "limit_window_seconds": 18000,
                    "reset_at": 1760000000
                },
                "secondary_window": {
                    "used_percent": 120,
                    "limit_window_seconds": 604800,
                    "reset_at": 1760100000
                }
            }
        }"#;
        let usage = parse_usage(RESPONSE).unwrap();
        assert_eq!(usage.plan.as_deref(), Some("pro"));
        assert_eq!(
            usage.limits,
            vec![
                UsageLimit {
                    label: "5-hour usage".into(),
                    percentage: Some(13),
                    reset_at: Some(1_760_000_000_000),
                    detail: None,
                },
                UsageLimit {
                    label: "Weekly usage".into(),
                    percentage: Some(100),
                    reset_at: Some(1_760_100_000_000),
                    detail: None,
                },
            ]
        );
    }

    #[test]
    fn codex_usage_skips_incomplete_windows() {
        const RESPONSE: &str = r#"{
            "rate_limit": {
                "primary_window": {"used_percent": 10},
                "secondary_window": {"used_percent": -2, "limit_window_seconds": 86400}
            }
        }"#;
        let usage = parse_usage(RESPONSE).unwrap();
        assert_eq!(
            usage.limits,
            vec![UsageLimit {
                label: "Daily usage".into(),
                percentage: Some(0),
                reset_at: None,
                detail: None,
            }]
        );
    }

    #[test_case("{}")]
    #[test_case(r#"{"rate_limit": {}}"#)]
    fn codex_usage_rejects_empty_responses(response: &str) {
        assert_eq!(
            parse_usage(response).unwrap_err().to_string(),
            EMPTY_USAGE_ERROR
        );
    }
}
