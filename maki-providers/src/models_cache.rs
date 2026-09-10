//! On-disk cache of discovered models, so startup and the model picker serve
//! the last discovery instantly instead of blocking on every provider's
//! /models endpoint. After a replay, live discovery still runs and rewrites
//! the cache in the background; an explicit refresh (R in the model picker)
//! skips the replay entirely.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use maki_config::ModelPolicy;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::model::{FastPricing, ModelInfo, ModelPricing};
use crate::model_registry;
use crate::provider::{ModelBatch, fetch_all_models};

const CACHE_FILE: &str = "discovered-models.json";

#[derive(Serialize, Deserialize)]
struct CachedPricing {
    input: f64,
    output: f64,
    cache_write: f64,
    cache_read: f64,
    fast: Option<(f64, f64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subsidised_by: Option<String>,
}

/// Mirror of [`ModelInfo`] without `provider_info`: that field is runtime
/// state (`Arc<dyn Any>`) and cannot be persisted; a live refresh restores it.
#[derive(Serialize, Deserialize)]
struct CachedModel {
    id: String,
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
    pricing: Option<CachedPricing>,
    supports_thinking: Option<bool>,
    supports_vision: Option<bool>,
    tier: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct ModelsCache {
    specs: Vec<String>,
    known: HashMap<String, Vec<CachedModel>>,
}

impl From<&ModelInfo> for CachedModel {
    fn from(m: &ModelInfo) -> Self {
        // Exhaustive destructure so a field added to ModelInfo upstream must
        // be either cached or explicitly ignored here — not silently dropped.
        let ModelInfo {
            id,
            context_window,
            max_output_tokens,
            pricing,
            supports_thinking,
            supports_vision,
            tier,
            provider_info: _,
        } = m;
        Self {
            id: id.clone(),
            context_window: *context_window,
            max_output_tokens: *max_output_tokens,
            pricing: pricing.as_ref().map(|p| {
                let ModelPricing {
                    input,
                    output,
                    cache_write,
                    cache_read,
                    fast,
                    subsidised_by,
                } = p;
                CachedPricing {
                    input: *input,
                    output: *output,
                    cache_write: *cache_write,
                    cache_read: *cache_read,
                    fast: fast.as_ref().map(|f| (f.input, f.output)),
                    subsidised_by: subsidised_by.as_deref().map(str::to_string),
                }
            }),
            supports_thinking: *supports_thinking,
            supports_vision: *supports_vision,
            tier: tier.map(|t| t.to_string()),
        }
    }
}

impl CachedModel {
    fn into_model_info(self) -> ModelInfo {
        ModelInfo {
            id: self.id,
            context_window: self.context_window,
            max_output_tokens: self.max_output_tokens,
            pricing: self.pricing.map(|p| ModelPricing {
                input: p.input,
                output: p.output,
                cache_write: p.cache_write,
                cache_read: p.cache_read,
                fast: p.fast.map(|(input, output)| FastPricing { input, output }),
                subsidised_by: p.subsidised_by.map(Arc::from),
            }),
            supports_thinking: self.supports_thinking,
            supports_vision: self.supports_vision,
            tier: self.tier.and_then(|t| match t.parse() {
                Ok(tier) => Some(tier),
                Err(_) => {
                    warn!(tier = %t, "unrecognised tier in models cache; dropped");
                    None
                }
            }),
            provider_info: None,
        }
    }
}

fn cache_path() -> Option<PathBuf> {
    maki_storage::paths::cache_dir()
        .ok()
        .map(|d| d.join(CACHE_FILE))
}

fn load_from(path: &Path) -> Option<ModelsCache> {
    let bytes = std::fs::read(path).ok()?;
    // A cache that fails to parse (corrupt, or written by an incompatible
    // version) is treated as absent: live discovery rewrites it.
    serde_json::from_slice(&bytes).ok()
}

fn store_at(path: &Path, cache: &ModelsCache) {
    match serde_json::to_vec(cache) {
        Ok(bytes) => {
            if let Err(e) = maki_storage::atomic_write(path, &bytes) {
                warn!(error = %e, "failed to write discovered-models cache");
            }
        }
        Err(e) => warn!(error = %e, "failed to serialize discovered-models cache"),
    }
}

/// Rewrite the cache at `path` from a completed live discovery. An empty
/// discovery (offline start) is not worth remembering: it would pin an empty
/// picker until the next refresh, so an existing good cache is left in place.
fn store_discovery(path: &Path, specs: Vec<String>) {
    if specs.is_empty() {
        return;
    }
    let known: HashMap<String, Vec<CachedModel>> = model_registry::all_known_models()
        .iter()
        .map(|(slug, models)| (slug.clone(), models.iter().map(CachedModel::from).collect()))
        .collect();
    store_at(path, &ModelsCache { specs, known });
}

/// Replay `cache` into the model registry and `on_ready`. Returns false for
/// an empty cache, leaving the registry untouched.
fn replay(cache: ModelsCache, policy: &ModelPolicy, on_ready: &mut impl FnMut(ModelBatch)) -> bool {
    if cache.specs.is_empty() {
        return false;
    }
    for (slug, models) in cache.known {
        let models = models
            .into_iter()
            .map(CachedModel::into_model_info)
            .collect();
        model_registry::set_known_models(&slug, models);
    }
    // The policy is applied on replay, not trusted from the cache file:
    // exclusions can change between runs.
    let mut models = cache.specs;
    models.retain(|spec| policy.allows(spec));
    on_ready(ModelBatch {
        models,
        warnings: Vec::new(),
    });
    true
}

/// Like [`fetch_all_models`], but backed by an on-disk cache: unless
/// `refresh` forces a cold start, a previous run's discovery is replayed
/// first so callers become usable immediately. Live discovery then runs
/// either way, merging into the same `on_ready` and rewriting the cache.
///
/// `on_done` fires exactly once: after the replay when there was one (the
/// caller is usable off cached data; the background refresh finishes
/// silently), otherwise after live discovery completes.
pub async fn fetch_all_models_cached(
    policy: &ModelPolicy,
    mut on_ready: impl FnMut(ModelBatch),
    on_done: Option<Box<dyn FnOnce() + Send>>,
    refresh: bool,
) {
    let mut on_done = on_done;
    if !refresh
        && let Some(cache) = cache_path().and_then(|p| load_from(&p))
        && replay(cache, policy, &mut on_ready)
        && let Some(done) = on_done.take()
    {
        done();
    }

    let specs = Arc::new(Mutex::new(Vec::new()));
    let done_specs = Arc::clone(&specs);
    let done_wrap: Box<dyn FnOnce() + Send> = Box::new(move || {
        let specs = std::mem::take(&mut *done_specs.lock().unwrap());
        if let Some(path) = cache_path() {
            store_discovery(&path, specs);
        }
        if let Some(done) = on_done {
            done();
        }
    });
    fetch_all_models(
        policy,
        |batch| {
            specs.lock().unwrap().extend(batch.models.iter().cloned());
            on_ready(batch);
        },
        Some(done_wrap),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelTier;

    fn sample_info() -> ModelInfo {
        ModelInfo {
            id: "test-model".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
            pricing: Some(ModelPricing {
                subsidised_by: Some(Arc::from("Max")),
                ..ModelPricing::per_token_with_fast(3.0, 15.0, 3.75, 0.3, 1.0, 5.0)
            }),
            supports_thinking: Some(true),
            supports_vision: Some(false),
            tier: Some(ModelTier::Strong),
            provider_info: None,
        }
    }

    #[test]
    fn cached_model_roundtrips_model_info() {
        let original = sample_info();
        let bytes = serde_json::to_vec(&CachedModel::from(&original)).unwrap();
        let cached: CachedModel = serde_json::from_slice(&bytes).unwrap();
        let restored = cached.into_model_info();
        assert_eq!(restored.id, original.id);
        assert_eq!(restored.context_window, original.context_window);
        assert_eq!(restored.max_output_tokens, original.max_output_tokens);
        assert_eq!(restored.supports_thinking, original.supports_thinking);
        assert_eq!(restored.supports_vision, original.supports_vision);
        assert_eq!(restored.tier, original.tier);
        let (r, o) = (restored.pricing.unwrap(), original.pricing.unwrap());
        assert_eq!(r.input, o.input);
        assert_eq!(r.output, o.output);
        assert_eq!(r.cache_write, o.cache_write);
        assert_eq!(r.cache_read, o.cache_read);
        let (rf, of) = (r.fast.unwrap(), o.fast.unwrap());
        assert_eq!((rf.input, rf.output), (of.input, of.output));
        assert_eq!(r.subsidised_by.as_deref(), Some("Max"));
    }

    #[test]
    fn replay_applies_current_policy() {
        let cache = ModelsCache {
            specs: vec![
                "kept/model-a".to_string(),
                "banned/model-b".to_string(),
                "kept/model-c".to_string(),
            ],
            known: HashMap::new(),
        };
        let policy = ModelPolicy::new(&[], &["banned/*".to_string()]).unwrap();
        let mut batches = Vec::new();
        assert!(replay(cache, &policy, &mut |b| batches.push(b)));
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].models, vec!["kept/model-a", "kept/model-c"]);
    }

    #[test]
    fn replay_empty_cache_reports_nothing() {
        let mut called = false;
        assert!(!replay(
            ModelsCache::default(),
            &ModelPolicy::default(),
            &mut |_| called = true,
        ));
        assert!(!called, "empty cache must not emit a batch");
    }

    #[test]
    fn empty_discovery_does_not_overwrite_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        store_at(
            &path,
            &ModelsCache {
                specs: vec!["good/model".to_string()],
                known: HashMap::new(),
            },
        );
        store_discovery(&path, Vec::new());
        let cache = load_from(&path).expect("good cache should survive");
        assert_eq!(cache.specs, vec!["good/model"]);
    }

    #[test]
    fn non_empty_discovery_rewrites_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        store_at(
            &path,
            &ModelsCache {
                specs: vec!["old/model".to_string()],
                known: HashMap::new(),
            },
        );
        store_discovery(&path, vec!["new/model".to_string()]);
        assert_eq!(load_from(&path).unwrap().specs, vec!["new/model"]);
    }

    #[test]
    fn corrupt_cache_loads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(load_from(&path).is_none());
        assert!(load_from(&dir.path().join("missing.json")).is_none());
    }
}
