//! On-disk cache of discovered model specs, so the model picker shows the last
//! discovery instantly instead of waiting on every provider's /models
//! endpoint. Live discovery always runs right after, and each provider's live
//! answer replaces what the cache said about it.
//!
//! The cache only ever feeds the picker list. It never touches the model
//! registry, so pricing, context windows, tier picks and
//! [`crate::model_registry::discovery_complete`] only ever see this run's
//! probe.

use std::path::{Path, PathBuf};

use maki_config::ModelPolicy;
use maki_storage::StateDir;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::provider::{ModelBatch, fetch_all_models, provider_available};
use crate::providers::{custom, dynamic};
use crate::spec::ProviderRegistry;

const CACHE_FILE: &str = "discovered-models.json";

/// Credential changes are caught by the fingerprint, so the age bound only has
/// to stop a long-abandoned file from seeding a picker with models a provider
/// has since retired. Live discovery corrects the list seconds later either
/// way, which is why this is generous rather than tight.
const MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

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
}

/// What the model picker should show right now.
pub struct ModelList {
    pub specs: Vec<String>,
    /// Some provider has not answered yet, so the list may still grow.
    pub loading: bool,
}

/// One discovery run as the picker sees it: last run's specs, with each
/// provider's share swapped for its live answer as that lands.
#[derive(Default)]
struct Listing {
    replayed: Vec<String>,
    live: Vec<String>,
    /// A provider that cannot be reached warns and falls back to its static
    /// specs, so warnings are how discovery reports a failed probe. Without
    /// this, an empty run reads as "offline" even when the real cause is an
    /// expired token or a revoked key.
    degraded: bool,
}

fn provider_of(spec: &str) -> &str {
    spec.split_once('/').map_or(spec, |(provider, _)| provider)
}

impl Listing {
    /// Replaces rather than merges, so a model the provider dropped since
    /// last run leaves the picker instead of failing when picked.
    fn add_live(&mut self, batch: &ModelBatch) {
        self.degraded |= !batch.warnings.is_empty();
        self.replayed.retain(|old| {
            !batch
                .models
                .iter()
                .any(|new| provider_of(new) == provider_of(old))
        });
        for spec in &batch.models {
            if !self.live.contains(spec) {
                self.live.push(spec.clone());
            }
        }
    }

    fn snapshot(&self) -> ModelList {
        ModelList {
            specs: self.live.iter().chain(&self.replayed).cloned().collect(),
            loading: true,
        }
    }

    /// Whatever is still replayed belongs to a provider that never answered
    /// this run (logged out, removed from config), so it goes too.
    fn finish(mut self) -> ModelList {
        self.replayed.clear();
        ModelList {
            specs: self.live,
            loading: false,
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
    let mut slugs: Vec<String> = ProviderRegistry::builtins()
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
    let env = ProviderRegistry::get(slug)?.api_key_env;
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

/// Last run's specs when this run's credentials wrote them and they are not
/// past [`MAX_AGE_MS`], empty otherwise. A miss is not an error: live
/// discovery rewrites the file moments later regardless.
fn replay(path: &Path, policy: &ModelPolicy, fingerprint: &str, now_ms: u64) -> Vec<String> {
    let Some(cache) = load_from(path) else {
        return Vec::new();
    };
    if cache.fingerprint != fingerprint {
        debug!("discovered-models cache belongs to other credentials; ignoring");
        return Vec::new();
    }
    if now_ms.saturating_sub(cache.written_at) > MAX_AGE_MS {
        debug!("discovered-models cache is past its maximum age; ignoring");
        return Vec::new();
    }
    // The policy is applied on replay, not trusted from the cache file:
    // exclusions can change between runs.
    let mut specs = cache.specs;
    specs.retain(|spec| policy.allows(spec));
    specs
}

/// Rewrite the cache at `path` from a completed live discovery.
///
/// An empty result is worth recording when discovery itself worked: nothing is
/// configured or authenticated any more, and saying so is what stops
/// yesterday's list replaying at every start after a logout or a revoked key.
/// An empty result from a degraded run is a failed probe, so an existing good
/// cache survives it for the next start.
fn store_discovery(path: &Path, listing: &Listing, fingerprint: String, now_ms: u64) {
    if listing.live.is_empty() && listing.degraded {
        return;
    }
    store_at(
        path,
        &ModelsCache {
            fingerprint,
            written_at: now_ms,
            specs: listing.live.clone(),
        },
    );
}

/// Like [`fetch_all_models`], but the first `on_update` comes straight from
/// the on-disk cache. Every call carries the whole list to show, not a delta,
/// plus the warnings of the batch that changed it. The last call has
/// `loading` off and holds only this run's live answer.
pub async fn fetch_all_models_cached(
    policy: &ModelPolicy,
    mut on_update: impl FnMut(ModelList, Vec<String>),
) {
    let path = cache_path();
    let fingerprint = fingerprint();
    let mut listing = Listing {
        replayed: path
            .as_deref()
            .map(|p| replay(p, policy, &fingerprint, maki_storage::auth::now_millis()))
            .unwrap_or_default(),
        ..Listing::default()
    };
    on_update(listing.snapshot(), Vec::new());

    fetch_all_models(
        policy,
        |batch| {
            listing.add_live(&batch);
            on_update(listing.snapshot(), batch.warnings);
        },
        None,
    )
    .await;

    if let Some(path) = &path {
        store_discovery(
            path,
            &listing,
            fingerprint,
            maki_storage::auth::now_millis(),
        );
    }
    on_update(listing.finish(), Vec::new());
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const FINGERPRINT: &str = "fingerprint-a";
    const NOW: u64 = 1_700_000_000_000;

    fn specs(specs: &[&str]) -> Vec<String> {
        specs.iter().map(|s| s.to_string()).collect()
    }

    fn cache_of(cached: &[&str]) -> ModelsCache {
        ModelsCache {
            fingerprint: FINGERPRINT.to_string(),
            written_at: NOW,
            specs: specs(cached),
        }
    }

    fn cache_file(cached: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        store_at(&path, &cache_of(cached));
        (dir, path)
    }

    fn batch(models: &[&str], warnings: &[&str]) -> ModelBatch {
        ModelBatch {
            models: specs(models),
            warnings: specs(warnings),
        }
    }

    fn listing(live: &[&str], degraded: bool) -> Listing {
        Listing {
            live: specs(live),
            degraded,
            ..Listing::default()
        }
    }

    #[test]
    fn replay_applies_current_policy() {
        let (_dir, path) = cache_file(&["kept/model-a", "banned/model-b", "kept/model-c"]);
        let policy = ModelPolicy::new(&[], &["banned/*".to_string()]).unwrap();
        assert_eq!(
            replay(&path, &policy, FINGERPRINT, NOW),
            ["kept/model-a", "kept/model-c"]
        );
    }

    #[test_case("fingerprint-b", NOW                  ; "foreign_fingerprint")]
    #[test_case(FINGERPRINT,     NOW + MAX_AGE_MS + 1 ; "past_max_age")]
    fn replay_misses(fingerprint: &str, now_ms: u64) {
        let (_dir, path) = cache_file(&["old/model"]);
        let policy = ModelPolicy::default();
        assert_eq!(
            replay(&path, &policy, FINGERPRINT, NOW + MAX_AGE_MS),
            ["old/model"]
        );
        assert!(replay(&path, &policy, fingerprint, now_ms).is_empty());
    }

    #[test]
    fn cache_without_fingerprint_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        std::fs::write(&path, br#"{"specs":["old/model"],"known":{}}"#).unwrap();
        assert!(load_from(&path).is_some(), "the old shape still parses");
        assert!(replay(&path, &ModelPolicy::default(), FINGERPRINT, NOW).is_empty());
    }

    #[test]
    fn live_answer_replaces_that_providers_cached_specs() {
        let mut listing = Listing {
            replayed: specs(&["a/removed", "a/kept", "b/cached"]),
            ..Listing::default()
        };
        listing.add_live(&batch(&["a/kept", "a/new"], &[]));
        let shown = listing.snapshot();
        assert!(shown.loading);
        assert_eq!(shown.specs, ["a/kept", "a/new", "b/cached"]);

        let done = listing.finish();
        assert!(!done.loading);
        assert_eq!(
            done.specs,
            ["a/kept", "a/new"],
            "a provider that never answered live must not keep its cached specs"
        );
    }

    #[test_case(&[],            true,  &["good/model"] ; "degraded_empty_keeps_cache")]
    #[test_case(&[],            false, &[]             ; "authenticated_empty_is_recorded")]
    #[test_case(&["new/model"], false, &["new/model"]  ; "non_empty_rewrites_cache")]
    fn store_discovery_cases(live: &[&str], degraded: bool, expected: &[&str]) {
        let (_dir, path) = cache_file(&["good/model"]);
        store_discovery(
            &path,
            &listing(live, degraded),
            FINGERPRINT.to_string(),
            NOW,
        );
        let cache = load_from(&path).unwrap();
        assert_eq!(cache.specs, expected);
        assert_eq!(cache.fingerprint, FINGERPRINT);
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
