use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use maki_storage::StateDir;
use maki_storage::auth::load_provider_credentials;
use maki_storage::id::SessionRef;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::model::{Model, ModelFamily, ModelInfo};
use crate::provider::{BoxFuture, Provider};
use crate::providers::catalog::{
    CatalogMeta, CatalogTransport, EndpointType, FreeTier, ProviderQuirks, UsageEndpoint,
    sends_if_available, shared_catalog,
};
use crate::spec::{
    AuthDoc, Build, CatalogDoc, GENERIC_DISCOVERY_NOTE, GeneratedDocs, NO_CURATED_MODELS, Native,
    ProviderSpec,
};
use crate::{
    AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse,
    ThinkingConfig, UsageLimit,
};

use super::codec::SessionCarrier;
use super::{ResolvedAuth, Timeouts, with_prefix};

/// Zen lives under the bare `opencode` models.dev entry; Go has its own.
pub(crate) const ZEN_SLUG: &str = "opencode";
pub(crate) const GO_SLUG: &str = "opencode-go";
pub(crate) const SLUGS: &[&str] = &[ZEN_SLUG, GO_SLUG];

const ZEN_DISPLAY_NAME: &str = "Opencode Zen";
const GO_DISPLAY_NAME: &str = "Opencode Go";
const ENV_VAR: &str = "OPENCODE_API_KEY";
const ZEN_API_URL: &str = "https://opencode.ai/zen/v1";
const GO_API_URL: &str = "https://opencode.ai/zen/go/v1";
const ZEN_FEATURES: &str = "Dynamically discovered models via [models.dev](https://models.dev/) + all the models provided by Opencode Zen API";
const GO_FEATURES: &str = "Dynamically discovered models via [models.dev](https://models.dev/) + all the models provided by Opencode Go API";
const GO_DISCOVERY_NOTE: &str = "No hardcoded model catalog. Use any model ID supported by this provider. An API key is required.";
const CATALOG_UNAVAILABLE: &str = "the models.dev catalog is unavailable";

const FREE_MODELS_NOTE: &str = r#"By default Maki hides free models from the Opencode catalog. To list free models (they use a public fallback, no API key needed), add this to `~/.config/maki/providers.toml`:

```toml
[opencode]
enable_free_models = true
```

The default is `false`."#;

/// Neither Opencode slug is a `maki auth login` target, so neither carries a
/// `login` row.
pub(crate) const ZEN_SPEC: ProviderSpec = ProviderSpec {
    slug: ZEN_SLUG,
    display_name: ZEN_DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: true,
    supports_deferred_tools: false,
    sends_thinking,
    thinking_fields_fallback: |_| None,
    accepts_arbitrary_models: true,
    catalog_is_authoritative: false,
    fallback_max_output: Some(128_000),
    fallback_context_window: 256_000,
    models_toml: NO_CURATED_MODELS,
    pricing_schedule: None,
    build: Build::Native(Native {
        new: create,
        with_auth: create_with_auth,
    }),
    aperture: None,
    login: None,
    docs: GeneratedDocs {
        api_urls: &[ZEN_API_URL],
        features: Some(ZEN_FEATURES),
        auth: AuthDoc::EnvVar,
        catalog: CatalogDoc::Discovered(GENERIC_DISCOVERY_NOTE),
        trailing_notes: &[FREE_MODELS_NOTE],
    },
};

pub(crate) const GO_SPEC: ProviderSpec = ProviderSpec {
    slug: GO_SLUG,
    display_name: GO_DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: false,
    supports_deferred_tools: false,
    sends_thinking: |_, choice| sends_if_available(GO_SLUG, choice),
    thinking_fields_fallback: |_| None,
    accepts_arbitrary_models: true,
    catalog_is_authoritative: false,
    fallback_max_output: Some(64_000),
    fallback_context_window: 128_000,
    models_toml: NO_CURATED_MODELS,
    pricing_schedule: None,
    build: Build::Catalog,
    aperture: None,
    login: None,
    docs: GeneratedDocs {
        api_urls: &[GO_API_URL],
        features: Some(GO_FEATURES),
        auth: AuthDoc::EnvVar,
        catalog: CatalogDoc::Discovered(GO_DISCOVERY_NOTE),
        trailing_notes: &[],
    },
};

/// Zen ids name the sub-provider that serves them, as
/// [`Opencode::stream_message`] reads them.
fn sends_thinking(model: &Model, choice: ThinkingConfig) -> ThinkingConfig {
    let sub_provider = model
        .id
        .split_once('/')
        .map_or(ZEN_SLUG, |(sub_provider, _)| sub_provider);
    sends_if_available(sub_provider, choice)
}

fn create(timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    Ok(Box::new(Opencode::new(timeouts)))
}

fn create_with_auth(
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
) -> Box<dyn Provider> {
    Box::new(Opencode::with_auth(auth, timeouts).with_system_prefix(system_prefix))
}

/// OpenCode asked clients to send one stable ID per conversation and warned
/// that requests without it may start failing:
/// https://github.com/tontinton/maki/issues/935
const SESSION_HEADERS: &[Cow<'static, str>] = &[Cow::Borrowed("x-opencode-session")];
const PUBLIC_KEY: &str = "public";

const FREE_TIER: Option<FreeTier> = Some(FreeTier {
    public_key: PUBLIC_KEY,
    config_slug: ZEN_SLUG,
});

/// Headers only: the OpenCode wires are OpenAI-compatible and carry no body
/// field for a session.
const SESSION_CARRIER: SessionCarrier = SessionCarrier {
    headers: Cow::Borrowed(SESSION_HEADERS),
    body_field: None,
};

/// Only Go has a quota endpoint; OpenCode themselves expose subscription
/// usage on `/zen/go/v1/usage` and nothing on the Zen base.
pub(crate) const ZEN_QUIRKS: ProviderQuirks = ProviderQuirks {
    free_tier: FREE_TIER,
    session_id: SESSION_CARRIER,
    usage: None,
};

pub(crate) const GO_QUIRKS: ProviderQuirks = ProviderQuirks {
    free_tier: FREE_TIER,
    session_id: SESSION_CARRIER,
    usage: Some(GO_USAGE),
};

const GO_USAGE: UsageEndpoint = UsageEndpoint {
    url: "https://opencode.ai/zen/go/v1/usage",
    parse: parse_usage,
};

/// Plan quotas, served on 401 as well as 429. They clear on a weekly or monthly
/// boundary, so retrying or rotating keys only burns time. OpenCode's genuinely
/// short-lived per-minute cap is `RateLimitError` and is deliberately absent.
const QUOTA_ERROR_TYPES: [&str; 6] = [
    "CreditsError",
    "MonthlyLimitError",
    "UserLimitError",
    "FreeUsageLimitError",
    "GoUsageLimitError",
    "BlackUsageLimitError",
];

/// Types that arrive on 401 without the token being stale: `ModelError` covers
/// "trial ended", "model disabled" and "no provider available", none of which a
/// fresh login fixes and none of which is a usage cap.
const NON_LOGIN_ERROR_TYPES: [&str; 1] = ["ModelError"];

pub(crate) const QUOTA_FALLBACK_MESSAGE: &str = "provider usage limit reached";
pub(crate) const NON_LOGIN_FALLBACK_MESSAGE: &str = "model unavailable, check your OpenCode plan";

pub struct Opencode {
    transport: CatalogTransport,
    auth: Option<Arc<Mutex<ResolvedAuth>>>,
    system_prefix: Option<String>,
}

impl Opencode {
    pub fn new(timeouts: Timeouts) -> Self {
        Self {
            transport: CatalogTransport::new(timeouts),
            auth: None,
            system_prefix: None,
        }
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: Timeouts) -> Self {
        Self {
            auth: Some(auth),
            ..Self::new(timeouts)
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    async fn do_list_models(&self) -> Result<Vec<ModelInfo>, AgentError> {
        let guard = shared_catalog().await.lock().unwrap();
        let Some(data) = guard.provider(ZEN_SLUG) else {
            return if zen_has_key(&guard.state_dir) {
                Err(AgentError::Config {
                    message: CATALOG_UNAVAILABLE.into(),
                })
            } else {
                Ok(Vec::new())
            };
        };
        Ok(data.available_models(&guard.state_dir, data.free_models_enabled()))
    }

    async fn lookup(
        &self,
        sub_provider: &str,
        actual_id: &str,
        session_id: Option<&SessionRef>,
    ) -> Result<(CatalogMeta, EndpointType, ResolvedAuth), AgentError> {
        let guard = shared_catalog().await.lock().unwrap();
        let (meta, provider_data) = guard.lookup(sub_provider, actual_id)?;
        let override_auth = self
            .auth
            .clone()
            .filter(|_| SLUGS.contains(&provider_data.slug.as_str()))
            .map(|auth| {
                let mut auth = auth.lock().unwrap().clone();
                if auth.base_url.is_none() {
                    auth.base_url = provider_data.base_url.clone();
                }
                auth
            });
        let auth = match override_auth {
            Some(auth) => auth,
            None => provider_data
                .catalog_auth(&guard.state_dir, provider_data.free_models_enabled())?
                .unlocked(sub_provider)?
                .clone(),
        };
        let auth = provider_data.request_auth(auth, session_id)?;
        Ok((meta.clone(), provider_data.api_format, auth))
    }
}

/// Zen is listed for everyone, so a missing catalog only deserves a warning
/// for someone who configured a key. Without one there is nothing to list and
/// nothing to fix.
fn zen_has_key(state_dir: &StateDir) -> bool {
    std::env::var(ENV_VAR).is_ok() || load_provider_credentials(state_dir, ZEN_SLUG).is_some()
}

impl Provider for Opencode {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a UnboundedSender<ProviderEvent>,
        opts: RequestOptions,
        session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let (sub_provider, actual_id) =
                model.id.split_once('/').unwrap_or((ZEN_SLUG, &model.id));

            let (meta, api_format, auth) = self.lookup(sub_provider, actual_id, session_id).await?;

            let mut buf = String::new();
            let system = with_prefix(&self.system_prefix, system, &mut buf);

            let stream_model = Model {
                id: actual_id.to_string(),
                // `..model` carries the agent's turn budget, which
                // [`Model::output_tokens`] clamps to this cap on read.
                max_output_tokens: Some(meta.max_output()),
                context_window: meta.context_window(),
                ..model.clone()
            };

            self.transport
                .stream(
                    api_format,
                    &stream_model,
                    messages,
                    system,
                    tools,
                    event_tx,
                    &auth,
                    &opts,
                )
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(self.do_list_models())
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async { Ok(()) })
    }
}

/// An OpenCode failure that re-authenticating cannot clear.
pub(crate) struct NonLoginError {
    pub(crate) message: String,
    /// Plan quota, so also not worth a retry or a key rotation.
    pub(crate) is_quota: bool,
}

/// OpenCode wraps billing and plan failures in `{"error":{"type","message"}}`
/// and serves them on the same statuses as a genuinely expired token, so the
/// shared error type has to ask us before it tells the user to log in again.
pub(crate) fn non_login_error(status: u16, body: &str) -> Option<NonLoginError> {
    if !matches!(status, 401 | 429) {
        return None;
    }
    let body: Value = serde_json::from_str(body).ok()?;
    let error_type = body.pointer("/error/type")?.as_str()?;
    let is_quota = QUOTA_ERROR_TYPES.contains(&error_type);
    if !is_quota && !NON_LOGIN_ERROR_TYPES.contains(&error_type) {
        return None;
    }
    let fallback = if is_quota {
        QUOTA_FALLBACK_MESSAGE
    } else {
        NON_LOGIN_FALLBACK_MESSAGE
    };
    Some(NonLoginError {
        message: body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .filter(|message| !message.trim().is_empty())
            .unwrap_or(fallback)
            .to_owned(),
        is_quota,
    })
}

const ROLLING_LABEL: &str = "5-hour";
const WEEKLY_LABEL: &str = "Weekly";
const MONTHLY_LABEL: &str = "Monthly";
const RATE_LIMITED_STATUS: &str = "rate-limited";
const RATE_LIMITED_DETAIL: &str = "rate limited";

#[derive(Default, Deserialize)]
struct GoUsageResponse {
    usage: GoUsage,
}

#[derive(Default, Deserialize)]
struct GoUsage {
    #[serde(default)]
    rolling: Option<GoWindow>,
    #[serde(default)]
    weekly: Option<GoWindow>,
    #[serde(default)]
    monthly: Option<GoWindow>,
}

#[derive(Deserialize)]
struct GoWindow {
    percent: Option<f64>,
    #[serde(
        default,
        rename = "resetsAt",
        deserialize_with = "crate::types::deserialize_reset_at"
    )]
    resets_at: Option<u64>,
    status: Option<String>,
}

impl GoUsage {
    fn limits(self) -> Vec<UsageLimit> {
        [
            (ROLLING_LABEL, self.rolling),
            (WEEKLY_LABEL, self.weekly),
            (MONTHLY_LABEL, self.monthly),
        ]
        .into_iter()
        .filter_map(|(label, window)| window.map(|window| (label, window)))
        .map(|(label, window)| UsageLimit {
            label: label.to_owned(),
            percentage: window
                .percent
                .map(|percent| percent.round().clamp(0.0, 100.0) as u32),
            reset_at: window.resets_at,
            detail: (window.status.as_deref() == Some(RATE_LIMITED_STATUS))
                .then(|| RATE_LIMITED_DETAIL.to_owned()),
        })
        .collect()
    }
}

/// The shape `UsageEndpoint::url` answers, see the wire sample in the tests.
fn parse_usage(body: &str) -> Result<ProviderUsage, AgentError> {
    let response: GoUsageResponse = serde_json::from_str(body)?;
    Ok(ProviderUsage {
        plan: None,
        limits: response.usage.limits(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_BODY: &str = r#"{"usage":{
        "rolling": {"status":"active","percent":62,"resetsAt":"2026-11-03T19:30:00Z"},
        "weekly": {"status":"rate-limited","percent":31.4,"resetsAt":"2026-11-08T00:00:00Z"},
        "monthly": {"status":"active","percent":0,"resetsAt":"2026-12-01T00:00:00Z"}
    }}"#;

    fn usage(body: &str) -> Vec<UsageLimit> {
        parse_usage(body).unwrap().limits
    }

    #[test]
    fn parse_usage_windows_keep_order_percent_and_reset() {
        let limits = usage(SAMPLE_BODY);
        assert_eq!(limits.len(), 3);
        assert_eq!(limits[0].label, ROLLING_LABEL);
        assert_eq!(limits[0].percentage, Some(62));
        assert_eq!(limits[0].detail, None);
        assert_eq!(limits[1].percentage, Some(31));
        assert_eq!(limits[1].detail.as_deref(), Some(RATE_LIMITED_DETAIL));
        assert_eq!(limits[2].percentage, Some(0));
        assert!(limits.iter().all(|limit| limit.reset_at.is_some()));
    }

    #[test]
    fn parse_usage_missing_windows_are_skipped() {
        let limited: GoUsageResponse =
            serde_json::from_str(r#"{"usage":{"weekly":{"percent":5}}}"#).unwrap();
        assert_eq!(limited.usage.limits()[0].label, WEEKLY_LABEL);
    }

    #[test]
    fn parse_usage_unknown_status_is_dropped() {
        let limits = usage(r#"{"usage":{"rolling":{"status":"what-now","percent":1}}}"#);
        assert_eq!(limits[0].detail, None);
    }
}
