use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use isahc::ReadResponseExt;
use isahc::config::{Configurable, RedirectPolicy, VersionNegotiation};
use maki_storage::auth::now_millis;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::AgentError;
use crate::model::{ModelInfo, ModelPricing};

use super::auth;

const CLI_MODELS_URL: &str = "https://cli-chat-proxy.grok.com/v1/models-v2";
const CACHE_SUBDIR: &str = "xai";
const CACHE_FILE: &str = "models-v2.json";
const CACHE_SCHEMA: u32 = 1;
const FRESH_TTL_MS: u64 = 15 * 60 * 1000;
const MAX_STALE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedModel {
    pub id: String,
    pub reasoning: bool,
    pub vision: bool,
    pub pricing: ModelPricingDto,
    pub context_window: u32,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ModelPricingDto {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

impl From<CachedModel> for ModelInfo {
    fn from(model: CachedModel) -> Self {
        ModelInfo {
            id: model.id,
            context_window: Some(model.context_window),
            max_output_tokens: Some(model.max_tokens),
            pricing: Some(ModelPricing::per_token(
                model.pricing.input,
                model.pricing.output,
                model.pricing.cache_write,
                model.pricing.cache_read,
            )),
            supports_thinking: Some(model.reasoning),
            supports_vision: Some(model.vision),
            tier: None,
            provider_info: None,
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
    let cache = load_cache(now_millis())?;
    cache
        .models
        .into_iter()
        .find(|model| model.id.eq_ignore_ascii_case(model_id))
}

pub(crate) fn list_models(access: &str, force: bool) -> Result<Vec<ModelInfo>, AgentError> {
    Ok(select_models(Some(access), force)?
        .into_iter()
        .map(ModelInfo::from)
        .collect())
}

pub(crate) fn refresh(access: &str) -> Result<Vec<ModelInfo>, AgentError> {
    list_models(access, true)
}

pub(crate) fn invalidate() {
    if let Some(path) = cache_path() {
        let _ = fs::remove_file(path);
    }
}

fn select_models(access: Option<&str>, force: bool) -> Result<Vec<CachedModel>, AgentError> {
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

    let Some(access) = access else {
        return Ok(curated_fallback());
    };

    match fetch_catalog(access) {
        FetchOutcome::Success(models) => {
            save_cache(&models, now);
            Ok(models)
        }
        FetchOutcome::Auth => Err(AgentError::Api {
            status: 401,
            message: CATALOG_UNAUTHORIZED.into(),
        }),
        FetchOutcome::Permanent => {
            warn!("xAI catalog refresh rejected, using curated fallback");
            invalidate();
            Ok(curated_fallback())
        }
        FetchOutcome::Transient | FetchOutcome::Invalid => {
            if force {
                warn!("xAI catalog force refresh failed, using curated fallback");
                invalidate();
                return Ok(curated_fallback());
            }
            if let Some(cache) = cache {
                warn!("xAI catalog refresh failed transiently, using stale cache");
                return Ok(cache.models);
            }
            warn!("xAI catalog refresh failed, using curated fallback");
            Ok(curated_fallback())
        }
    }
}

fn curated_fallback() -> Vec<CachedModel> {
    super::models()
        .iter()
        .filter_map(|entry| {
            let id = *entry.prefixes.first()?;
            Some(CachedModel {
                id: id.to_string(),
                reasoning: true,
                vision: entry.vision,
                pricing: ModelPricingDto {
                    input: entry.pricing.input,
                    output: entry.pricing.output,
                    cache_write: entry.pricing.cache_write,
                    cache_read: entry.pricing.cache_read,
                },
                context_window: entry.context_window,
                max_tokens: entry
                    .max_output_tokens
                    .unwrap_or(DEFAULT_UNKNOWN_MAX_TOKENS),
            })
        })
        .collect()
}

fn fetch_catalog(access: &str) -> FetchOutcome {
    let client = match isahc::HttpClient::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(FETCH_TIMEOUT)
        .redirect_policy(RedirectPolicy::None)
        // curl carries http2 for OTLP.
        .version_negotiation(VersionNegotiation::http11())
        .build()
    {
        Ok(client) => client,
        Err(_) => return FetchOutcome::Transient,
    };

    let request = match isahc::Request::builder()
        .method("GET")
        .uri(CLI_MODELS_URL)
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {access}"))
        .header("user-agent", crate::providers::user_agent())
        .header("x-xai-token-auth", auth::TOKEN_AUTH)
        .header("x-authenticateresponse", auth::AUTHENTICATE_RESPONSE)
        .header("x-grok-client-identifier", auth::CLIENT_IDENTIFIER)
        .header("x-grok-client-version", env!("CARGO_PKG_VERSION"))
        .header("x-grok-client-mode", "headless")
        .body(())
    {
        Ok(request) => request,
        Err(_) => return FetchOutcome::Transient,
    };

    let mut resp = match client.send(request) {
        Ok(resp) => resp,
        Err(e) => {
            debug!(error = %e, "xAI catalog request failed");
            return FetchOutcome::Transient;
        }
    };

    let status = resp.status().as_u16();
    if status == 401 || status == 403 {
        return FetchOutcome::Auth;
    }
    if status == 408 || status == 429 || status >= 500 {
        return FetchOutcome::Transient;
    }
    if status != 200 {
        return FetchOutcome::Permanent;
    }

    let body = match resp.text() {
        Ok(body) => body,
        Err(_) => return FetchOutcome::Invalid,
    };
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

    let known = super::models()
        .iter()
        .find(|entry| entry.prefixes.iter().any(|p| normalized.starts_with(p)));
    let vision = first_bool(obj, meta, &["acceptsImages"])
        .or_else(|| parse_accepts_images(first_value(obj, meta, &["inputModalities"])))
        .unwrap_or(known.is_some_and(|entry| entry.vision));

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
        .map(|entry| ModelPricingDto {
            input: entry.pricing.input,
            output: entry.pricing.output,
            cache_write: entry.pricing.cache_write,
            cache_read: entry.pricing.cache_read,
        })
        .unwrap_or(ModelPricingDto {
            input: 0.0,
            output: 0.0,
            cache_write: 0.0,
            cache_read: 0.0,
        });

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
    super::models()
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

fn load_cache(now: u64) -> Option<CacheRecord> {
    let path = cache_path()?;
    let bytes = fs::read(path).ok()?;
    let record: CacheRecord = serde_json::from_slice(&bytes).ok()?;
    if record.schema_version != CACHE_SCHEMA || record.models.is_empty() {
        return None;
    }
    if record.fetched_at > now.saturating_add(CLOCK_SKEW_MS)
        || now.saturating_sub(record.fetched_at) > MAX_STALE_MS
    {
        return None;
    }
    Some(record)
}

fn save_cache(models: &[CachedModel], now: u64) {
    let Some(path) = cache_path() else {
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        debug!(error = %e, "failed to create xAI catalog cache dir");
        return;
    }
    let record = CacheRecord {
        schema_version: CACHE_SCHEMA,
        fetched_at: now,
        models: models.to_vec(),
    };
    let Ok(bytes) = serde_json::to_vec_pretty(&record) else {
        return;
    };
    if let Err(e) = maki_storage::atomic_write(&path, &bytes) {
        debug!(error = %e, "failed to write xAI catalog cache");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn fallback_models_include_curated_defaults() {
        let ids: Vec<_> = curated_fallback().into_iter().map(|m| m.id).collect();
        assert!(ids.contains(&"grok-4.6".into()));
        assert!(ids.contains(&"grok-4.3".into()));
    }
}
