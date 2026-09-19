//! On-disk cache of discovered models, so startup and the model picker serve
//! the last discovery instantly instead of blocking on every provider's
//! /models endpoint. After a replay, live discovery still runs and rewrites
//! the cache in the background; an explicit refresh (R in the model picker)
//! skips the replay entirely.
//!
//! A replayed list is last run's answer, not this run's: it feeds the picker
//! and [`model_registry::set_cached_models`], never `known_models`, so
//! [`model_registry::discovery_complete`] stays false until a real probe
//! lands.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use maki_config::ModelPolicy;
use maki_storage::StateDir;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::manifest::ManifestRegistry;
use crate::model::{FastPricing, ModelInfo, ModelPricing};
use crate::model_registry;
use crate::provider::{ModelBatch, ProviderKind, fetch_all_models, provider_available};
use crate::providers::{custom, dynamic};

const CACHE_FILE: &str = "discovered-models.json";

/// Credential changes are caught by the fingerprint, so the age bound only has
/// to stop a long-abandoned file from seeding a picker with models a provider
/// has since retired. Live discovery corrects the list seconds later either
/// way, which is why this is generous rather than tight.
const MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Serialize, Deserialize)]
struct CachedPricing {
    input: f64,
    output: f64,
    cache_write: f64,
    cache_read: f64,
    fast: Option<(f64, f64)>,
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
    /// Digest of the credentials this list was discovered under, from
    /// [`fingerprint`]. One cache file serves every maki process on the
    /// machine, so without it a shell logged into account A replays its list
    /// into a shell logged into account B and the picker offers models that
    /// account cannot touch. Files written before this field existed
    /// deserialize to an empty string, which matches no live fingerprint and
    /// so is discarded like any other mismatch.
    #[serde(default)]
    fingerprint: String,
    /// Unix milliseconds, for [`MAX_AGE_MS`].
    #[serde(default)]
    written_at: u64,
    specs: Vec<String>,
    known: HashMap<String, Vec<CachedModel>>,
}

/// What a completed live discovery found, and whether to believe it when it
/// found nothing.
struct Discovery {
    specs: Vec<String>,
    /// A provider that cannot be reached warns and falls back to its static
    /// specs, so warnings are how discovery reports a failed probe. Without
    /// this, an empty run reads as "offline" even when the real cause is an
    /// expired token or a revoked key, and the stale list replays forever.
    degraded: bool,
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
                } = p;
                CachedPricing {
                    input: *input,
                    output: *output,
                    cache_write: *cache_write,
                    cache_read: *cache_read,
                    fast: fast.as_ref().map(|f| (f.input, f.output)),
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

/// Providers whose credentials resolve right now, sorted.
///
/// Catalog-backed slugs are deliberately absent: they are available only once
/// the models.dev catalog has warmed, so folding them in would change the
/// fingerprint between a cold start and the write at the end of discovery, and
/// the cache would miss every time.
fn resolved_providers() -> Vec<String> {
    let mut slugs: Vec<String> = ManifestRegistry::builtins()
        .iter()
        .map(|m| m.slug)
        .filter(|slug| provider_available(slug))
        .map(str::to_string)
        .collect();
    slugs.extend(dynamic::discovered_slugs().iter().map(|s| s.to_string()));
    slugs.extend(
        custom::declared_model_specs()
            .iter()
            .filter_map(|spec| spec.split_once('/'))
            .map(|(slug, _)| slug.to_string()),
    );
    slugs.sort();
    slugs.dedup();
    slugs
}

/// A stable per-account identifier for `slug`, or `None` when the provider
/// offers nothing stable to key on.
///
/// The value never reaches disk: [`fingerprint`] only ever feeds it to a
/// digest. OAuth logins carry an `account_id`, which is exactly the
/// non-secret identifier wanted here. A key-based provider has nothing else
/// that tells one account from another, so as a last resort the key itself is
/// hashed in - it is sensitive, which is why it is hashed and never stored,
/// and it is also what has to change for a key downgraded between runs to
/// invalidate the cache. Access and refresh tokens are skipped on purpose:
/// they rotate, and a fingerprint that changed on every token refresh would
/// never hit.
fn account_identity(dir: &StateDir, slug: &str) -> Option<String> {
    if let Some(id) = maki_storage::auth::load_tokens(dir, slug).and_then(|t| t.account_id) {
        return Some(id);
    }
    if let Some(creds) = maki_storage::auth::load_provider_credentials(dir, slug) {
        return Some(creds.api_key);
    }
    let env = ProviderKind::from_str(slug).ok()?.api_key_env();
    std::env::var(env).ok().filter(|key| !key.is_empty())
}

fn digest(entries: &[(String, Option<String>)]) -> String {
    let mut hasher = Sha256::new();
    for (slug, identity) in entries {
        // Length-prefixed so ("ab", "c") and ("a", "bc") cannot collide.
        for part in [Some(slug.as_str()), identity.as_deref()] {
            let part = part.unwrap_or_default();
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Identifies the credentials a discovery was taken under, so a cache written
/// by another account or by a since-downgraded key is treated as missing.
fn fingerprint() -> String {
    let dir = StateDir::resolve().ok();
    let entries: Vec<(String, Option<String>)> = resolved_providers()
        .into_iter()
        .map(|slug| {
            let identity = dir.as_ref().and_then(|d| account_identity(d, &slug));
            (slug, identity)
        })
        .collect();
    digest(&entries)
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

/// A cache is usable only when this run's credentials wrote it and it is not
/// past [`MAX_AGE_MS`]. Both are misses rather than errors: live discovery
/// rewrites the file moments later regardless.
fn load_usable(path: &Path, fingerprint: &str, now_ms: u64) -> Option<ModelsCache> {
    let cache = load_from(path)?;
    if cache.fingerprint != fingerprint {
        debug!("discovered-models cache belongs to other credentials; ignoring");
        return None;
    }
    if now_ms.saturating_sub(cache.written_at) > MAX_AGE_MS {
        debug!("discovered-models cache is past its maximum age; ignoring");
        return None;
    }
    Some(cache)
}

/// Rewrite the cache at `path` from a completed live discovery.
///
/// An empty result is worth recording when discovery itself worked: nothing is
/// configured or authenticated any more, and saying so is what stops
/// yesterday's list replaying at every start after a logout or a revoked key.
/// An empty result from a degraded run is a failed probe, so an existing good
/// cache survives it. `forced` (R in the picker) records either way, so a user
/// staring at a wrong list is never reduced to deleting the file by hand.
fn store_discovery(
    path: &Path,
    discovery: Discovery,
    fingerprint: String,
    now_ms: u64,
    forced: bool,
) {
    if discovery.specs.is_empty() && discovery.degraded && !forced {
        return;
    }
    let known: HashMap<String, Vec<CachedModel>> = model_registry::all_known_models()
        .iter()
        .map(|(slug, models)| (slug.clone(), models.iter().map(CachedModel::from).collect()))
        .collect();
    store_at(
        path,
        &ModelsCache {
            fingerprint,
            written_at: now_ms,
            specs: discovery.specs,
            known,
        },
    );
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
        model_registry::set_cached_models(&slug, models);
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

/// Replay the cache at `path` when one is usable, telling `on_done` that the
/// caller is now serviceable off cached data. Returns whether it replayed.
fn replay_cached(
    path: Option<&Path>,
    policy: &ModelPolicy,
    fingerprint: &str,
    now_ms: u64,
    on_ready: &mut impl FnMut(ModelBatch),
    on_done: Option<&(dyn Fn() + Send)>,
) -> bool {
    let Some(cache) = path.and_then(|p| load_usable(p, fingerprint, now_ms)) else {
        return false;
    };
    if !replay(cache, policy, on_ready) {
        return false;
    }
    if let Some(done) = on_done {
        done();
    }
    true
}

/// Record a completed live discovery and tell `on_done` again, now that the
/// registry holds this run's answer rather than last run's.
fn finish_discovery(
    path: Option<&Path>,
    discovery: Discovery,
    fingerprint: String,
    now_ms: u64,
    forced: bool,
    on_done: Option<&(dyn Fn() + Send)>,
) {
    if let Some(path) = path {
        store_discovery(path, discovery, fingerprint, now_ms, forced);
    }
    if let Some(done) = on_done {
        done();
    }
}

/// Like [`fetch_all_models`], but backed by an on-disk cache: unless
/// `refresh` forces a cold start, a previous run's discovery is replayed
/// first so callers become usable immediately. Live discovery then runs
/// either way, merging into the same `on_ready` and rewriting the cache.
///
/// `on_done` fires after the replay when there was one and again once live
/// discovery completes, hence `Fn`. Callers that snapshot registry metadata
/// (a `Model` reads it when built, not afterwards) would otherwise keep
/// yesterday's context window and pricing for the whole session even though
/// the registry was corrected seconds later.
pub async fn fetch_all_models_cached(
    policy: &ModelPolicy,
    mut on_ready: impl FnMut(ModelBatch),
    on_done: Option<Box<dyn Fn() + Send>>,
    refresh: bool,
) {
    let path = cache_path();
    let fingerprint = fingerprint();
    if !refresh {
        replay_cached(
            path.as_deref(),
            policy,
            &fingerprint,
            maki_storage::auth::now_millis(),
            &mut on_ready,
            on_done.as_deref(),
        );
    }

    let discovery = Arc::new(Mutex::new(Discovery {
        specs: Vec::new(),
        degraded: false,
    }));
    let done_discovery = Arc::clone(&discovery);
    let done_wrap: Box<dyn FnOnce() + Send> = Box::new(move || {
        let mut guard = done_discovery.lock().unwrap();
        let finished = Discovery {
            specs: std::mem::take(&mut guard.specs),
            degraded: guard.degraded,
        };
        drop(guard);
        finish_discovery(
            path.as_deref(),
            finished,
            fingerprint,
            maki_storage::auth::now_millis(),
            refresh,
            on_done.as_deref(),
        );
    });
    fetch_all_models(
        policy,
        |batch| {
            let mut guard = discovery.lock().unwrap();
            guard.degraded |= !batch.warnings.is_empty();
            guard.specs.extend(batch.models.iter().cloned());
            drop(guard);
            on_ready(batch);
        },
        Some(done_wrap),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::model::ModelTier;

    fn sample_info() -> ModelInfo {
        ModelInfo {
            id: "test-model".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
            pricing: Some(ModelPricing::per_million_with_fast(
                3.0, 15.0, 3.75, 0.3, 1.0, 5.0,
            )),
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
    }

    const FINGERPRINT: &str = "fingerprint-a";
    const NOW: u64 = 1_700_000_000_000;

    fn cache_of(specs: &[&str]) -> ModelsCache {
        ModelsCache {
            fingerprint: FINGERPRINT.to_string(),
            written_at: NOW,
            specs: specs.iter().map(|s| s.to_string()).collect(),
            known: HashMap::new(),
        }
    }

    fn counter() -> (Arc<AtomicUsize>, Box<dyn Fn() + Send>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let bumped = Arc::clone(&calls);
        (
            calls,
            Box::new(move || {
                bumped.fetch_add(1, Ordering::SeqCst);
            }),
        )
    }

    fn discovery(specs: &[&str], degraded: bool) -> Discovery {
        Discovery {
            specs: specs.iter().map(|s| s.to_string()).collect(),
            degraded,
        }
    }

    #[test]
    fn replay_applies_current_policy() {
        let cache = cache_of(&["kept/model-a", "banned/model-b", "kept/model-c"]);
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
    fn replay_does_not_complete_discovery() {
        // A slug of its own: the registry is global, and a neighbouring test
        // that really probed a provider must not answer for this one.
        const SLUG: &str = "models-cache-replay-probe";
        let mut cache = cache_of(&[&format!("{SLUG}/test-model")]);
        cache
            .known
            .insert(SLUG.to_string(), vec![CachedModel::from(&sample_info())]);

        assert!(replay(cache, &ModelPolicy::default(), &mut |_| {}));

        assert!(
            !model_registry::discovery_complete(SLUG),
            "a replay is last run's answer, not a probe of this one"
        );
        let restored = model_registry::discovered(SLUG, "test-model")
            .expect("replayed metadata still fills the picker");
        assert_eq!(restored.context_window, Some(200_000));
    }

    #[test]
    fn on_done_fires_after_replay_and_after_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        store_at(&path, &cache_of(&["cached/model"]));
        let (calls, done) = counter();

        assert!(replay_cached(
            Some(&path),
            &ModelPolicy::default(),
            FINGERPRINT,
            NOW,
            &mut |_| {},
            Some(&*done),
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "replay made us serviceable"
        );

        finish_discovery(
            Some(&path),
            discovery(&["live/model"], false),
            FINGERPRINT.to_string(),
            NOW,
            false,
            Some(&*done),
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "live discovery corrected the registry, so callers must re-resolve"
        );
    }

    #[test]
    fn degraded_empty_discovery_does_not_overwrite_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        store_at(&path, &cache_of(&["good/model"]));
        store_discovery(
            &path,
            discovery(&[], true),
            FINGERPRINT.to_string(),
            NOW,
            false,
        );
        let cache = load_from(&path).expect("good cache should survive");
        assert_eq!(cache.specs, vec!["good/model"]);
    }

    #[test]
    fn authenticated_empty_discovery_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        store_at(&path, &cache_of(&["logged-out/model"]));
        // Nothing warned, so nothing is configured any more: a logout, not a
        // dropped network.
        store_discovery(
            &path,
            discovery(&[], false),
            FINGERPRINT.to_string(),
            NOW,
            false,
        );
        assert!(load_from(&path).unwrap().specs.is_empty());
    }

    #[test]
    fn forced_empty_discovery_clears_a_bad_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        store_at(&path, &cache_of(&["wrong/model"]));
        store_discovery(
            &path,
            discovery(&[], true),
            FINGERPRINT.to_string(),
            NOW,
            true,
        );
        assert!(
            load_from(&path).unwrap().specs.is_empty(),
            "R must always be able to escape a bad cache"
        );
    }

    #[test]
    fn non_empty_discovery_rewrites_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        store_at(&path, &cache_of(&["old/model"]));
        store_discovery(
            &path,
            discovery(&["new/model"], false),
            FINGERPRINT.to_string(),
            NOW,
            false,
        );
        let cache = load_from(&path).unwrap();
        assert_eq!(cache.specs, vec!["new/model"]);
        assert_eq!(cache.fingerprint, FINGERPRINT);
        assert_eq!(cache.written_at, NOW);
    }

    #[test]
    fn foreign_fingerprint_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        store_at(&path, &cache_of(&["account-a/model"]));
        assert!(load_usable(&path, FINGERPRINT, NOW).is_some());
        assert!(
            load_usable(&path, "fingerprint-b", NOW).is_none(),
            "another account's list must not replay"
        );

        let (calls, done) = counter();
        let mut batches = Vec::new();
        assert!(!replay_cached(
            Some(&path),
            &ModelPolicy::default(),
            "fingerprint-b",
            NOW,
            &mut |b| batches.push(b),
            Some(&*done),
        ));
        assert!(batches.is_empty(), "a miss emits no models");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cache_past_max_age_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        store_at(&path, &cache_of(&["old/model"]));
        assert!(load_usable(&path, FINGERPRINT, NOW + MAX_AGE_MS).is_some());
        assert!(load_usable(&path, FINGERPRINT, NOW + MAX_AGE_MS + 1).is_none());
    }

    #[test]
    fn cache_without_fingerprint_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        std::fs::write(&path, br#"{"specs":["old/model"],"known":{}}"#).unwrap();
        assert!(load_from(&path).is_some(), "the old shape still parses");
        assert!(load_usable(&path, FINGERPRINT, NOW).is_none());
    }

    #[test]
    fn digest_separates_accounts_and_is_stable() {
        let a = vec![("openai".to_string(), Some("account-a".to_string()))];
        let b = vec![("openai".to_string(), Some("account-b".to_string()))];
        assert_eq!(digest(&a), digest(&a));
        assert_ne!(digest(&a), digest(&b));
        assert_ne!(digest(&a), digest(&[("openai".to_string(), None)]));
        assert!(
            !digest(&a).contains("account-a"),
            "an identifier must not be recoverable from the file"
        );
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
