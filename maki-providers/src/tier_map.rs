//! Per-model tier assignments (strong / medium / weak).
//!
//! Three layers, checked in order: user overrides (persisted, one model per
//! tier) > static entries from the provider registry > auto-assignment by
//! position in `list_models()`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{OnceLock, RwLock};

use maki_storage::{StateDir, atomic_write};
use tracing::warn;

use crate::model::ModelTier;
use crate::provider::ProviderKind;

const TIERS_FILE: &str = "model-tiers";

static TIERS: OnceLock<RwLock<TierMap>> = OnceLock::new();

pub fn tier_map() -> &'static RwLock<TierMap> {
    TIERS.get_or_init(|| RwLock::new(TierMap::default()))
}

pub fn load_from_storage(dir: &StateDir) {
    let overrides = read_overrides(dir.path().join(TIERS_FILE).as_path());
    tier_map().write().unwrap().set_overrides(overrides);
}

pub fn set_and_persist(spec: String, tier: ModelTier, dir: &StateDir) {
    let snapshot = {
        let mut map = tier_map().write().unwrap();
        map.set(spec, tier);
        map.overrides.clone()
    };
    write_overrides(dir.path().join(TIERS_FILE).as_path(), &snapshot);
}

pub fn unset_and_persist(spec: &str, tier: ModelTier, dir: &StateDir) {
    let snapshot = {
        let mut map = tier_map().write().unwrap();
        map.unset(spec, tier);
        map.overrides.clone()
    };
    write_overrides(dir.path().join(TIERS_FILE).as_path(), &snapshot);
}

#[derive(Debug, Default)]
pub struct TierMap {
    /// Keyed by tier (not spec) so inserting a model automatically evicts the
    /// previous holder. Persisted to disk.
    overrides: BTreeMap<ModelTier, String>,
    /// Rebuilt every session from `list_models()`. Position 0 = strong, etc.
    known_models: HashMap<ProviderKind, Vec<String>>,
}

impl TierMap {
    pub fn set_overrides(&mut self, overrides: BTreeMap<ModelTier, String>) {
        self.overrides = overrides;
    }

    pub fn set_known_models(&mut self, provider: ProviderKind, ids: Vec<String>) {
        self.known_models.insert(provider, ids);
    }

    pub fn set(&mut self, spec: String, tier: ModelTier) {
        self.overrides.insert(tier, spec);
    }

    pub fn unset(&mut self, spec: &str, tier: ModelTier) {
        if self.overrides.get(&tier).map(String::as_str) == Some(spec) {
            self.overrides.remove(&tier);
        }
    }

    pub fn has_override(&self, spec: &str, tier: ModelTier) -> bool {
        self.overrides.get(&tier).map(String::as_str) == Some(spec)
    }

    pub fn tier_for(
        &self,
        spec: &str,
        provider: ProviderKind,
        static_tier: Option<ModelTier>,
    ) -> ModelTier {
        if let Some((&t, _)) = self.overrides.iter().find(|(_, s)| s.as_str() == spec) {
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

    pub fn spec_for_tier(&self, provider: ProviderKind, tier: ModelTier) -> Option<String> {
        let prefix = format!("{provider}/");
        if let Some(spec) = self.overrides.get(&tier)
            && spec.starts_with(&prefix)
        {
            return Some(spec.clone());
        }

        let models = self.known_models.get(&provider).filter(|m| !m.is_empty())?;
        let slot = match tier {
            ModelTier::Strong => 0,
            ModelTier::Medium => 1,
            ModelTier::Weak => 2,
        };
        let idx = slot.min(models.len() - 1);
        let spec = format!("{provider}/{}", models[idx]);

        let overridden_elsewhere = self.overrides.iter().any(|(&t, s)| s == &spec && t != tier);
        (!overridden_elsewhere).then_some(spec)
    }

    pub fn spec_for_tier_any(&self, tier: ModelTier) -> Option<String> {
        if let Some(spec) = self.overrides.get(&tier) {
            return Some(spec.clone());
        }
        for &provider in self.known_models.keys() {
            if let Some(spec) = self.spec_for_tier(provider, tier) {
                return Some(spec);
            }
        }
        None
    }

    pub fn override_tier_label(&self, spec: &str) -> Option<String> {
        let tiers: Vec<_> = self
            .overrides
            .iter()
            .rev()
            .filter(|(_, s)| s.as_str() == spec)
            .map(|(t, _)| t.to_string())
            .collect();
        (!tiers.is_empty()).then(|| tiers.join("/"))
    }
}

fn tier_for_position(pos: usize) -> ModelTier {
    [ModelTier::Strong, ModelTier::Medium, ModelTier::Weak][pos.min(2)]
}

fn read_overrides(path: &Path) -> BTreeMap<ModelTier, String> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    if raw.trim().is_empty() {
        return BTreeMap::new();
    }
    if let Ok(map) = serde_json::from_str::<BTreeMap<ModelTier, String>>(&raw) {
        return map;
    }
    // Legacy format: { "provider/model": "tier" } — invert on read.
    match serde_json::from_str::<BTreeMap<String, ModelTier>>(&raw) {
        Ok(legacy) => legacy.into_iter().map(|(s, t)| (t, s)).collect(),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to parse tier overrides, ignoring");
            BTreeMap::new()
        }
    }
}

fn write_overrides(path: &Path, overrides: &BTreeMap<ModelTier, String>) {
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

    fn make_map(overrides: &[(ModelTier, &str)], models: &[&str]) -> TierMap {
        let mut map = TierMap::default();
        map.set_overrides(overrides.iter().map(|(t, s)| (*t, s.to_string())).collect());
        if !models.is_empty() {
            map.set_known_models(
                ProviderKind::Ollama,
                models.iter().map(|s| s.to_string()).collect(),
            );
        }
        map
    }

    #[test]
    fn tier_for_resolution_priority() {
        let mut map = make_map(&[], &["pos0", "pos1", "pos2"]);
        map.set("ollama/pos1".into(), ModelTier::Weak);

        let t = |spec, static_tier| map.tier_for(spec, ProviderKind::Ollama, static_tier);

        assert_eq!(t("ollama/pos1", Some(ModelTier::Strong)), ModelTier::Weak);
        assert_eq!(t("ollama/pos0", Some(ModelTier::Weak)), ModelTier::Weak);
        assert_eq!(t("ollama/pos0", None), ModelTier::Strong);
        assert_eq!(t("ollama/pos1", None), ModelTier::Weak);
        assert_eq!(t("ollama/pos2", None), ModelTier::Weak);
        assert_eq!(t("ollama/unknown", None), ModelTier::Medium);
    }

    #[test]
    fn spec_for_tier_resolution() {
        let map = make_map(
            &[(ModelTier::Strong, "ollama/custom")],
            &["big", "mid", "small"],
        );
        let s = |t| map.spec_for_tier(ProviderKind::Ollama, t);

        assert_eq!(s(ModelTier::Strong), Some("ollama/custom".into()));
        assert_eq!(s(ModelTier::Medium), Some("ollama/mid".into()));
        assert_eq!(s(ModelTier::Weak), Some("ollama/small".into()));

        let scoped = make_map(&[(ModelTier::Strong, "openai/gpt-foo")], &[]);
        assert_eq!(
            scoped.spec_for_tier(ProviderKind::Ollama, ModelTier::Strong),
            None
        );

        let conflict = make_map(&[(ModelTier::Weak, "ollama/big")], &["big", "mid", "small"]);
        assert_eq!(
            conflict.spec_for_tier(ProviderKind::Ollama, ModelTier::Strong),
            None
        );
    }

    #[test]
    fn spec_for_tier_any_cross_provider() {
        let map = make_map(
            &[
                (ModelTier::Weak, "zai/glm-5"),
                (ModelTier::Strong, "openai/gpt-foo"),
            ],
            &["big", "mid", "small"],
        );
        assert_eq!(
            map.spec_for_tier_any(ModelTier::Strong),
            Some("openai/gpt-foo".into())
        );
        assert_eq!(
            map.spec_for_tier_any(ModelTier::Weak),
            Some("zai/glm-5".into())
        );
        assert_eq!(
            map.spec_for_tier_any(ModelTier::Medium),
            Some("ollama/mid".into())
        );
    }

    #[test]
    fn persistence_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(TIERS_FILE);

        assert!(read_overrides(&path).is_empty());

        let mut m = BTreeMap::new();
        m.insert(ModelTier::Strong, "ollama/qwen3".into());
        m.insert(ModelTier::Medium, "ollama/qwen3:8b".into());
        write_overrides(&path, &m);

        let loaded = read_overrides(&path);
        assert_eq!(loaded.get(&ModelTier::Strong).unwrap(), "ollama/qwen3");
        assert_eq!(loaded.get(&ModelTier::Medium).unwrap(), "ollama/qwen3:8b");
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

    #[test]
    fn unset_removes_matching_override() {
        let mut map = make_map(&[(ModelTier::Strong, "ollama/a")], &[]);
        map.unset("ollama/a", ModelTier::Strong);
        assert!(!map.has_override("ollama/a", ModelTier::Strong));
        assert!(map.overrides.is_empty());
    }

    #[test]
    fn unset_ignores_mismatched_spec() {
        let mut map = make_map(&[(ModelTier::Strong, "ollama/a")], &[]);
        map.unset("ollama/b", ModelTier::Strong);
        assert!(map.has_override("ollama/a", ModelTier::Strong));
    }

    #[test]
    fn unset_ignores_mismatched_tier() {
        let mut map = make_map(&[(ModelTier::Strong, "ollama/a")], &[]);
        map.unset("ollama/a", ModelTier::Weak);
        assert!(map.has_override("ollama/a", ModelTier::Strong));
    }

    #[test]
    fn has_override_returns_false_for_no_override() {
        let map = make_map(&[], &[]);
        assert!(!map.has_override("ollama/a", ModelTier::Strong));
    }

    #[test]
    fn backwards_compat_reads_legacy_format() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(TIERS_FILE);
        let legacy = r#"{"ollama/a": "strong", "ollama/b": "strong", "ollama/c": "weak"}"#;
        std::fs::write(&path, legacy).unwrap();

        let loaded = read_overrides(&path);
        assert_eq!(loaded.get(&ModelTier::Strong).unwrap(), "ollama/b");
        assert_eq!(loaded.get(&ModelTier::Weak).unwrap(), "ollama/c");
    }
}
