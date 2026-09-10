use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_storage::id::SessionRef;
use serde::Deserialize;
use serde_json::Value;

use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider};
use crate::types::{ModelUsageRow, ProviderUsage, UsageLimit};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, dialect};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyPool, ResolvedAuth};

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "regolo",
    api_key_env: "REGOLO_API_KEY",
    base_url: "https://api.regolo.ai/v1",
    max_tokens_field: "max_completion_tokens",
    include_stream_usage: true,
    provider_name: "Regolo",
};

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: "regolo",
    display_name: "Regolo",
    protocol: maki_config::providers::Protocol::Openai,
    default_base_url: "https://api.regolo.ai/v1",
    default_api_key_env: "REGOLO_API_KEY",
    default_model: "regolo/qwen3-coder-next",
    plans: None,
    login_url: Some("https://dashboard.regolo.ai"),
    needs_url: false,
});

/// Curated tier defaults only: pricing, context windows and capabilities for
/// the full catalogue come live from `/v1/models` joined with
/// `/model_group/info` in [`Regolo::list_models`]. Context windows mirror the
/// group's `max_input_tokens`, the same field [`join_model_info`] prefers, not
/// its `max_tokens` (input plus output) which would let a session grow past
/// what the upstream accepts.
pub(crate) const fn models() -> &'static [ModelEntry] {
    &[
        ModelEntry {
            prefixes: &["qwen3.5-122b"],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 1.00,
                output: 4.20,
                cache_write: 0.00,
                cache_read: 0.00,
                fast: None,
                subsidised_by: None,
            },
            max_output_tokens: Some(120_000),
            context_window: 120_000,
        },
        ModelEntry {
            prefixes: &["qwen3-coder-next"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            vision: false,
            default: true,
            pricing: ModelPricing {
                input: 0.50,
                output: 2.00,
                cache_write: 0.00,
                cache_read: 0.00,
                fast: None,
                subsidised_by: None,
            },
            max_output_tokens: Some(120_000),
            context_window: 120_000,
        },
        ModelEntry {
            prefixes: &["qwen3.5-9b"],
            tier: ModelTier::Weak,
            family: ModelFamily::Generic,
            vision: false,
            default: true,
            pricing: ModelPricing {
                input: 0.07,
                output: 0.35,
                cache_write: 0.00,
                cache_read: 0.00,
                fast: None,
                subsidised_by: None,
            },
            // The group advertises 120k output against an 80k input window;
            // capped so a tier default never promises more output than the
            // window it has to fit in.
            max_output_tokens: Some(80_000),
            context_window: 80_000,
        },
    ]
}

#[derive(Deserialize)]
struct SpendLogsResponse {
    data: Vec<SpendLogEntry>,
}

#[derive(Deserialize)]
struct SpendLogEntry {
    model_group: String,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    /// USD; rendered as micro-dollars at the boundary to keep `ProviderUsage`
    /// in `Eq` land.
    spend: f64,
}

impl SpendLogsResponse {
    fn into_rows(self) -> Vec<ModelUsageRow> {
        let mut by_model: BTreeMap<String, SpendLogEntry> = BTreeMap::new();
        for entry in self.data {
            by_model
                .entry(entry.model_group.clone())
                .and_modify(|acc| {
                    acc.prompt_tokens += entry.prompt_tokens;
                    acc.completion_tokens += entry.completion_tokens;
                    acc.total_tokens += entry.total_tokens;
                    acc.spend += entry.spend;
                })
                .or_insert(entry);
        }
        let mut rows: Vec<ModelUsageRow> = by_model
            .into_values()
            .map(|e| ModelUsageRow {
                model: e.model_group,
                input_tokens: e.prompt_tokens,
                output_tokens: e.completion_tokens,
                total_tokens: e.total_tokens,
                spend_microdollars: (e.spend * PER_MILLION).round() as u64,
            })
            .collect();
        rows.sort_by_key(|row| Reverse(row.spend_microdollars));
        rows
    }
}

#[derive(Deserialize)]
struct KeyInfoResponse {
    info: KeyInfo,
}

#[derive(Deserialize)]
struct KeyInfo {
    spend: f64,
    max_budget: Option<f64>,
    budget_reset_at: Option<String>,
}

impl From<KeyInfo> for UsageLimit {
    fn from(info: KeyInfo) -> Self {
        let mut detail = format!("${:.2} spent", info.spend);
        if let Some(budget) = info.max_budget {
            detail.push_str(&format!(" of ${budget:.2} budget"));
        }
        let percentage = info
            .max_budget
            .filter(|budget| *budget > 0.0)
            .map(|budget| ((info.spend / budget * 100.0) as u32).min(100));
        let reset_at = info
            .budget_reset_at
            .as_deref()
            .and_then(|at| at.parse::<jiff::Timestamp>().ok())
            .map(|at| at.as_millisecond().max(0) as u64);
        Self {
            label: "Spend".into(),
            percentage,
            reset_at,
            detail: Some(detail),
        }
    }
}

#[derive(Deserialize)]
struct ActivityResponse {
    sum_api_requests: u64,
    sum_total_tokens: u64,
}

/// `/global/activity` is key-scoped despite its name and answers per UTC day.
fn activity_url(root: &str, day: jiff::civil::Date) -> String {
    format!("{root}{ACTIVITY_PATH}?start_date={day}&end_date={day}")
}

/// `/spend/logs/v2` accepts YYYY-MM-DD, end-inclusive. One hour = one row per
/// model, so callers must sum across rows to get per-model daily totals.
fn spend_logs_url(root: &str, day: jiff::civil::Date) -> String {
    format!("{root}{SPEND_LOGS_PATH}?start_date={day}&end_date={day}")
}

fn next_utc_midnight(now: jiff::Timestamp) -> Option<u64> {
    let day = now.as_second().div_euclid(SECONDS_PER_DAY) + 1;
    jiff::Timestamp::from_second(day * SECONDS_PER_DAY)
        .ok()
        .map(|at| at.as_millisecond() as u64)
}

impl From<ActivityResponse> for UsageLimit {
    fn from(activity: ActivityResponse) -> Self {
        Self {
            label: "Today".into(),
            // Regolo tracks a per-account daily token cap (free trial: 1M) but
            // no endpoint reports it, so there is no honest percentage to show.
            percentage: None,
            reset_at: next_utc_midnight(jiff::Timestamp::now()),
            detail: Some(format!(
                "{} requests · {} tokens",
                activity.sum_api_requests, activity.sum_total_tokens
            )),
        }
    }
}

#[derive(Deserialize)]
struct ModelGroupInfoResponse {
    data: Vec<ModelGroup>,
}

#[derive(Deserialize)]
struct ModelGroup {
    model_group: String,
    mode: String,
    input_cost_per_token: Option<f64>,
    output_cost_per_token: Option<f64>,
    max_input_tokens: Option<f64>,
    max_output_tokens: Option<f64>,
    max_tokens: Option<f64>,
    supports_reasoning: bool,
    supports_vision: bool,
}

/// Joins the callable IDs from `/v1/models` with the metadata from
/// `/model_group/info`. Groups in a non-chat `mode` (embedding, rerank, ocr,
/// image, audio) and IDs without a chat group are not agent models. Order
/// follows `ids`, which the compat lister already sorts.
fn join_model_info(ids: Vec<String>, groups: Vec<ModelGroup>) -> Vec<crate::model::ModelInfo> {
    let by_group: BTreeMap<&str, &ModelGroup> = groups
        .iter()
        .filter(|group| group.mode == CHAT_MODE)
        .map(|group| (group.model_group.as_str(), group))
        .collect();
    ids.into_iter()
        .filter_map(|id| {
            let group = by_group.get(id.as_str())?;
            let pricing = match (group.input_cost_per_token, group.output_cost_per_token) {
                (Some(input), Some(output)) => Some(ModelPricing {
                    input: input * PER_MILLION,
                    output: output * PER_MILLION,
                    cache_write: 0.00,
                    cache_read: 0.00,
                    fast: None,
                    subsidised_by: None,
                }),
                _ => None,
            };
            Some(crate::model::ModelInfo {
                context_window: group
                    .max_input_tokens
                    .or(group.max_tokens)
                    .and_then(|tokens| u32::try_from(tokens as u64).ok()),
                max_output_tokens: group
                    .max_output_tokens
                    .and_then(|tokens| u32::try_from(tokens as u64).ok()),
                pricing,
                supports_thinking: Some(group.supports_reasoning),
                supports_vision: Some(group.supports_vision),
                id,
                tier: None,
                provider_info: None,
            })
        })
        .collect()
}

pub struct Regolo {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

const KEY_INFO_PATH: &str = "/key/info";
const ACTIVITY_PATH: &str = "/global/activity";
const SPEND_LOGS_PATH: &str = "/spend/logs/v2";
const MODEL_GROUP_INFO_PATH: &str = "/model_group/info";

/// Regolo's management endpoints live at the host root, outside `/v1`, so
/// they must not be absolute urls: a custom base url or a gateway (Aperture)
/// keeps serving them, an absolute one would bypass it and 401 on its key.
fn root_url(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
}
const CHAT_MODE: &str = "chat";
const PER_MILLION: f64 = 1_000_000.0;
const SECONDS_PER_DAY: i64 = 86_400;

impl Regolo {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve("regolo", CONFIG.api_key_env)?;
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

impl Provider for Regolo {
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
            opts.thinking
                .apply_reasoning_effort(&mut body, &dialect::STANDARD, model);
            self.compat
                .do_stream(model, &[], &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let ids = self
                .compat
                .do_list_models(&auth)
                .await?
                .into_iter()
                .map(|info| info.id)
                .collect::<Vec<_>>();
            let root = root_url(&self.compat.base_url(&auth));
            let groups = self
                .compat
                .get_text(&auth, &format!("{root}{MODEL_GROUP_INFO_PATH}"))
                .await
                .ok()
                .and_then(|body| serde_json::from_str::<ModelGroupInfoResponse>(&body).ok());
            match groups {
                Some(groups) => Ok(join_model_info(ids, groups.data)),
                // The endpoint hiccups (it has 500ed): keep the live ids
                // without metadata instead of dropping to the static few.
                None => Ok(ids
                    .into_iter()
                    .map(|id| crate::model::ModelInfo {
                        id,
                        ..Default::default()
                    })
                    .collect()),
            }
        })
    }

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let root = root_url(&self.compat.base_url(&auth));
            let key_body = self
                .compat
                .get_text(&auth, &format!("{root}{KEY_INFO_PATH}"))
                .await?;
            let key_info: KeyInfoResponse = serde_json::from_str(&key_body)?;
            let mut limits = vec![UsageLimit::from(key_info.info)];
            let today = jiff::Timestamp::now()
                .to_zoned(jiff::tz::TimeZone::UTC)
                .date();
            if let Ok(body) = self
                .compat
                .get_text(&auth, &activity_url(&root, today))
                .await
                && let Ok(activity) = serde_json::from_str::<ActivityResponse>(&body)
            {
                limits.push(activity.into());
            }
            let by_model_today = self
                .compat
                .get_text(&auth, &spend_logs_url(&root, today))
                .await
                .ok()
                .and_then(|body| serde_json::from_str::<SpendLogsResponse>(&body).ok())
                .map(SpendLogsResponse::into_rows)
                .unwrap_or_default();
            Ok(Some(ProviderUsage {
                plan: None,
                limits,
                by_model_today,
            }))
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
    use super::*;
    use crate::manifest::ManifestRegistry;

    const BUDGETED_KEY_INFO: &str = r#"{"key":"abc","info":{"key_alias":"me@example.com","spend":2.5,"max_budget":10.0,"budget_reset_at":"2026-09-01T00:00:00Z"}}"#;
    const UNBUDGETED_KEY_INFO: &str =
        r#"{"key":"abc","info":{"spend":0.42,"max_budget":null,"budget_reset_at":null}}"#;

    #[test]
    fn budgeted_key_reports_percentage_and_reset() {
        let resp: KeyInfoResponse = serde_json::from_str(BUDGETED_KEY_INFO).unwrap();
        let limit = UsageLimit::from(resp.info);
        assert_eq!(limit.percentage, Some(25));
        assert_eq!(
            limit.detail.as_deref(),
            Some("$2.50 spent of $10.00 budget")
        );
        assert_eq!(limit.reset_at, Some(1_788_220_800_000));
    }

    #[test]
    fn unbudgeted_key_reports_spend_without_percentage() {
        let resp: KeyInfoResponse = serde_json::from_str(UNBUDGETED_KEY_INFO).unwrap();
        let limit = UsageLimit::from(resp.info);
        assert_eq!(limit.percentage, None);
        assert_eq!(limit.reset_at, None);
        assert_eq!(limit.detail.as_deref(), Some("$0.42 spent"));
    }

    const ACTIVITY_TODAY: &str = r#"{"sum_api_requests":14,"sum_total_tokens":29027}"#;

    #[test]
    fn activity_maps_to_daily_counts_without_percentage() {
        let activity: ActivityResponse = serde_json::from_str(ACTIVITY_TODAY).unwrap();
        let limit = UsageLimit::from(activity);
        assert_eq!(limit.label, "Today");
        assert_eq!(limit.percentage, None);
        assert!(limit.reset_at.is_some());
        assert_eq!(limit.detail.as_deref(), Some("14 requests · 29027 tokens"));
    }

    #[test]
    fn next_utc_midnight_is_following_day_start() {
        let now = "2026-08-25T13:45:10Z".parse::<jiff::Timestamp>().unwrap();
        assert_eq!(next_utc_midnight(now), Some(1_787_702_400_000));
    }

    #[test]
    fn root_url_strips_the_version_segment() {
        assert_eq!(
            root_url("https://api.regolo.ai/v1"),
            "https://api.regolo.ai"
        );
        assert_eq!(
            root_url("https://api.regolo.ai/v1/"),
            "https://api.regolo.ai"
        );
        assert_eq!(root_url("https://api.regolo.ai"), "https://api.regolo.ai");
    }

    #[test]
    fn manifest_lists_the_catalogued_default_model() {
        let manifest = ManifestRegistry::get(CONFIG.slug).expect("regolo is a builtin");
        assert!(
            manifest
                .models
                .iter()
                .any(|m| m.prefixes == ["qwen3-coder-next"])
        );
    }

    const MODEL_GROUPS_FIXTURE: &str = r#"{"data":[
        {
            "model_group": "glm5.2", "mode": "chat",
            "input_cost_per_token": 2e-06, "output_cost_per_token": 5.2e-06,
            "max_input_tokens": 96000.0, "max_output_tokens": 96000.0, "max_tokens": null,
            "supports_reasoning": true, "supports_vision": false
        },
        {
            "model_group": "qwen3-coder-next", "mode": "chat",
            "input_cost_per_token": 5e-07, "output_cost_per_token": 2e-06,
            "max_input_tokens": null, "max_output_tokens": 120000.0, "max_tokens": 240000.0,
            "supports_reasoning": false, "supports_vision": true
        },
        {
            "model_group": "Qwen3-Embedding-8B", "mode": "embedding",
            "input_cost_per_token": 0.0, "output_cost_per_token": 0.0,
            "max_input_tokens": null, "max_output_tokens": null, "max_tokens": null,
            "supports_reasoning": false, "supports_vision": false
        }
    ]}"#;

    fn join_fixture(ids: &[&str]) -> Vec<crate::model::ModelInfo> {
        let groups: ModelGroupInfoResponse = serde_json::from_str(MODEL_GROUPS_FIXTURE).unwrap();
        join_model_info(
            ids.iter().map(|id| (*id).to_string()).collect(),
            groups.data,
        )
    }

    #[test]
    fn group_metadata_maps_to_pricing_context_and_capabilities() {
        let mut infos = join_fixture(&["glm5.2"]);
        assert_eq!(infos.len(), 1);
        let info = infos.pop().unwrap();
        assert_eq!(info.id, "glm5.2");
        assert_eq!(info.context_window, Some(96_000));
        assert_eq!(info.max_output_tokens, Some(96_000));
        let pricing = info.pricing.unwrap();
        assert_eq!(pricing.input, 2.0);
        assert_eq!(pricing.output, 5.2);
        assert_eq!(info.supports_thinking, Some(true));
        assert_eq!(info.supports_vision, Some(false));
    }

    #[test]
    fn missing_input_window_falls_back_to_max_tokens() {
        let info = join_fixture(&["qwen3-coder-next"]).pop().unwrap();
        assert_eq!(info.context_window, Some(240_000));
        assert_eq!(info.supports_thinking, Some(false));
    }

    #[test]
    fn non_chat_groups_and_ids_without_group_are_dropped() {
        let infos = join_fixture(&["Qwen3-Embedding-8B", "glm5.2", "brand-new-model"]);
        let ids: Vec<&str> = infos.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["glm5.2"]);
    }

    const SPEND_LOGS_FIXTURE: &str = r#"{
      "data": [
        {
          "request_id": "h1", "api_key": "k",
          "model_group": "qwen3-coder-next", "key_alias": "a",
          "startTime": "2026-08-25T09:00:00+00:00", "endTime": "2026-08-25T10:00:00+00:00",
          "completionStartTime": null,
          "api_requests": 4, "total_tokens": 20000,
          "prompt_tokens": 15000, "completion_tokens": 5000,
          "spend": 0.010000, "request_duration_ms": 3600000
        },
        {
          "request_id": "h2", "api_key": "k",
          "model_group": "qwen3-coder-next", "key_alias": "a",
          "startTime": "2026-08-25T10:00:00+00:00", "endTime": "2026-08-25T11:00:00+00:00",
          "completionStartTime": null,
          "api_requests": 3, "total_tokens": 8000,
          "prompt_tokens": 5000, "completion_tokens": 3000,
          "spend": 0.004200, "request_duration_ms": 3600000
        },
        {
          "request_id": "h3", "api_key": "k",
          "model_group": "glm5.2", "key_alias": "a",
          "startTime": "2026-08-25T09:00:00+00:00", "endTime": "2026-08-25T10:00:00+00:00",
          "completionStartTime": null,
          "api_requests": 2, "total_tokens": 360,
          "prompt_tokens": 200, "completion_tokens": 160,
          "spend": 0.000900, "request_duration_ms": 3600000
        }
      ]
    }"#;

    #[test]
    fn spend_logs_aggregate_by_model_and_rank_by_spend() {
        let resp: SpendLogsResponse = serde_json::from_str(SPEND_LOGS_FIXTURE).unwrap();
        let rows = resp.into_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].model, "qwen3-coder-next");
        assert_eq!(rows[0].input_tokens, 20_000);
        assert_eq!(rows[0].output_tokens, 8_000);
        assert_eq!(rows[0].total_tokens, 28_000);
        assert_eq!(rows[0].spend_microdollars, 14_200);
        assert_eq!(rows[1].model, "glm5.2");
        assert_eq!(rows[1].spend_microdollars, 900);
    }
}
