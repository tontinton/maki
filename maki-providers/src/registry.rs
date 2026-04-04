use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use arc_swap::ArcSwap;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::model::{ModelEntry, ModelPricing, ModelTier};
use crate::provider::ProviderKind;

// If you change this, remember to change it in install.sh too.
const CACHE_FILENAME: &str = "models-cache.json";
const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const WEAK_PRICE_THRESHOLD: f64 = 2.0;
const MEDIUM_PRICE_THRESHOLD: f64 = 10.0;

static FALLBACK_JSON: &str = include_str!("fallback_models.json");
static REGISTRY: OnceLock<ModelRegistry> = OnceLock::new();

struct TierRule {
    family_prefix: &'static str,
    tier: ModelTier,
}

struct ProviderSpec {
    provider: ProviderKind,
    json_key: &'static str,
    tier_rules: &'static [TierRule],
    defaults: &'static [(ModelTier, &'static str)],
    has_responses_api: bool,
    legacy_api_prefixes: &'static [&'static str],
    supports_thinking: bool,
    supports_tool_examples: bool,
}

impl ProviderSpec {
    fn infer_tier(&self, family: &str, output_price: f64) -> ModelTier {
        let mut best: Option<&TierRule> = None;
        for rule in self.tier_rules {
            if family.starts_with(rule.family_prefix)
                && best.is_none_or(|b| rule.family_prefix.len() > b.family_prefix.len())
            {
                best = Some(rule);
            }
        }
        best.map(|r| r.tier)
            .unwrap_or_else(|| tier_from_price(output_price))
    }

    fn is_default(&self, model_id: &str, tier: ModelTier) -> bool {
        self.defaults
            .iter()
            .any(|&(t, id)| t == tier && model_id == id)
    }

    fn uses_responses_api(&self, model_id: &str) -> bool {
        self.has_responses_api
            && !self
                .legacy_api_prefixes
                .iter()
                .any(|&p| model_id.starts_with(p))
    }
}

static ANTHROPIC_SPEC: ProviderSpec = ProviderSpec {
    provider: ProviderKind::Anthropic,
    json_key: "anthropic",
    tier_rules: &[
        TierRule {
            family_prefix: "claude-haiku",
            tier: ModelTier::Weak,
        },
        TierRule {
            family_prefix: "claude-sonnet",
            tier: ModelTier::Medium,
        },
        TierRule {
            family_prefix: "claude-opus",
            tier: ModelTier::Strong,
        },
    ],
    defaults: &[
        (ModelTier::Weak, "claude-haiku-4-5"),
        (ModelTier::Medium, "claude-sonnet-4-6"),
        (ModelTier::Strong, "claude-opus-4-6"),
    ],
    has_responses_api: false,
    legacy_api_prefixes: &[],
    supports_thinking: true,
    supports_tool_examples: true,
};

static OPENAI_SPEC: ProviderSpec = ProviderSpec {
    provider: ProviderKind::OpenAi,
    json_key: "openai",
    tier_rules: &[
        TierRule {
            family_prefix: "gpt-codex-mini",
            tier: ModelTier::Medium,
        },
        TierRule {
            family_prefix: "gpt-codex-spark",
            tier: ModelTier::Medium,
        },
        TierRule {
            family_prefix: "gpt-codex",
            tier: ModelTier::Strong,
        },
        TierRule {
            family_prefix: "gpt-nano",
            tier: ModelTier::Weak,
        },
        TierRule {
            family_prefix: "gpt-mini",
            tier: ModelTier::Weak,
        },
        TierRule {
            family_prefix: "gpt-pro",
            tier: ModelTier::Strong,
        },
        TierRule {
            family_prefix: "o-mini",
            tier: ModelTier::Medium,
        },
        TierRule {
            family_prefix: "o-pro",
            tier: ModelTier::Strong,
        },
        TierRule {
            family_prefix: "o",
            tier: ModelTier::Strong,
        },
    ],
    defaults: &[
        (ModelTier::Weak, "gpt-5.4-nano"),
        (ModelTier::Medium, "gpt-4.1"),
        (ModelTier::Strong, "gpt-5.4"),
    ],
    has_responses_api: true,
    legacy_api_prefixes: &["gpt-4.1", "gpt-4.1-mini", "gpt-4.1-nano"],
    supports_thinking: false,
    supports_tool_examples: true,
};

static REGISTRY_SPECS: &[&ProviderSpec] = &[&ANTHROPIC_SPEC, &OPENAI_SPEC];

fn spec_for_provider(provider: ProviderKind) -> Option<&'static ProviderSpec> {
    REGISTRY_SPECS
        .iter()
        .find(|s| s.provider == provider)
        .copied()
}

pub fn provider_capabilities(provider: ProviderKind) -> (bool, bool) {
    spec_for_provider(provider)
        .map(|s| (s.supports_thinking, s.supports_tool_examples))
        .unwrap_or((false, true))
}

struct ModelRegistry {
    providers: HashMap<&'static str, ArcSwap<Vec<ModelEntry>>>,
    cached_etag: Mutex<Option<String>>,
}

#[derive(Deserialize)]
struct CacheFile {
    etag: Option<String>,
    data: serde_json::Value,
}

#[derive(Deserialize)]
struct ProviderData {
    models: HashMap<String, RawModel>,
}

#[derive(Deserialize)]
struct RawModel {
    id: String,
    family: Option<String>,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    tool_call: bool,
    cost: Option<RawCost>,
    limit: Option<RawLimit>,
    modalities: Option<RawModalities>,
}

#[derive(Deserialize)]
struct RawCost {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

#[derive(Deserialize)]
struct RawLimit {
    context: Option<u32>,
    output: Option<u32>,
}

#[derive(Deserialize)]
struct RawModalities {
    output: Option<Vec<String>>,
}

fn cache_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".maki").join(CACHE_FILENAME))
}

fn load_cache() -> Option<CacheFile> {
    let path = cache_path()?;
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

fn save_cache(etag: Option<&str>, data: &serde_json::Value) {
    let Some(path) = cache_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let cache = serde_json::json!({
        "etag": etag,
        "data": data,
    });
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn tier_from_price(output_price: f64) -> ModelTier {
    if output_price < WEAK_PRICE_THRESHOLD {
        ModelTier::Weak
    } else if output_price < MEDIUM_PRICE_THRESHOLD {
        ModelTier::Medium
    } else {
        ModelTier::Strong
    }
}

fn parse_models(data: &serde_json::Value, spec: &ProviderSpec) -> Vec<ModelEntry> {
    let Some(provider_data) = data.get(spec.json_key) else {
        return Vec::new();
    };

    let prov: ProviderData = match serde_json::from_value(provider_data.clone()) {
        Ok(p) => p,
        Err(e) => {
            warn!(provider = spec.json_key, error = %e, "failed to parse provider data");
            return Vec::new();
        }
    };

    let mut entries: Vec<ModelEntry> = prov
        .models
        .into_values()
        .filter(|m| {
            if !m.tool_call {
                return false;
            }
            m.modalities
                .as_ref()
                .and_then(|mo| mo.output.as_ref())
                .is_none_or(|out| out.iter().any(|o| o == "text"))
        })
        .map(|m| {
            let family_str = m.family.as_deref().unwrap_or("");
            let cost = m.cost.as_ref();
            let output_price = cost.and_then(|c| c.output).unwrap_or(0.0);
            let limit = m.limit.as_ref();
            let tier = spec.infer_tier(family_str, output_price);

            ModelEntry {
                id: m.id.clone(),
                tier,
                default: spec.is_default(&m.id, tier),
                pricing: ModelPricing {
                    input: cost.and_then(|c| c.input).unwrap_or(0.0),
                    output: output_price,
                    cache_write: cost.and_then(|c| c.cache_write).unwrap_or(0.0),
                    cache_read: cost.and_then(|c| c.cache_read).unwrap_or(0.0),
                },
                max_output_tokens: limit.and_then(|l| l.output).unwrap_or(32_768),
                context_window: limit.and_then(|l| l.context).unwrap_or(200_000),
                supports_thinking: spec.supports_thinking && m.reasoning,
                supports_tool_examples: spec.supports_tool_examples,
                uses_responses_api: spec.uses_responses_api(&m.id),
            }
        })
        .collect();

    entries.sort_by(|a, b| b.id.len().cmp(&a.id.len()).then_with(|| a.id.cmp(&b.id)));

    ensure_defaults(&mut entries, spec);

    entries
}

fn ensure_defaults(entries: &mut [ModelEntry], spec: &ProviderSpec) {
    for &(tier, default_id) in spec.defaults {
        let has_default = entries.iter().any(|e| e.tier == tier && e.default);
        if has_default {
            continue;
        }
        if let Some(entry) = entries
            .iter_mut()
            .find(|e| e.id == default_id && e.tier == tier)
        {
            entry.default = true;
        } else if let Some(entry) = entries.iter_mut().find(|e| e.tier == tier) {
            warn!(
                provider = spec.json_key,
                tier = %tier,
                fallback_model = %entry.id,
                "preferred default not found, using first model in tier"
            );
            entry.default = true;
        }
    }
}

fn fallback_data() -> serde_json::Value {
    serde_json::from_str(FALLBACK_JSON).expect("embedded fallback JSON is invalid")
}

impl ModelRegistry {
    fn new(data: &serde_json::Value, etag: Option<String>) -> Self {
        let providers = REGISTRY_SPECS
            .iter()
            .map(|spec| {
                (
                    spec.json_key,
                    ArcSwap::from_pointee(parse_models(data, spec)),
                )
            })
            .collect();
        Self {
            providers,
            cached_etag: Mutex::new(etag),
        }
    }

    fn try_swap(&self, data: &serde_json::Value) -> bool {
        let parsed: Vec<_> = REGISTRY_SPECS
            .iter()
            .map(|spec| (spec.json_key, parse_models(data, spec)))
            .collect();

        if parsed.iter().any(|(_, entries)| entries.is_empty()) {
            warn!("refusing to swap registry: one or more providers parsed to zero models");
            return false;
        }

        for (key, entries) in parsed {
            if let Some(slot) = self.providers.get(key) {
                slot.store(Arc::new(entries));
            }
        }
        true
    }

    fn take_etag(&self) -> Option<String> {
        self.cached_etag.lock().ok()?.take()
    }

    fn set_etag(&self, etag: Option<String>) {
        if let Ok(mut guard) = self.cached_etag.lock() {
            *guard = etag;
        }
    }
}

fn get() -> &'static ModelRegistry {
    REGISTRY.get_or_init(|| {
        debug!("using embedded model registry fallback");
        ModelRegistry::new(&fallback_data(), None)
    })
}

pub fn init() {
    REGISTRY.get_or_init(|| {
        if let Some(cache) = load_cache() {
            debug!("loaded model registry from disk cache");
            return ModelRegistry::new(&cache.data, cache.etag);
        }

        debug!("using embedded model registry fallback");
        ModelRegistry::new(&fallback_data(), None)
    });
}

pub fn models(json_key: &str) -> Arc<Vec<ModelEntry>> {
    get()
        .providers
        .get(json_key)
        .map(|a| a.load_full())
        .unwrap_or_else(|| Arc::new(Vec::new()))
}

pub fn models_for(provider: ProviderKind) -> Option<Arc<Vec<ModelEntry>>> {
    let spec = spec_for_provider(provider)?;
    Some(models(spec.json_key))
}

pub async fn refresh() {
    let registry = get();
    let cached_etag = registry.take_etag();

    let result = smol::unblock(move || fetch_with_etag(cached_etag.as_deref())).await;

    match result {
        Ok(FetchResult::NotModified) => {
            debug!("model registry: 304 Not Modified");
        }
        Ok(FetchResult::Updated { etag, data }) => {
            if registry.try_swap(&data) {
                registry.set_etag(etag.clone());
                smol::unblock(move || save_cache(etag.as_deref(), &data)).await;
                debug!("model registry refreshed and cached");
            }
        }
        Err(e) => {
            warn!(error = %e, "model registry refresh failed, keeping existing data");
        }
    }
}

enum FetchResult {
    NotModified,
    Updated {
        etag: Option<String>,
        data: serde_json::Value,
    },
}

fn fetch_with_etag(
    cached_etag: Option<&str>,
) -> Result<FetchResult, Box<dyn std::error::Error + Send + Sync>> {
    use isahc::config::Configurable;
    use isahc::{ReadResponseExt, Request};

    let mut builder = Request::get(MODELS_DEV_URL)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30));

    if let Some(etag) = cached_etag {
        builder = builder.header("If-None-Match", etag);
    }

    let request = builder.body(())?;
    let mut response = isahc::send(request)?;

    if response.status() == 304 {
        return Ok(FetchResult::NotModified);
    }

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()).into());
    }

    let etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let body = response.text()?;
    let data: serde_json::Value = serde_json::from_str(&body)?;

    Ok(FetchResult::Updated { etag, data })
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn parse_fallback_json() {
        let data = fallback_data();
        for spec in REGISTRY_SPECS {
            let entries = parse_models(&data, spec);
            assert!(!entries.is_empty(), "no {} models parsed", spec.json_key);
        }
    }

    #[test]
    fn longest_prefix_wins_regardless_of_rule_order() {
        let spec = ProviderSpec {
            provider: ProviderKind::OpenAi,
            json_key: "test",
            tier_rules: &[
                TierRule {
                    family_prefix: "a",
                    tier: ModelTier::Weak,
                },
                TierRule {
                    family_prefix: "a-pro",
                    tier: ModelTier::Strong,
                },
            ],
            defaults: &[],
            has_responses_api: false,
            legacy_api_prefixes: &[],
            supports_thinking: false,
            supports_tool_examples: true,
        };
        assert_eq!(spec.infer_tier("a-pro-max", 0.0), ModelTier::Strong);
        assert_eq!(spec.infer_tier("a-mini", 0.0), ModelTier::Weak);
    }

    #[test_case(1.0,  ModelTier::Weak   ; "below_weak_threshold")]
    #[test_case(5.0,  ModelTier::Medium  ; "between_thresholds")]
    #[test_case(20.0, ModelTier::Strong  ; "above_medium_threshold")]
    fn unmatched_family_falls_back_to_price(price: f64, expected: ModelTier) {
        let spec = ProviderSpec {
            provider: ProviderKind::OpenAi,
            json_key: "test",
            tier_rules: &[],
            defaults: &[],
            has_responses_api: false,
            legacy_api_prefixes: &[],
            supports_thinking: false,
            supports_tool_examples: true,
        };
        assert_eq!(spec.infer_tier("unknown", price), expected);
    }

    #[test]
    fn uses_responses_api_respects_legacy_prefixes() {
        let spec = ProviderSpec {
            provider: ProviderKind::OpenAi,
            json_key: "test",
            tier_rules: &[],
            defaults: &[],
            has_responses_api: true,
            legacy_api_prefixes: &["old-v1"],
            supports_thinking: false,
            supports_tool_examples: true,
        };
        assert!(spec.uses_responses_api("new-model"));
        assert!(!spec.uses_responses_api("old-v1-turbo"));

        let no_resp = ProviderSpec {
            has_responses_api: false,
            ..spec
        };
        assert!(!no_resp.uses_responses_api("new-model"));
    }

    #[test_case(serde_json::json!({})                                     ; "empty")]
    #[test_case(serde_json::json!({ "anthropic": { "models": {} } })      ; "partial")]
    fn try_swap_rejects_incomplete_data(bad: serde_json::Value) {
        let registry = ModelRegistry::new(&fallback_data(), None);
        assert!(!registry.try_swap(&bad));
    }
}
