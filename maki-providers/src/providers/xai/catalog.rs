use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use maki_storage::auth::now_millis;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::AgentError;
use crate::model::{ModelInfo, ModelPricing};
use crate::providers::status_and_text;

use super::auth;

const CLI_MODELS_URL: &str = "https://cli-chat-proxy.grok.com/v1/models-v2";
const CACHE_SUBDIR: &str = "xai";
const CACHE_FILE: &str = "models-v2.json";
const CACHE_SCHEMA: u32 = 1;
const FRESH_TTL_MS: u64 = 15 * 60 * 1000;
const MAX_STALE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ENTRIES: usize = 256;
const MAX_MODEL_ID_LEN: usize = 128;
const MAX_CONTEXT_WINDOW: u32 = 10_000_000;
const MAX_OUTPUT_TOKENS: u32 = 1_000_000;
const DEFAULT_UNKNOWN_MAX_TOKENS: u32 = 16_384;
const CLOCK_SKEW_MS: u64 = 5 * 60 * 1000;
const API_KEY_ONLY_IDS: &[&str] = &["grok-build-0.1"];
const RESPONSES_BACKEND: &str = "responses";

pub(crate) const INVALID_CATALOG: &str = "xAI model catalog response was invalid";
pub(crate) const CATALOG_UNAUTHORIZED: &str = "xAI model catalog request was unauthorized";
const CATALOG_UNAVAILABLE: &str = "xAI model catalog could not be fetched";

/// The record this process last loaded or listed. Turns read only this copy, so
/// a peer's refresh shows up at this process's next listing.
static CACHE: ArcSwapOption<CacheRecord> = ArcSwapOption::const_empty();

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedModel {
    pub id: String,
    pub reasoning: bool,
    pub vision: bool,
    pub pricing: ModelPricingDto,
    pub context_window: u32,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ModelPricingDto {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

impl From<&ModelPricing> for ModelPricingDto {
    fn from(pricing: &ModelPricing) -> Self {
        Self {
            input: pricing.input,
            output: pricing.output,
            cache_write: pricing.cache_write,
            cache_read: pricing.cache_read,
        }
    }
}

impl From<CachedModel> for ModelInfo {
    fn from(model: CachedModel) -> Self {
        ModelInfo {
            id: model.id,
            context_window: Some(model.context_window),
            max_output_tokens: Some(model.max_tokens),
            pricing: Some(ModelPricing::per_million(
                model.pricing.input,
                model.pricing.output,
                model.pricing.cache_write,
                model.pricing.cache_read,
            )),
            supports_thinking: Some(model.reasoning),
            supports_vision: Some(model.vision),
            tier: None,
            provider_info: None,
            extra: None,
            effort: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheRecord {
    schema_version: u32,
    fetched_at: u64,
    models: Vec<CachedModel>,
}

#[derive(Debug)]
enum FetchOutcome {
    Success(Vec<CachedModel>),
    Auth,
    Permanent,
    Transient,
    Invalid,
}

pub(crate) fn cached_model(model_id: &str) -> Option<CachedModel> {
    let now = now_millis();
    CACHE
        .load_full()
        .filter(|record| usable(record, now))
        .or_else(|| load_cache(now))?
        .models
        .iter()
        .find(|model| model.id.eq_ignore_ascii_case(model_id))
        .cloned()
}

pub(crate) async fn list_models(access: &str, force: bool) -> Result<Vec<ModelInfo>, AgentError> {
    Ok(select_models(access, force)
        .await?
        .into_iter()
        .map(ModelInfo::from)
        .collect())
}

pub(crate) async fn refresh(access: &str) -> Result<Vec<ModelInfo>, AgentError> {
    list_models(access, true).await
}

pub(crate) fn invalidate() {
    CACHE.store(None);
    if let Some(path) = cache_path() {
        let _ = fs::remove_file(path);
    }
}

async fn select_models(access: &str, force: bool) -> Result<Vec<CachedModel>, AgentError> {
    let now = now_millis();
    let cache = load_cache(now);

    if !force
        && let Some(cache) = &cache
        && now.saturating_sub(cache.fetched_at) < FRESH_TTL_MS
    {
        debug!(
            age_ms = now.saturating_sub(cache.fetched_at),
            "using fresh xAI catalog cache"
        );
        return Ok(cache.models.clone());
    }

    match fetch_catalog(access).await {
        FetchOutcome::Success(models) => {
            save_cache(&models, now);
            Ok(models)
        }
        FetchOutcome::Auth => Err(AgentError::api(401, CATALOG_UNAUTHORIZED)),
        FetchOutcome::Permanent => {
            invalidate();
            Err(unavailable())
        }
        FetchOutcome::Transient | FetchOutcome::Invalid => {
            if force {
                invalidate();
                return Err(unavailable());
            }
            if let Some(cache) = cache {
                warn!("xAI catalog refresh failed transiently, using stale cache");
                return Ok(cache.models.clone());
            }
            Err(unavailable())
        }
    }
}

/// A failed listing is not an empty catalog: callers fall back to the static
/// specs and warn, and discovery must not read it as a live answer.
fn unavailable() -> AgentError {
    AgentError::Config {
        message: CATALOG_UNAVAILABLE.into(),
    }
}

async fn fetch_catalog(access: &str) -> FetchOutcome {
    let Ok(client) = auth::http_client(FETCH_TIMEOUT) else {
        return FetchOutcome::Transient;
    };

    let Ok(request) = client
        .get(CLI_MODELS_URL)
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {access}"))
        .header("x-xai-token-auth", auth::TOKEN_AUTH)
        .header("x-authenticateresponse", auth::AUTHENTICATE_RESPONSE)
        .header("x-grok-client-identifier", auth::CLIENT_IDENTIFIER)
        .header("x-grok-client-version", auth::GROK_CLI_VERSION)
        .header("x-grok-client-mode", "headless")
        .build()
    else {
        return FetchOutcome::Transient;
    };

    let (status, body) = match status_and_text(&client, request).await {
        Ok(result) => result,
        Err(e) => {
            debug!(error = %e, "xAI catalog request failed");
            return FetchOutcome::Transient;
        }
    };

    if status == 401 || status == 403 {
        return FetchOutcome::Auth;
    }
    if status == 408 || status == 429 || status >= 500 {
        return FetchOutcome::Transient;
    }
    if status != 200 {
        return FetchOutcome::Permanent;
    }

    match normalize_catalog_payload(&body) {
        Ok(models) => FetchOutcome::Success(models),
        Err(_) => FetchOutcome::Invalid,
    }
}

pub(crate) fn normalize_catalog_payload(body: &str) -> Result<Vec<CachedModel>, AgentError> {
    let root: serde_json::Value = serde_json::from_str(body)?;
    let data = root
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AgentError::Config {
            message: INVALID_CATALOG.into(),
        })?;
    if data.len() > MAX_ENTRIES {
        return Err(AgentError::Config {
            message: INVALID_CATALOG.into(),
        });
    }

    let mut models = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut malformed = 0usize;
    for entry in data {
        match normalize_entry(entry) {
            EntryResult::Model(model) => {
                let key = model.id.to_ascii_lowercase();
                if !seen.insert(key) {
                    continue;
                }
                models.push(model);
            }
            EntryResult::Excluded => {}
            EntryResult::Malformed => malformed += 1,
        }
    }

    if models.is_empty() && malformed > 0 {
        return Err(AgentError::Config {
            message: INVALID_CATALOG.into(),
        });
    }
    Ok(models)
}

enum EntryResult {
    Model(CachedModel),
    Excluded,
    Malformed,
}

fn normalize_entry(value: &serde_json::Value) -> EntryResult {
    let obj = match value.as_object() {
        Some(obj) => obj,
        None => return EntryResult::Malformed,
    };
    let meta = obj.get("_meta").and_then(|v| v.as_object());

    let id = first_string(obj, meta, &["model", "modelId", "id"]);
    let Some(id) = safe_model_id(id.as_deref()) else {
        return EntryResult::Malformed;
    };
    let normalized = id.to_ascii_lowercase();
    if API_KEY_ONLY_IDS.contains(&normalized.as_str()) || has_api_key_only(obj, meta) {
        return EntryResult::Excluded;
    }
    if first_bool(obj, meta, &["hidden"]) == Some(true) {
        return EntryResult::Excluded;
    }

    let backend = first_string(obj, meta, &["apiBackend", "api_backend"]);
    let Some(backend) = backend else {
        return EntryResult::Malformed;
    };
    if !backend.eq_ignore_ascii_case(RESPONSES_BACKEND) {
        return EntryResult::Excluded;
    }

    let Some(context_window) = first_positive_u32(
        obj,
        meta,
        &["contextWindow", "context_window", "totalContextTokens"],
        MAX_CONTEXT_WINDOW,
    ) else {
        return EntryResult::Malformed;
    };

    let supplied_max = first_value(obj, meta, &["maxCompletionTokens", "max_completion_tokens"]);
    let max_tokens = match supplied_max {
        None => known_max_tokens(&normalized).unwrap_or(DEFAULT_UNKNOWN_MAX_TOKENS),
        Some(value) => {
            let Some(max) = positive_u32(value, MAX_OUTPUT_TOKENS) else {
                return EntryResult::Malformed;
            };
            if max > context_window {
                return EntryResult::Malformed;
            }
            max
        }
    };

    let known = super::SPEC
        .models()
        .iter()
        .find(|entry| entry.prefixes.iter().any(|p| normalized.starts_with(p)));
    let vision = first_bool(obj, meta, &["acceptsImages"])
        .or_else(|| parse_accepts_images(first_value(obj, meta, &["inputModalities"])))
        .or_else(|| known.and_then(|entry| entry.supports_vision))
        .unwrap_or(false);

    let supports_effort = first_bool(
        obj,
        meta,
        &["supportsReasoningEffort", "supports_reasoning_effort"],
    );
    let explicit_reasoning = first_bool(obj, meta, &["reasoning", "supportsReasoning"]);
    let reasoning = explicit_reasoning.unwrap_or_else(|| {
        supports_effort.unwrap_or_else(|| {
            first_value(obj, meta, &["reasoningEfforts", "reasoning_efforts"])
                .and_then(|v| v.as_array())
                .is_some_and(|arr| !arr.is_empty())
                || first_string(obj, meta, &["reasoningEffort", "reasoning_effort"]).is_some()
                || known.is_some()
        })
    });

    let pricing = known
        .and_then(|entry| entry.pricing.as_ref())
        .map(ModelPricingDto::from)
        .unwrap_or_default();

    EntryResult::Model(CachedModel {
        id,
        reasoning: reasoning && supports_effort != Some(false),
        vision,
        pricing,
        context_window,
        max_tokens: max_tokens.min(context_window),
    })
}

fn known_max_tokens(model_id: &str) -> Option<u32> {
    super::SPEC
        .models()
        .iter()
        .find(|entry| entry.prefixes.iter().any(|p| model_id.starts_with(p)))
        .and_then(|entry| entry.max_output_tokens)
}

fn safe_model_id(value: Option<&str>) -> Option<String> {
    let id = value?.trim();
    if id.is_empty() || id.len() > MAX_MODEL_ID_LEN {
        return None;
    }
    let mut chars = id.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphanumeric() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-')) {
        return None;
    }
    Some(id.to_string())
}

fn has_api_key_only(
    obj: &serde_json::Map<String, serde_json::Value>,
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> bool {
    for key in ["apiKey", "api_key", "envKey", "env_key"] {
        if obj.contains_key(key) || meta.is_some_and(|m| m.contains_key(key)) {
            return true;
        }
    }
    first_string(
        obj,
        meta,
        &["authScheme", "auth_scheme", "authType", "auth_type"],
    )
    .is_some_and(|scheme| {
        matches!(
            scheme.to_ascii_lowercase().as_str(),
            "api-key" | "api_key" | "apikey" | "bearer-api-key"
        )
    })
}

fn parse_accepts_images(value: Option<&serde_json::Value>) -> Option<bool> {
    let arr = value?.as_array()?;
    Some(arr.iter().any(|v| v.as_str() == Some("image")))
}

fn first_string(
    obj: &serde_json::Map<String, serde_json::Value>,
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
    keys: &[&str],
) -> Option<String> {
    first_value(obj, meta, keys)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn first_bool(
    obj: &serde_json::Map<String, serde_json::Value>,
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
    keys: &[&str],
) -> Option<bool> {
    first_value(obj, meta, keys)?.as_bool()
}

fn first_positive_u32(
    obj: &serde_json::Map<String, serde_json::Value>,
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
    keys: &[&str],
    maximum: u32,
) -> Option<u32> {
    positive_u32(first_value(obj, meta, keys)?, maximum)
}

fn first_value<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    meta: Option<&'a serde_json::Map<String, serde_json::Value>>,
    keys: &[&str],
) -> Option<&'a serde_json::Value> {
    for key in keys {
        if let Some(value) = obj.get(*key) {
            return Some(value);
        }
    }
    if let Some(meta) = meta {
        for key in keys {
            if let Some(value) = meta.get(*key) {
                return Some(value);
            }
        }
    }
    None
}

fn positive_u32(value: &serde_json::Value, maximum: u32) -> Option<u32> {
    let n = value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))?;
    let n = u32::try_from(n).ok()?;
    (n > 0 && n <= maximum).then_some(n)
}

fn cache_path() -> Option<PathBuf> {
    let dir = maki_storage::paths::cache_dir().ok()?;
    Some(dir.join(CACHE_SUBDIR).join(CACHE_FILE))
}

fn usable(record: &CacheRecord, now: u64) -> bool {
    record.schema_version == CACHE_SCHEMA
        && !record.models.is_empty()
        && record.fetched_at <= now.saturating_add(CLOCK_SKEW_MS)
        && now.saturating_sub(record.fetched_at) <= MAX_STALE_MS
}

fn load_cache(now: u64) -> Option<Arc<CacheRecord>> {
    let bytes = fs::read(cache_path()?).ok()?;
    let record = serde_json::from_slice::<CacheRecord>(&bytes)
        .ok()
        .filter(|record| usable(record, now))
        .map(Arc::new)?;
    CACHE.store(Some(Arc::clone(&record)));
    Some(record)
}

fn save_cache(models: &[CachedModel], now: u64) {
    let record = Arc::new(CacheRecord {
        schema_version: CACHE_SCHEMA,
        fetched_at: now,
        models: models.to_vec(),
    });
    CACHE.store(Some(Arc::clone(&record)));
    let Some(path) = cache_path() else {
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        debug!(error = %e, "failed to create xAI catalog cache dir");
        return;
    }
    let Ok(bytes) = serde_json::to_vec_pretty(&*record) else {
        return;
    };
    if let Err(e) = maki_storage::atomic_write(&path, &bytes) {
        debug!(error = %e, "failed to write xAI catalog cache");
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::test_support::isolated_home;

    const ACCESS_TOKEN: &str = "unused-access-token";
    const CACHED_ID: &str = "grok-4.6";
    const OWN_WINDOW: u32 = 200_000;
    const PEER_WINDOW: u32 = 300_000;
    const CACHED_MAX_TOKENS: u32 = 16_384;
    const FOLLOWED_PEER_UNLISTED: &str = "the copy followed a peer's write before any listing";
    const LISTING_MISSED_PEER: &str = "a listing did not pick up the record a peer wrote";

    const VALID_PAYLOAD: &str = r#"{
        "object": "list",
        "data": [
            {
                "id": "grok-4.6",
                "model": "grok-4.6",
                "name": "Grok 4.6",
                "api_backend": "responses",
                "context_window": 500000,
                "supports_reasoning_effort": true,
                "reasoning_efforts": ["low", "medium", "high", "xhigh"],
                "acceptsImages": true
            },
            {
                "id": "grok-build-0.1",
                "model": "grok-build-0.1",
                "api_backend": "responses",
                "context_window": 128000
            },
            {
                "id": "hidden-model",
                "model": "hidden-model",
                "api_backend": "responses",
                "context_window": 128000,
                "hidden": true
            }
        ]
    }"#;

    #[test]
    fn normalize_keeps_oauth_models_and_drops_api_key_only() {
        let models = normalize_catalog_payload(VALID_PAYLOAD).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "grok-4.6");
        assert!(models[0].reasoning);
        assert!(models[0].vision);
        assert_eq!(models[0].context_window, 500_000);
        assert_eq!(models[0].pricing.input, 2.0);
    }

    #[test]
    fn normalize_rejects_all_malformed_payloads() {
        const PAYLOAD: &str = r#"{
            "data": [
                {"id": "bad", "api_backend": "responses"},
                {"id": "also-bad", "context_window": 1000}
            ]
        }"#;
        let err = normalize_catalog_payload(PAYLOAD).unwrap_err();
        assert_eq!(err.to_string(), INVALID_CATALOG);
    }

    #[test]
    fn normalize_excludes_non_responses_backends() {
        const PAYLOAD: &str = r#"{
            "data": [
                {
                    "id": "grok-chat",
                    "api_backend": "chat-completions",
                    "context_window": 128000
                }
            ]
        }"#;
        let models = normalize_catalog_payload(PAYLOAD).unwrap();
        assert!(models.is_empty());
    }

    #[test]
    fn reasoning_denied_by_supports_flag() {
        const PAYLOAD: &str = r#"{
            "data": [
                {
                    "id": "grok-4.3",
                    "api_backend": "responses",
                    "context_window": 1000000,
                    "supports_reasoning_effort": false,
                    "reasoning_efforts": ["low", "medium", "high"]
                }
            ]
        }"#;
        let models = normalize_catalog_payload(PAYLOAD).unwrap();
        assert_eq!(models.len(), 1);
        assert!(!models[0].reasoning);
        assert_eq!(models[0].context_window, 1_000_000);
    }

    fn cached(context_window: u32) -> CachedModel {
        CachedModel {
            id: CACHED_ID.into(),
            reasoning: true,
            vision: false,
            pricing: ModelPricingDto::default(),
            context_window,
            max_tokens: CACHED_MAX_TOKENS,
        }
    }

    fn cached_window() -> Option<u32> {
        cached_model(CACHED_ID).map(|model| model.context_window)
    }

    fn save_then_remove_file(now: u64) {
        save_cache(&[cached(OWN_WINDOW)], now);
        fs::remove_file(cache_path().unwrap()).unwrap();
    }

    fn save_then_invalidate(now: u64) {
        save_cache(&[cached(OWN_WINDOW)], now);
        invalidate();
    }

    fn invalidate_then_peer_saves(now: u64) {
        save_then_invalidate(now);
        let peer = CacheRecord {
            schema_version: CACHE_SCHEMA,
            fetched_at: now,
            models: vec![cached(PEER_WINDOW)],
        };
        let path = cache_path().unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(&peer).unwrap()).unwrap();
    }

    fn save_past_max_stale(now: u64) {
        save_cache(&[cached(OWN_WINDOW)], now - MAX_STALE_MS - 1);
    }

    fn save_ahead_of_clock(now: u64) {
        save_cache(&[cached(OWN_WINDOW)], now + 2 * CLOCK_SKEW_MS);
    }

    #[test_case(save_then_remove_file, Some(OWN_WINDOW) ; "saved_record_outlives_its_file")]
    #[test_case(save_then_invalidate, None ; "invalidate_clears_the_copy")]
    #[test_case(invalidate_then_peer_saves, Some(PEER_WINDOW) ; "emptied_copy_refills_from_disk")]
    #[test_case(save_past_max_stale, None ; "record_past_max_stale_is_not_served")]
    #[test_case(save_ahead_of_clock, None ; "record_ahead_of_the_clock_is_not_served")]
    fn cached_model_answers_from_memory(setup: fn(u64), expected: Option<u32>) {
        let _home = isolated_home();
        setup(now_millis());
        assert_eq!(cached_window(), expected);
    }

    #[tokio::test]
    async fn listing_picks_up_a_record_a_peer_wrote() {
        let _home = isolated_home();
        let now = now_millis();
        save_cache(&[cached(OWN_WINDOW)], now);
        let peer = CacheRecord {
            schema_version: CACHE_SCHEMA,
            fetched_at: now,
            models: vec![cached(PEER_WINDOW)],
        };
        fs::write(cache_path().unwrap(), serde_json::to_vec(&peer).unwrap()).unwrap();
        assert_eq!(
            cached_window(),
            Some(OWN_WINDOW),
            "{FOLLOWED_PEER_UNLISTED}"
        );

        select_models(ACCESS_TOKEN, false).await.unwrap();
        assert_eq!(cached_window(), Some(PEER_WINDOW), "{LISTING_MISSED_PEER}");
    }
}
