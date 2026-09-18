//! Anthropic allows 4 cache breakpoints per request. We place them on: the last tool
//! definition, the system prompt, and the last block of the 2 most recent messages.

pub(crate) mod bedrock;
pub(crate) mod shared;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flume::Sender;
use futures_lite::io::{AsyncBufReadExt, BufReader};
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use maki_storage::id::SessionRef;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use maki_config::providers::Protocol;

use crate::model::{Model, ModelFamily};
use crate::provider::{BoxFuture, Provider};
use crate::providers::aperture::NO_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, CatalogDoc, GeneratedDocs, LoginConfig, Native, ProviderSpec,
};
use crate::{
    AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse, UsageLimit,
};

use super::{KeyHeader, KeyPool, KeyRotation, ResolvedAuth, Timeouts};

const API_VERSION: &str = "2023-06-01";
const API_ORIGIN: &str = "https://api.anthropic.com";
const MESSAGES_PATH: &str = "/v1/messages";
const MODELS_PATH: &str = "/v1/models?limit=1000";
const USAGE_PATH: &str = "/api/oauth/usage";
const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const MONEY_EXPONENT: u32 = 2;
const LABEL_SESSION: &str = "Current session";
const LABEL_WEEK_ALL: &str = "Current week (all models)";

pub(crate) const SLUG: &str = "anthropic";
pub(crate) const DISPLAY_NAME: &str = "Anthropic";
const ENV_VAR: &str = "ANTHROPIC_API_KEY";
const API_KEY_HEADER: &str = "x-api-key";
const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-4-6";
const LOGIN_URL: &str = "https://console.anthropic.com/settings/keys";
/// The messages endpoint, which is what the docs quote; the provider itself
/// builds it from [`API_ORIGIN`] plus [`MESSAGES_PATH`].
const DOC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const FEATURES: &str = "Prompt caching, thinking mode (adaptive/budgeted), advanced tool use";

const LONG_CONTEXT_NOTE: &str = r#"Add `-1m` to any Claude model, like `claude-sonnet-4-6-1m`, to use the 1M token context window."#;

const BEDROCK_NOTE: &str = r#"#### Amazon Bedrock

If you already use Claude through AWS Bedrock, you can point Maki at it instead of the direct Anthropic API. Set `CLAUDE_CODE_USE_BEDROCK=1` and Maki will route all Anthropic requests through Bedrock. The same models, the same features, just a different door.

You will need `AWS_REGION` and one of the following for auth:

| Method | Env vars |
|--------|----------|
| IAM credentials | `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (and optionally `AWS_SESSION_TOKEN`) |
| Credentials file | `AWS_PROFILE` (defaults to `default`), reads `~/.aws/credentials` |
| Bearer token | `AWS_BEARER_TOKEN_BEDROCK` |
| Gateway proxy | `CLAUDE_CODE_SKIP_BEDROCK_AUTH=1` + `ANTHROPIC_BEDROCK_BASE_URL` (skips signing, useful behind a proxy that handles auth) |

You can override the model with `ANTHROPIC_MODEL` and the endpoint with `ANTHROPIC_BEDROCK_BASE_URL`. These env var names match Claude Code, so if you were already using Bedrock there, the same setup works here."#;

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Claude,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(128_000),
    fallback_context_window: 200_000,
    models: models(),
    pricing_schedule: None,
    native: Some(Native {
        new: create,
        with_auth: create_with_auth,
        aperture: Some(ApertureRoute {
            path_prefix: NO_PATH_PREFIX,
        }),
    }),
    login: Some(LoginConfig {
        protocol: Protocol::Anthropic,
        default_base_url: API_ORIGIN,
        default_model: DEFAULT_MODEL,
        plans: None,
        login_url: Some(LOGIN_URL),
        needs_url: false,
    }),
    docs: GeneratedDocs {
        api_urls: &[DOC_API_URL],
        features: Some(FEATURES),
        auth: AuthDoc::EnvVar,
        catalog: CatalogDoc::Table,
        trailing_notes: &[LONG_CONTEXT_NOTE, BEDROCK_NOTE],
    },
};

inventory::submit!(SPEC.config_row());

fn create(timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    if bedrock::is_enabled() {
        Ok(Box::new(bedrock::Bedrock::new(timeouts)?))
    } else {
        Ok(Box::new(Anthropic::new(timeouts)?))
    }
}

fn create_with_auth(
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
) -> Box<dyn Provider> {
    Box::new(Anthropic::with_auth(auth, timeouts).with_system_prefix(system_prefix))
}

pub(crate) use shared::models;

/// Returns whether the fast-mode beta header must be attached. We re-check
/// `supports_fast()` here rather than trusting `opts.fast` alone, so a stale UI
/// flag can never bill an ineligible model at the premium fast-mode rate.
fn apply_fast_mode(body: &mut Value, model: &Model, opts: RequestOptions) -> bool {
    let on = opts.fast && model.supports_fast();
    if on {
        body["speed"] = json!("fast");
    }
    on
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OauthUsage {
    limits: Vec<ApiLimit>,
    spend: Option<Spend>,
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
    seven_day_sonnet: Option<UsageWindow>,
    seven_day_opus: Option<UsageWindow>,
    extra_usage: Option<ExtraUsage>,
}

#[derive(Deserialize)]
struct ApiLimit {
    kind: String,
    #[serde(default)]
    percent: Option<f64>,
    #[serde(default)]
    resets_at: Option<String>,
    #[serde(default)]
    scope: Value,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Spend {
    enabled: bool,
    percent: Option<f64>,
    used: Option<Money>,
}

#[derive(Deserialize)]
struct Money {
    amount_minor: i64,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    exponent: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct UsageWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ExtraUsage {
    is_enabled: bool,
    utilization: Option<f64>,
    used_credits: Option<f64>,
    currency: Option<String>,
    decimal_places: Option<u32>,
}

fn parse_reset(rfc3339: &str) -> Option<u64> {
    let ts: jiff::Timestamp = rfc3339.parse().ok()?;
    u64::try_from(ts.as_millisecond()).ok()
}

fn spent(minor_units: f64, exponent: Option<u32>, currency: Option<&str>) -> String {
    let amount = minor_units / 10f64.powi(exponent.unwrap_or(MONEY_EXPONENT) as i32);
    match currency {
        None | Some("USD") => format!("${amount:.2} spent"),
        Some(c) => format!("{amount:.2} {c} spent"),
    }
}

fn limit_label(l: &ApiLimit) -> String {
    match l.kind.as_str() {
        "session" => LABEL_SESSION.into(),
        "weekly_all" => LABEL_WEEK_ALL.into(),
        "weekly_scoped" => match l
            .scope
            .pointer("/model/display_name")
            .and_then(Value::as_str)
        {
            Some(name) => format!("Current week ({name})"),
            None => "Current week".into(),
        },
        other => other.into(),
    }
}

fn credits_limit(u: &OauthUsage) -> Option<UsageLimit> {
    let (percent, detail) = match (&u.spend, &u.extra_usage) {
        (Some(s), _) if s.enabled => (
            s.percent.unwrap_or_default(),
            s.used
                .as_ref()
                .map(|m| spent(m.amount_minor as f64, m.exponent, m.currency.as_deref())),
        ),
        (None, Some(e)) if e.is_enabled => (
            e.utilization?,
            e.used_credits
                .map(|c| spent(c, e.decimal_places, e.currency.as_deref())),
        ),
        _ => return None,
    };
    Some(UsageLimit {
        label: "Usage credits".into(),
        percentage: Some(percent.round() as u32),
        reset_at: None,
        detail,
    })
}

impl From<OauthUsage> for ProviderUsage {
    fn from(u: OauthUsage) -> Self {
        let mut limits: Vec<UsageLimit> = u
            .limits
            .iter()
            .filter_map(|l| {
                Some(UsageLimit {
                    label: limit_label(l),
                    percentage: Some(l.percent?.round() as u32),
                    reset_at: l.resets_at.as_deref().and_then(parse_reset),
                    detail: None,
                })
            })
            .collect();
        if limits.is_empty() {
            let windows = [
                (LABEL_SESSION, &u.five_hour),
                (LABEL_WEEK_ALL, &u.seven_day),
                ("Current week (Sonnet)", &u.seven_day_sonnet),
                ("Current week (Opus)", &u.seven_day_opus),
            ];
            limits.extend(windows.into_iter().filter_map(|(label, w)| {
                let w = w.as_ref()?;
                Some(UsageLimit {
                    label: label.into(),
                    percentage: Some(w.utilization?.round() as u32),
                    reset_at: w.resets_at.as_deref().and_then(parse_reset),
                    detail: None,
                })
            }));
        }
        limits.extend(credits_limit(&u));
        ProviderUsage {
            plan: None,
            limits,
            by_model_today: vec![],
        }
    }
}

/// Reduce a `base_url` to a bare origin, tolerating a trailing `/v1/messages`
/// (which Anthropic base URLs historically included) and any query string
/// (e.g. `?beta=true` from Claude Code style endpoints).
fn origin(base_url: &str) -> &str {
    let without_query = base_url.split(['?', '#']).next().unwrap_or(base_url);
    let trimmed = without_query.trim_end_matches('/');
    trimmed.strip_suffix(MESSAGES_PATH).unwrap_or(trimmed)
}

/// True when `base_url` targets the real Anthropic API, directly or via the
/// construction-time base-URL override (so quota stays visible behind a proxy).
fn first_party(base_url: &str, configured_override: Option<&str>) -> bool {
    let target = origin(base_url);
    target.contains("api.anthropic.com")
        || configured_override.is_some_and(|configured| origin(configured) == target)
}

/// Subscription quota only exists for OAuth tokens against the real Anthropic
/// API; API keys and anthropic-protocol third-party endpoints have none.
fn usage_eligible(auth: &super::ResolvedAuth, configured_override: Option<&str>) -> bool {
    auth.headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        && auth
            .base_url
            .as_deref()
            .is_none_or(|url| first_party(url, configured_override))
}

fn resolve_anthropic_base_url() -> Option<String> {
    let config = maki_config::providers::ProvidersConfig::load();
    maki_config::providers::resolve_base_url("anthropic", config.get("anthropic"))
}

fn resolve_auth_from_key(
    key: &str,
    base_url: Option<String>,
) -> Result<super::ResolvedAuth, AgentError> {
    Ok(
        super::ResolvedAuth::new("anthropic", vec![(API_KEY_HEADER.into(), key.to_string())])?
            .with_base_url(base_url),
    )
}

pub struct Anthropic {
    client: HttpClient,
    auth: Arc<Mutex<super::ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
    stream_timeout: Duration,
    /// Env / `providers.toml` / inventory default, resolved once at construction.
    /// Reused by key rotation / reload so they do not re-parse providers.toml.
    resolved_base_url: Option<String>,
}

impl Anthropic {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve("anthropic", ENV_VAR)?;
        let resolved_base_url = resolve_anthropic_base_url();
        let resolved = resolve_auth_from_key(pool.current(), resolved_base_url.clone())?;
        debug!(keys = pool.len(), "using API key authentication");
        Ok(Self {
            client: super::http_client(timeouts),
            auth: Arc::new(Mutex::new(resolved)),
            key_pool: Some(pool),
            system_prefix: None,
            stream_timeout: timeouts.stream,
            resolved_base_url,
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<super::ResolvedAuth>>,
        timeouts: super::Timeouts,
    ) -> Self {
        Self {
            client: super::http_client(timeouts),
            auth,
            key_pool: None,
            system_prefix: None,
            stream_timeout: timeouts.stream,
            // Custom / dynamic callers own their base URL; treating it as the
            // anthropic override would make every third-party endpoint look
            // first party and poll `/api/oauth/usage` against it.
            resolved_base_url: None,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    fn build_request(&self, method: &str, path: &str) -> isahc::http::request::Builder {
        let auth = self.auth.lock().unwrap();
        let base = auth.base_url.as_deref().unwrap_or(API_ORIGIN);
        let url = format!("{}{path}", origin(base));
        auth.configure_request(
            Request::builder()
                .method(method)
                .uri(url)
                .header("anthropic-version", API_VERSION)
                .header("user-agent", super::user_agent()),
        )
    }

    async fn do_stream_request(
        &self,
        body: &Value,
        event_tx: &Sender<ProviderEvent>,
        fast: bool,
        long_context: bool,
    ) -> Result<StreamResponse, AgentError> {
        let json_body = serde_json::to_vec(body)?;
        let mut builder = self
            .build_request("POST", MESSAGES_PATH)
            .header("content-type", "application/json");
        let mut betas = Vec::new();
        if fast {
            betas.push(FAST_MODE_BETA);
        }
        if long_context {
            betas.push(shared::LONG_CONTEXT_BETA);
        }
        if !betas.is_empty() {
            builder = builder.header("anthropic-beta", betas.join(","));
        }
        let request = builder.body(json_body)?;
        let response = self.client.send_async(request).await?;
        let status = response.status().as_u16();

        if status == 200 {
            parse_sse(response, event_tx, self.stream_timeout).await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }

    async fn do_list_models(&self) -> Result<Vec<crate::model::ModelInfo>, AgentError> {
        let mut models = Vec::new();
        let mut after_id: Option<String> = None;

        loop {
            let mut path = MODELS_PATH.to_string();
            if let Some(cursor) = &after_id {
                path.push_str(&format!("&after_id={cursor}"));
            }

            let request = self.build_request("GET", &path).body(())?;
            let mut response = self.client.send_async(request).await?;
            if response.status().as_u16() != 200 {
                return Err(AgentError::from_response(response).await);
            }

            let body_text = response.text().await?;
            let page: ModelsPage = serde_json::from_str(&body_text)?;
            for m in page.data {
                models.extend(discovered_model_infos(m));
            }

            if !page.has_more {
                break;
            }
            after_id = page.last_id;
        }

        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }
}

impl Provider for Anthropic {
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
            let system_blocks = if let Some(prefix) = &self.system_prefix {
                vec![
                    shared::SystemBlock {
                        r#type: "text",
                        text: prefix,
                        cache_control: None,
                    },
                    shared::SystemBlock {
                        r#type: "text",
                        text: system,
                        cache_control: Some(shared::EPHEMERAL),
                    },
                ]
            } else {
                vec![shared::SystemBlock {
                    r#type: "text",
                    text: system,
                    cache_control: Some(shared::EPHEMERAL),
                }]
            };

            let mut body = shared::build_request_body_with_system(
                model,
                messages,
                &system_blocks,
                tools,
                opts.thinking,
            );
            body["model"] = json!(shared::strip_long_context(&model.id));
            body["stream"] = json!(true);
            let fast = apply_fast_mode(&mut body, model, opts);
            let long_context = model.id.ends_with(shared::LONG_CONTEXT_SUFFIX);

            debug!(model = %model.id, num_messages = messages.len(), thinking = ?opts.thinking, fast, long_context, "sending API request");
            self.do_stream_request(&body, event_tx, fast, long_context)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(self.do_list_models())
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            let pool = KeyPool::resolve("anthropic", ENV_VAR)?;
            *self.auth.lock().unwrap() =
                resolve_auth_from_key(pool.current(), self.resolved_base_url.clone())?;
            debug!("reloaded Anthropic auth from env");
            Ok(())
        })
    }

    fn keys(&self) -> Option<KeyRotation<'_>> {
        Some(KeyRotation::new(
            self.key_pool.as_ref()?,
            &self.auth,
            KeyHeader::Raw(API_KEY_HEADER),
        ))
    }

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async move {
            if !usage_eligible(
                &self.auth.lock().unwrap(),
                self.resolved_base_url.as_deref(),
            ) {
                return Ok(None);
            }
            let request = self
                .build_request("GET", USAGE_PATH)
                .header("anthropic-beta", OAUTH_BETA)
                .body(())?;
            let mut response = self.client.send_async(request).await?;
            if response.status().as_u16() != 200 {
                return Err(AgentError::from_response(response).await);
            }
            let parsed: OauthUsage = serde_json::from_str(&response.text().await?)?;
            Ok(Some(parsed.into()))
        })
    }
}

#[derive(Deserialize)]
struct ApiModelInfo {
    id: String,
    #[serde(default)]
    max_input_tokens: u32,
    #[serde(default)]
    max_tokens: Option<u32>,
}

/// Discovery entries for one /v1/models item: a synthesised `-1m` variant
/// (when the reported input window reaches 1M) followed by the base model.
/// The variant pins `context_window` to [`shared::LONG_CONTEXT_WINDOW`] --
/// that is what the suffix asserts -- while the base keeps whatever
/// discovery reported; zero-valued fields are treated as unreported.
///
/// The base id deliberately drops a window at or past 1M. Reaching it needs
/// the [`shared::LONG_CONTEXT_BETA`] header, which is gated on the `-1m`
/// suffix, so the base id would gauge against a million tokens the request
/// never asks for: compaction would never fire and the API would reject the
/// turn at 200K. Unreported lets the curated entry answer instead.
fn discovered_model_infos(m: ApiModelInfo) -> Vec<crate::model::ModelInfo> {
    let mut models = Vec::new();
    let context_window = (m.max_input_tokens > 0
        && m.max_input_tokens < shared::LONG_CONTEXT_WINDOW)
        .then_some(m.max_input_tokens);
    let max_output_tokens = m.max_tokens.filter(|&v| v > 0);
    if m.max_input_tokens >= shared::LONG_CONTEXT_WINDOW {
        models.push(crate::model::ModelInfo {
            id: format!("{}{}", m.id, shared::LONG_CONTEXT_SUFFIX),
            context_window: Some(shared::LONG_CONTEXT_WINDOW),
            max_output_tokens,
            ..Default::default()
        });
    }
    models.push(crate::model::ModelInfo {
        id: m.id,
        context_window,
        max_output_tokens,
        ..Default::default()
    });
    models
}

#[derive(Deserialize)]
struct ModelsPage {
    data: Vec<ApiModelInfo>,
    has_more: bool,
    last_id: Option<String>,
}

pub(crate) async fn parse_sse(
    response: isahc::Response<isahc::AsyncBody>,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let reader = BufReader::new(response.into_body());
    let mut lines = reader.lines();
    let mut parser = shared::EventParser::new();
    let mut current_event = String::new();
    let mut deadline = Instant::now() + stream_timeout;

    while let Some(line) = super::next_sse_line(&mut lines, &mut deadline, stream_timeout).await? {
        if let Some(rest) = line.strip_prefix("event:") {
            current_event = rest.strip_prefix(' ').unwrap_or(rest).to_string();
            continue;
        }

        let data = match line.strip_prefix("data:") {
            Some(d) => d.strip_prefix(' ').unwrap_or(d),
            None => continue,
        };

        if parser
            .process(&current_event, data, event_tx)
            .await?
            .is_break()
        {
            break;
        }
    }

    Ok(parser.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentBlock, EMPTY_RESPONSE_MARKER, ProviderEvent, Role, StopReason, TokenUsage};
    use serde_json::{Value, json};
    use shared::build_wire_messages;
    use std::time::Duration;
    use test_case::test_case;

    const TEST_STREAM_TIMEOUT: Duration = Duration::from_secs(300);
    const THIRD_PARTY_BASE_URL: &str = "https://proxy.example.com/v1/messages";

    const USAGE_BODY: &str = r#"{
        "five_hour": {"utilization": 14.0, "resets_at": "2026-02-06T22:00:00+00:00"},
        "seven_day": {"utilization": 2.0,  "resets_at": "2026-02-09T00:00:00+00:00"},
        "limits": [
            {"kind": "session",       "group": "session", "percent": 14, "severity": "normal",
             "resets_at": "2026-02-06T22:00:00+00:00", "scope": null, "is_active": true},
            {"kind": "weekly_all",    "group": "weekly",  "percent": 2,  "severity": "normal",
             "resets_at": "2026-02-09T00:00:00+00:00", "scope": null, "is_active": false},
            {"kind": "weekly_scoped", "group": "weekly",  "percent": 3,  "severity": "normal",
             "resets_at": "2026-02-09T00:00:00+00:00",
             "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null},
             "is_active": false}
        ],
        "extra_usage": {"is_enabled": true, "monthly_limit": 15000, "used_credits": 233.0,
                        "utilization": 1.55, "currency": "USD", "decimal_places": 2},
        "spend": {
            "used":  {"amount_minor": 233,   "currency": "USD", "exponent": 2},
            "limit": {"amount_minor": 15000, "currency": "USD", "exponent": 2},
            "percent": 2, "severity": "normal", "enabled": true
        }
    }"#;

    #[test]
    fn parse_oauth_usage_response() {
        let parsed: OauthUsage = serde_json::from_str(USAGE_BODY).unwrap();
        let usage: ProviderUsage = parsed.into();
        assert!(usage.plan.is_none());
        assert_eq!(usage.limits.len(), 4);
        assert_eq!(usage.limits[0].label, "Current session");
        assert_eq!(usage.limits[0].percentage, Some(14));
        assert_eq!(usage.limits[0].reset_at, Some(1770415200000));
        assert_eq!(usage.limits[1].label, "Current week (all models)");
        assert_eq!(usage.limits[1].percentage, Some(2));
        assert_eq!(usage.limits[2].label, "Current week (Fable)");
        assert_eq!(usage.limits[2].percentage, Some(3));
        assert_eq!(usage.limits[3].label, "Usage credits");
        assert_eq!(usage.limits[3].percentage, Some(2));
        assert_eq!(usage.limits[3].reset_at, None);
        assert_eq!(usage.limits[3].detail.as_deref(), Some("$2.33 spent"));
    }

    #[test]
    fn parse_oauth_usage_windows_fallback() {
        let body = r#"{
            "five_hour":        {"utilization": 35.4, "resets_at": "2026-02-06T22:00:00+00:00"},
            "seven_day":        {"utilization": 14.0, "resets_at": "2026-02-09T00:00:00+00:00"},
            "seven_day_sonnet": {"utilization": 39.0, "resets_at": "2026-02-09T00:00:00+00:00"},
            "seven_day_opus":   {"utilization": 2.6,  "resets_at": "2026-02-09T00:00:00+00:00"},
            "extra_usage":      {"is_enabled": true, "used_credits": 233.0, "utilization": 2.0}
        }"#;
        let parsed: OauthUsage = serde_json::from_str(body).unwrap();
        let usage: ProviderUsage = parsed.into();
        assert_eq!(usage.limits.len(), 5);
        assert_eq!(usage.limits[0].label, "Current session");
        assert_eq!(usage.limits[0].percentage, Some(35));
        assert_eq!(usage.limits[0].reset_at, Some(1770415200000));
        assert_eq!(usage.limits[1].label, "Current week (all models)");
        assert_eq!(usage.limits[2].label, "Current week (Sonnet)");
        assert_eq!(usage.limits[3].label, "Current week (Opus)");
        assert_eq!(usage.limits[3].percentage, Some(3));
        assert_eq!(usage.limits[4].label, "Usage credits");
        assert_eq!(usage.limits[4].percentage, Some(2));
        assert_eq!(usage.limits[4].detail.as_deref(), Some("$2.33 spent"));
    }

    #[test]
    fn parse_oauth_usage_null_windows_skipped() {
        let body = r#"{
            "five_hour": {"utilization": 5.0, "resets_at": "not a timestamp"},
            "seven_day_opus": null,
            "extra_usage": {"is_enabled": true, "utilization": null}
        }"#;
        let parsed: OauthUsage = serde_json::from_str(body).unwrap();
        let usage: ProviderUsage = parsed.into();
        assert_eq!(usage.limits.len(), 1);
        assert_eq!(usage.limits[0].label, "Current session");
        assert_eq!(usage.limits[0].reset_at, None);
    }

    #[test_case("Authorization", None, true ; "bearer_default_url_eligible")]
    #[test_case("authorization", Some("https://api.anthropic.com/v1/messages"), true ; "bearer_anthropic_url_eligible")]
    #[test_case("authorization", Some("https://api.anthropic.com"), true ; "bearer_anthropic_origin_eligible")]
    #[test_case("x-api-key", None, false ; "api_key_not_eligible")]
    #[test_case("Authorization", Some("https://proxy.example.com/v1/messages"), false ; "foreign_base_url_not_eligible")]
    fn usage_eligibility(header: &str, base_url: Option<&str>, expected: bool) {
        let auth = crate::providers::ResolvedAuth::for_test(
            base_url.map(String::from),
            vec![(header.into(), "token".into())],
        );
        assert_eq!(usage_eligible(&auth, None), expected);
    }

    #[test]
    fn with_auth_keeps_third_party_endpoint_ineligible() {
        let auth = crate::providers::ResolvedAuth::for_test(
            Some(THIRD_PARTY_BASE_URL.into()),
            vec![("authorization".into(), "Bearer token".into())],
        );
        let provider = Anthropic::with_auth(
            Arc::new(Mutex::new(auth)),
            crate::providers::Timeouts::default(),
        );
        assert!(provider.resolved_base_url.is_none());
        assert!(!usage_eligible(
            &provider.auth.lock().unwrap(),
            provider.resolved_base_url.as_deref()
        ));
    }

    #[test]
    fn usage_eligible_when_url_matches_configured_override() {
        let auth = crate::providers::ResolvedAuth::for_test(
            Some(THIRD_PARTY_BASE_URL.into()),
            vec![("Authorization".into(), "token".into())],
        );
        assert!(usage_eligible(&auth, Some(THIRD_PARTY_BASE_URL)));
        assert!(!usage_eligible(
            &auth,
            Some("https://other-proxy.example.com")
        ));
    }

    #[test_case("https://api.anthropic.com/v1/messages", "https://api.anthropic.com" ; "strips_messages_path")]
    #[test_case("https://proxy.example.com/v1/messages/", "https://proxy.example.com" ; "strips_messages_path_with_trailing_slash")]
    #[test_case("https://api.anthropic.com", "https://api.anthropic.com" ; "origin_unchanged")]
    #[test_case("http://localhost:8080/", "http://localhost:8080" ; "trims_trailing_slash")]
    #[test_case("https://api.anthropic.com/v1/messages?beta=true", "https://api.anthropic.com" ; "strips_query_string")]
    fn origin_normalizes(input: &str, expected: &str) {
        assert_eq!(origin(input), expected);
    }

    fn mock_response(data: &'static [u8]) -> isahc::Response<isahc::AsyncBody> {
        let body = isahc::AsyncBody::from_bytes_static(data);
        isahc::Response::builder().status(200).body(body).unwrap()
    }

    #[test]
    fn parse_sse_text_and_usage() {
        smol::block_on(async {
            let sse_data = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"cache_creation_input_tokens\":5,\"cache_read_input_tokens\":8}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":10}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n";

            let (tx, rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert_eq!(
                resp.usage,
                TokenUsage {
                    input: 42,
                    output: 10,
                    cache_creation: 5,
                    cache_read: 8,
                    cost: None,
                }
            );
            assert!(
                matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "Hello world")
            );
            assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));

            let mut deltas = Vec::new();
            while let Ok(e) = rx.try_recv() {
                if let ProviderEvent::TextDelta { text: t } = e {
                    deltas.push(t);
                }
            }
            assert_eq!(deltas, vec!["Hello", " world"]);
        })
    }

    #[test]
    fn parse_sse_no_space_after_colon() {
        smol::block_on(async {
            let sse_data = b"\
event:message_start\n\
data:{\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\
\n\
event:content_block_start\n\
data:{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event:content_block_delta\n\
data:{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\
\n\
event:message_delta\n\
data:{\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\
\n\
event:message_stop\n\
data:{\"type\":\"message_stop\"}\n";

            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "OK")
            );
            assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
            assert_eq!(resp.usage.input, 7);
            assert_eq!(resp.usage.output, 1);
        })
    }

    #[test]
    fn parse_sse_tool_use() {
        smol::block_on(async {
            let sse_data = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"bash\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\" \\\"echo hi\\\"}\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n";

            let (tx, rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].0, "tu_1");
            assert_eq!(tools[0].1, "bash");

            let starts: Vec<_> = rx
                .drain()
                .filter_map(|e| match e {
                    ProviderEvent::ToolUseStart { id, name } => Some((id, name)),
                    _ => None,
                })
                .collect();
            assert_eq!(starts, vec![("tu_1".to_string(), "bash".to_string())]);
        })
    }

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text { text: text.into() }
    }

    fn thinking_block(thinking: &str) -> ContentBlock {
        ContentBlock::Thinking {
            thinking: thinking.into(),
            signature: None,
        }
    }

    fn message(role: Role, content: Vec<ContentBlock>) -> Message {
        Message {
            role,
            content,
            ..Default::default()
        }
    }

    /// `expected` names the (message, block) pairs that should carry a breakpoint.
    #[test_case(vec![Message::user("only".into())], &[(0, 0)] ; "single_message")]
    #[test_case(
        vec![
            Message::user("first".into()),
            message(Role::Assistant, vec![text_block("reply")]),
            message(Role::User, vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "ok".into(),
                    is_error: false,
                },
                text_block("second"),
            ]),
        ],
        &[(1, 0), (2, 1)]
        ; "last_two_messages_only"
    )]
    #[test_case(
        vec![
            message(Role::Assistant, vec![
                thinking_block("hmm"),
                text_block("reply"),
                thinking_block("more"),
            ]),
            message(Role::Assistant, vec![thinking_block("stalled")]),
        ],
        &[(0, 1)]
        ; "skips_thinking_blocks"
    )]
    fn cache_control_placement(messages: Vec<Message>, expected: &[(usize, usize)]) {
        let json: Value = serde_json::to_value(build_wire_messages(&messages)).unwrap();

        let marked: Vec<(usize, usize)> = json
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .flat_map(|(msg_idx, msg)| {
                msg["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .filter(|(_, block)| block["cache_control"] == json!({"type": "ephemeral"}))
                    .map(move |(block_idx, _)| (msg_idx, block_idx))
            })
            .collect();
        assert_eq!(marked, expected);
    }

    #[test]
    fn blank_text_blocks_never_reach_the_wire() {
        let messages = vec![
            message(Role::Assistant, vec![text_block(" \n"), text_block("kept")]),
            message(Role::Assistant, vec![text_block("   ")]),
        ];
        let json: Value = serde_json::to_value(build_wire_messages(&messages)).unwrap();

        assert_eq!(json[0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(json[0]["content"][0]["text"], "kept");
        assert_eq!(json[1]["content"].as_array().unwrap().len(), 1);
        assert_eq!(json[1]["content"][0]["text"], EMPTY_RESPONSE_MARKER);
    }

    #[test]
    fn tool_result_with_trailing_image_serializes_valid_wire_blocks() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "[image: pic.png 1KB]".into(),
                    is_error: false,
                },
                ContentBlock::Image {
                    source: crate::ImageSource::new(
                        crate::ImageMediaType::Png,
                        std::sync::Arc::from("aGVsbG8="),
                    ),
                },
            ],
            ..Default::default()
        }];
        let wire = build_wire_messages(&messages);
        let json: Value = serde_json::to_value(&wire).unwrap();

        assert_eq!(json[0]["content"][0]["type"], "tool_result");
        assert_eq!(json[0]["content"][0]["tool_use_id"], "t1");
        assert_eq!(
            json[0]["content"][1],
            json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": "aGVsbG8=",
                },
                "cache_control": {"type": "ephemeral"},
            })
        );
    }

    #[test]
    fn apply_fast_mode_sets_speed_on_capable_model() {
        let model = Model::from_spec("anthropic/claude-opus-4-8").unwrap();
        let mut body = json!({});
        let header = apply_fast_mode(
            &mut body,
            &model,
            RequestOptions {
                fast: true,
                ..Default::default()
            },
        );
        assert!(header);
        assert_eq!(body["speed"], json!("fast"));
    }

    #[test]
    fn apply_fast_mode_ignores_stale_flag_on_ineligible_model() {
        // Sonnet is not fast-capable, so opts.fast=true must still skip `speed`.
        let model = Model::from_spec("anthropic/claude-sonnet-4-5").unwrap();
        let mut body = json!({});
        let header = apply_fast_mode(
            &mut body,
            &model,
            RequestOptions {
                fast: true,
                ..Default::default()
            },
        );
        assert!(!header);
        assert!(body.get("speed").is_none());
    }

    #[test]
    fn apply_fast_mode_off_when_not_requested() {
        let model = Model::from_spec("anthropic/claude-opus-4-8").unwrap();
        let mut body = json!({});
        let header = apply_fast_mode(&mut body, &model, RequestOptions::default());
        assert!(!header);
        assert!(body.get("speed").is_none());
    }

    #[test]
    fn long_context_spec_resolves_to_1m_window() {
        let model = Model::from_spec("anthropic/claude-opus-4-8-1m").unwrap();
        assert_eq!(model.id, "claude-opus-4-8-1m");
        assert_eq!(model.context_window, shared::LONG_CONTEXT_WINDOW);
        assert!(model.id.ends_with(shared::LONG_CONTEXT_SUFFIX));
        // The API has never heard of `-1m`, so strip it before sending.
        assert_eq!(shared::strip_long_context(&model.id), "claude-opus-4-8");
    }

    #[test]
    fn list_models_adds_1m_variant_from_max_input_tokens() {
        // The real /v1/models payload hides the 1M window in `max_input_tokens`.
        let page: ModelsPage = serde_json::from_str(
            r#"{
                "data": [
                    {"id": "claude-opus-4-8", "max_input_tokens": 1000000},
                    {"id": "claude-opus-4-5-20251101", "max_input_tokens": 200000}
                ],
                "has_more": false,
                "last_id": null
            }"#,
        )
        .unwrap();

        let mut models = Vec::new();
        for m in page.data {
            if m.max_input_tokens >= shared::LONG_CONTEXT_WINDOW {
                models.push(format!("{}{}", m.id, shared::LONG_CONTEXT_SUFFIX));
            }
            models.push(m.id);
        }
        models.sort();

        assert_eq!(
            models,
            vec![
                "claude-opus-4-5-20251101".to_string(),
                "claude-opus-4-8".to_string(),
                "claude-opus-4-8-1m".to_string(),
            ]
        );
    }

    #[test]
    fn parse_sse_overloaded_error() {
        smol::block_on(async {
            let input = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n";
            let (tx, _rx) = flume::unbounded();
            let err = parse_sse(mock_response(input), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap_err();
            match err {
                AgentError::Api {
                    status, message, ..
                } => {
                    assert_eq!(status, 529);
                    assert_eq!(message, "Overloaded");
                }
                other => panic!("expected Api error, got: {other:?}"),
            }
        })
    }

    #[test]
    fn parse_sse_unparseable_error() {
        smol::block_on(async {
            let input = b"event: error\ndata: not-json\n";
            let (tx, _rx) = flume::unbounded();
            let err = parse_sse(mock_response(input), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap_err();
            match err {
                AgentError::Api {
                    status, message, ..
                } => {
                    assert_eq!(status, 400);
                    assert_eq!(message, "not-json");
                }
                other => panic!("expected Api error, got: {other:?}"),
            }
        })
    }

    #[test]
    fn parse_sse_malformed_tool_json_yields_empty_object() {
        smol::block_on(async {
            let sse_data = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_2\",\"name\":\"read\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{broken\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n";

            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].1, "read");
            assert_eq!(*tools[0].2, Value::Object(Default::default()));
        })
    }

    #[test]
    fn parse_sse_thinking_blocks() {
        smol::block_on(async {
            let sse_data = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\" think\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig123\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n";

            let (tx, rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, signature }
                    if thinking == "Let me think" && *signature == Some("sig123".to_string()))
            );
            assert!(
                matches!(&resp.message.content[1], ContentBlock::Text { text } if text == "Hello")
            );

            let thinking_deltas: Vec<_> = rx
                .drain()
                .filter_map(|e| match e {
                    ProviderEvent::ThinkingDelta { text } => Some(text),
                    _ => None,
                })
                .collect();
            assert_eq!(thinking_deltas, vec!["Let me", " think"]);
        })
    }

    #[test]
    fn parse_sse_redacted_thinking() {
        smol::block_on(async {
            let sse_data = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque_data\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n";

            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::RedactedThinking { data } if data == "opaque_data")
            );
            assert!(
                matches!(&resp.message.content[1], ContentBlock::Text { text } if text == "Hi")
            );
        })
    }

    #[test]
    fn api_model_info_deserializes_discovery_metadata() {
        let m: ApiModelInfo = serde_json::from_value(json!({
            "id": "claude-x",
            "max_input_tokens": 200_000,
            "max_tokens": 64_000,
        }))
        .unwrap();
        assert_eq!(m.max_input_tokens, 200_000);
        assert_eq!(m.max_tokens, Some(64_000));

        // Fields absent from the response must not fail deserialization.
        let m: ApiModelInfo = serde_json::from_value(json!({ "id": "claude-x" })).unwrap();
        assert_eq!(m.max_input_tokens, 0);
        assert_eq!(m.max_tokens, None);
    }

    #[test]
    fn discovered_metadata_flows_into_model_info() {
        let infos = discovered_model_infos(ApiModelInfo {
            id: "claude-x".into(),
            max_input_tokens: 200_000,
            max_tokens: Some(64_000),
        });
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].context_window, Some(200_000));
        assert_eq!(infos[0].max_output_tokens, Some(64_000));

        // Zero means the API did not report the field.
        let infos = discovered_model_infos(ApiModelInfo {
            id: "claude-x".into(),
            max_input_tokens: 0,
            max_tokens: Some(0),
        });
        assert_eq!(infos[0].context_window, None);
        assert_eq!(infos[0].max_output_tokens, None);
    }

    #[test]
    fn long_context_model_gets_pinned_1m_variant() {
        let infos = discovered_model_infos(ApiModelInfo {
            id: "claude-x".into(),
            max_input_tokens: shared::LONG_CONTEXT_WINDOW,
            max_tokens: Some(64_000),
        });
        assert_eq!(infos.len(), 2);
        assert_eq!(
            infos[0].id,
            format!("claude-x{}", shared::LONG_CONTEXT_SUFFIX)
        );
        assert_eq!(infos[0].context_window, Some(shared::LONG_CONTEXT_WINDOW));
        assert_eq!(infos[0].max_output_tokens, Some(64_000));
        // Only the suffixed id may claim 1M; the base leaves the window
        // unreported so the curated 200K stands and compaction still fires.
        assert_eq!(infos[1].id, "claude-x");
        assert_eq!(infos[1].context_window, None);
    }

    /// The window the gauge trusts must match the window the request gets:
    /// the 1M beta header is suffix-gated, so a base id that inherited a
    /// discovered 1M would overrun the API's 200K cap with no compaction.
    #[test]
    fn discovered_1m_does_not_widen_the_base_id() {
        let infos = discovered_model_infos(ApiModelInfo {
            id: "claude-sonnet-4-5".into(),
            max_input_tokens: shared::LONG_CONTEXT_WINDOW,
            max_tokens: Some(64_000),
        });
        crate::model_registry::set_known_models("anthropic", infos);
        let base = Model::from_spec("anthropic/claude-sonnet-4-5").unwrap();
        assert_eq!(base.context_window, 200_000);
        let long = Model::from_spec("anthropic/claude-sonnet-4-5-1m").unwrap();
        assert_eq!(long.context_window, shared::LONG_CONTEXT_WINDOW);
        crate::model_registry::set_known_models("anthropic", Vec::new());
    }
}
