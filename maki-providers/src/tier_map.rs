//! Single source of truth for per-model tier assignments.
//!
//! Tier resolution combines three layers, in priority order:
//!
//! 1. **User overrides** — explicit tier assignments persisted to
//!    `~/.maki/model-tiers` (JSON). Apply to any provider.
//! 2. **Static entries** — `ModelEntry::tier` from the provider's built-in
//!    registry. Consulted by `model.rs` via [`TierMap::tier_for`].
//! 3. **Auto-assignment** — for providers that accept arbitrary models
//!    (e.g. Ollama), derived from the position in the ordered list returned
//!    by `list_models()`.
//!
//! All tier reads and writes go through [`tier_map`]. The module also owns
//! persistence: [`load_from_storage`] at startup, [`set_and_persist`] on user
//! edits. Callers never touch the on-disk format directly.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{OnceLock, RwLock};

use maki_storage::{DataDir, atomic_write};
use tracing::warn;

use crate::model::ModelTier;
use crate::provider::ProviderKind;

const TIERS_FILE: &str = "model-tiers";

static TIERS: OnceLock<RwLock<TierMap>> = OnceLock::new();

/// Access the global `TierMap`. Lazily initialises to an empty map on first
/// use, so the access order between [`load_from_storage`], `list_models()`,
/// and ordinary reads does not matter.
pub fn tier_map() -> &'static RwLock<TierMap> {
    TIERS.get_or_init(|| RwLock::new(TierMap::default()))
}

/// Load persisted overrides from `~/.maki/model-tiers` into the global map.
/// Replaces any previously-loaded overrides but preserves `known_models`.
pub fn load_from_storage(dir: &DataDir) {
    let overrides = read_overrides(dir.path().join(TIERS_FILE).as_path());
    tier_map().write().unwrap().set_overrides(overrides);
}

/// Atomically set an override and write the new state to disk.
/// Releases the write lock before touching the filesystem, so I/O latency
/// does not block other tier reads.
pub fn set_and_persist(spec: String, tier: ModelTier, dir: &DataDir) {
    let snapshot = {
        let mut map = tier_map().write().unwrap();
        map.set(spec, tier);
        map.overrides.clone()
    };
    write_overrides(dir.path().join(TIERS_FILE).as_path(), &snapshot);
}

#[derive(Debug, Default)]
pub struct TierMap {
    /// User-assigned tier overrides keyed by full spec (e.g. "ollama/qwen3:8b").
    /// Persisted to disk. `BTreeMap` for deterministic iteration and sorted
    /// on-disk output.
    overrides: BTreeMap<String, ModelTier>,
    /// Ordered model IDs per provider, populated from `list_models()`.
    /// Not persisted — rebuilt every session. Used for auto-assignment on
    /// providers that accept arbitrary models.
    known_models: HashMap<ProviderKind, Vec<String>>,
}

impl TierMap {
    /// Replace the overrides map wholesale. Preserves `known_models`.
    pub fn set_overrides(&mut self, overrides: BTreeMap<String, ModelTier>) {
        self.overrides = overrides;
    }

    pub fn set_known_models(&mut self, provider: ProviderKind, ids: Vec<String>) {
        self.known_models.insert(provider, ids);
    }

    pub fn set(&mut self, spec: String, tier: ModelTier) {
        self.overrides.insert(spec, tier);
    }

    /// Resolve the tier for a given model spec.
    ///
    /// Priority: user override → `static_tier` → auto-assigned by position in
    /// `known_models` → [`ModelTier::Medium`] as the safe default.
    pub fn tier_for(
        &self,
        spec: &str,
        provider: ProviderKind,
        static_tier: Option<ModelTier>,
    ) -> ModelTier {
        if let Some(&t) = self.overrides.get(spec) {
            return t;
        }
        if let Some(t) = static_tier {
            return t;
        }
        if let Some((_, model_id)) = spec.split_once('/')
            && let Some(models) = self.known_models.get(&provider)
            && let Some(pos) = models.iter().position(|id| id == model_id)
        {
            return tier_for_position(pos);
        }
        ModelTier::Medium
    }

    /// Find a model spec that matches the requested tier for a provider.
    ///
    /// Priority: first matching user override (sorted spec order) →
    /// auto-assigned model at the tier's slot in `known_models`, unless the
    /// user has overridden that exact spec to a different tier.
    pub fn spec_for_tier(&self, provider: ProviderKind, tier: ModelTier) -> Option<String> {
        let prefix = format!("{provider}/");
        for (spec, &t) in &self.overrides {
            if t == tier && spec.starts_with(&prefix) {
                return Some(spec.clone());
            }
        }

        let models = self.known_models.get(&provider).filter(|m| !m.is_empty())?;
        let want = match tier {
            ModelTier::Strong => 0,
            ModelTier::Medium => 1,
            ModelTier::Weak => 2,
        };
        let idx = want.min(models.len() - 1);
        let spec = format!("{provider}/{}", models[idx]);

        // Skip the auto-pick if the user explicitly assigned it to another tier.
        match self.overrides.get(&spec) {
            Some(&t) if t != tier => None,
            _ => Some(spec),
        }
    }
}

/// Pure function: map a list position to a tier. Position 0 is Strong, 1 is
/// Medium, the rest are Weak. The caller guarantees `pos < known_models.len()`.
fn tier_for_position(pos: usize) -> ModelTier {
    match pos {
        0 => ModelTier::Strong,
        1 => ModelTier::Medium,
        _ => ModelTier::Weak,
    }
}

// File format: pretty-printed JSON, `{ "provider/model": "tier", ... }`.
// `serde` handles escaping for unusual characters in specs; `BTreeMap`
// produces sorted, diff-friendly output.

fn read_overrides(path: &Path) -> BTreeMap<String, ModelTier> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    if raw.trim().is_empty() {
        return BTreeMap::new();
    }
    match serde_json::from_str::<BTreeMap<String, ModelTier>>(&raw) {
        Ok(map) => map,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to parse tier overrides, ignoring");
            BTreeMap::new()
        }
    }
}

fn write_overrides(path: &Path, overrides: &BTreeMap<String, ModelTier>) {
    let json = match serde_json::to_vec_pretty(overrides) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "failed to serialize tier overrides");
            return;
        }
    };
    if let Err(e) = atomic_write(path, &json) {
        warn!(path = %path.display(), error = %e, "failed to persist tier overrides");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_map(overrides: &[(&str, ModelTier)], models: &[&str]) -> TierMap {
        let mut map = TierMap::default();
        map.set_overrides(overrides.iter().map(|(s, t)| (s.to_string(), *t)).collect());
        if !models.is_empty() {
            map.set_known_models(
                ProviderKind::Ollama,
                models.iter().map(|s| s.to_string()).collect(),
            );
        }
        map
    }

    #[test]
    fn tier_for_priority_and_positions() {
        // Resolution order: user override > static_tier > auto-by-position > Medium default.
        let mut map = make_map(&[], &["pos0", "pos1", "pos2"]);
        map.set("ollama/pos0".into(), ModelTier::Weak);

        let t = |spec, static_tier| map.tier_for(spec, ProviderKind::Ollama, static_tier);

        assert_eq!(t("ollama/pos0", Some(ModelTier::Strong)), ModelTier::Weak);
        assert_eq!(t("ollama/pos1", Some(ModelTier::Weak)), ModelTier::Weak);
        assert_eq!(t("ollama/pos1", None), ModelTier::Medium);
        assert_eq!(t("ollama/pos2", None), ModelTier::Weak);
        assert_eq!(t("ollama/unknown", None), ModelTier::Medium);
    }

    #[test]
    fn tier_for_auto_assigns_position_zero_to_strong() {
        // Sibling test covers positions 1 and 2 but cannot cover pos 0 (overridden there).
        let map = make_map(&[], &["only"]);
        assert_eq!(
            map.tier_for("ollama/only", ProviderKind::Ollama, None),
            ModelTier::Strong
        );
    }

    #[test]
    fn spec_for_tier_override_first() {
        let map = make_map(
            &[("ollama/custom", ModelTier::Strong)],
            &["big", "mid", "small"],
        );
        assert_eq!(
            map.spec_for_tier(ProviderKind::Ollama, ModelTier::Strong),
            Some("ollama/custom".into())
        );
    }

    #[test]
    fn spec_for_tier_override_scan_is_deterministic() {
        // Two overrides at the same tier; BTreeMap sorted order → "a" wins.
        let map = make_map(
            &[
                ("ollama/b", ModelTier::Strong),
                ("ollama/a", ModelTier::Strong),
            ],
            &[],
        );
        assert_eq!(
            map.spec_for_tier(ProviderKind::Ollama, ModelTier::Strong),
            Some("ollama/a".into())
        );
    }

    #[test]
    fn spec_for_tier_override_scoped_by_provider() {
        let map = make_map(&[("openai/gpt-foo", ModelTier::Strong)], &[]);
        assert_eq!(
            map.spec_for_tier(ProviderKind::Ollama, ModelTier::Strong),
            None
        );
    }

    #[test]
    fn spec_for_tier_auto_fallback() {
        let map = make_map(&[], &["big", "mid", "small"]);
        assert_eq!(
            map.spec_for_tier(ProviderKind::Ollama, ModelTier::Strong),
            Some("ollama/big".into())
        );
        assert_eq!(
            map.spec_for_tier(ProviderKind::Ollama, ModelTier::Medium),
            Some("ollama/mid".into())
        );
        assert_eq!(
            map.spec_for_tier(ProviderKind::Ollama, ModelTier::Weak),
            Some("ollama/small".into())
        );
    }

    #[test]
    fn spec_for_tier_no_models_returns_none() {
        let map = make_map(&[], &[]);
        assert_eq!(
            map.spec_for_tier(ProviderKind::Ollama, ModelTier::Strong),
            None
        );
    }

    #[test]
    fn spec_for_tier_skips_model_overridden_elsewhere() {
        // "big" is at the Strong slot but overridden to Weak; don't return it for Strong.
        let map = make_map(&[("ollama/big", ModelTier::Weak)], &["big", "mid", "small"]);
        assert_eq!(
            map.spec_for_tier(ProviderKind::Ollama, ModelTier::Strong),
            None
        );
    }

    #[test]
    fn persistence_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(TIERS_FILE);

        assert!(read_overrides(&path).is_empty());

        let mut m = BTreeMap::new();
        m.insert("ollama/qwen3".into(), ModelTier::Strong);
        m.insert("ollama/qwen3:8b".into(), ModelTier::Medium);
        write_overrides(&path, &m);

        let loaded = read_overrides(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.get("ollama/qwen3"), Some(&ModelTier::Strong));
        assert_eq!(loaded.get("ollama/qwen3:8b"), Some(&ModelTier::Medium));
    }

    #[test]
    fn persistence_handles_missing_or_invalid_input() {
        let tmp = TempDir::new().unwrap();
        assert!(read_overrides(&tmp.path().join("does-not-exist")).is_empty());

        for bad in [
            b"".as_slice(),
            b"   \n".as_slice(),
            b"not json at all".as_slice(),
        ] {
            let path = tmp.path().join(TIERS_FILE);
            std::fs::write(&path, bad).unwrap();
            assert!(read_overrides(&path).is_empty());
        }
    }
}
