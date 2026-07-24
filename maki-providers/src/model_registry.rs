//! Per-model tier assignments (strong / medium / weak).
//!
//! Three layers, checked in order: user overrides (persisted, one model per
//! tier) > static entries from the provider registry > auto-assignment by
//! position in `list_models()`.
//!
//! Discovered metadata (context windows, pricing) from `/models` endpoints is
//! stored in `known_models` and consulted by [`crate::model::Model::from_base`].
//!
//! All reads and writes go through [`model_registry`]. The module owns
//! persistence: [`load_from_storage`] at startup, [`set_and_persist`] on user
//! edits. Callers never touch the on-disk format directly.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

use maki_storage::{StateDir, atomic_write};
use tracing::warn;

use crate::model::{ModelInfo, ModelTier};

const TIERS_FILE: &str = "model-tiers";

static REGISTRY: OnceLock<RwLock<ModelRegistry>> = OnceLock::new();

pub fn model_registry() -> &'static RwLock<ModelRegistry> {
    REGISTRY.get_or_init(|| RwLock::new(ModelRegistry::default()))
}

pub fn load_from_storage(dir: &StateDir) {
    let overrides = read_overrides(dir.path().join(TIERS_FILE).as_path());
    model_registry().write().unwrap().set_overrides(overrides);
}

pub fn set_and_persist(spec: String, tier: ModelTier, dir: &StateDir) {
    let snapshot = {
        let mut reg = model_registry().write().unwrap();
        reg.set(spec, tier);
        reg.overrides.clone()
    };
    write_overrides(dir.path().join(TIERS_FILE).as_path(), &snapshot);
}

pub fn unset_and_persist(spec: &str, tier: ModelTier, dir: &StateDir) {
    let snapshot = {
        let mut reg = model_registry().write().unwrap();
        reg.unset(spec, tier);
        reg.overrides.clone()
    };
    write_overrides(dir.path().join(TIERS_FILE).as_path(), &snapshot);
}

#[derive(Debug, Default)]
pub struct ModelRegistry {
    /// Keyed by tier (not spec) so inserting a model automatically evicts the
    /// previous holder. Persisted to disk.
    overrides: BTreeMap<ModelTier, String>,
    /// Ordered model info per provider, populated from `list_models()`.
    /// Not persisted - rebuilt every session. Used for auto-tier assignment
    /// and discovered metadata lookup.
    known_models: HashMap<Arc<str>, Vec<ModelInfo>>,
}

impl ModelRegistry {
    pub fn set_overrides(&mut self, overrides: BTreeMap<ModelTier, String>) {
        self.overrides = overrides;
    }

    pub fn set_known_models(&mut self, provider: &Arc<str>, models: Vec<ModelInfo>) {
        self.known_models.insert(Arc::clone(provider), models);
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

    /// Lookup discovered metadata for a model by ID.
    pub fn discovered(&self, provider: &str, model_id: &str) -> Option<&ModelInfo> {
        self.known_models
            .get(provider)?
            .iter()
            .find(|m| m.id == model_id)
    }

    pub fn tier_for(
        &self,
        spec: &str,
        provider: &str,
        static_tier: Option<ModelTier>,
    ) -> ModelTier {
        // A spec may hold several tiers; prefer the strongest agent tier,
        // falling back to Compaction only when it is the sole assignment.
        let mut tiers = self
            .overrides
            .iter()
            .rev()
            .filter(|(_, s)| s.as_str() == spec)
            .map(|(&t, _)| t);
        if let Some(first) = tiers.next() {
            return match first {
                ModelTier::Compaction => tiers.next().unwrap_or(first),
                t => t,
            };
        }
        if let Some((_, model_id)) = spec.split_once('/')
            && let Some(models) = self.known_models.get(provider)
            && let Some(model) = models.iter().find(|model| model.id == model_id)
        {
            if let Some(tier) = model.tier {
                return tier;
            }
            if static_tier.is_none()
                && let Some(pos) = models.iter().position(|candidate| candidate.id == model_id)
            {
                return tier_for_position(pos);
            }
        }
        if let Some(t) = static_tier {
            return t;
        }
        ModelTier::Medium
    }

    pub fn spec_for_tier(&self, provider: &str, tier: ModelTier) -> Option<String> {
        let prefix = format!("{provider}/");
        if let Some(spec) = self.overrides.get(&tier)
            && spec.starts_with(&prefix)
        {
            return Some(spec.clone());
        }

        let candidate = self
            .discovered_static_candidate(provider, tier)
            .or_else(|| self.metadata_candidate(provider, tier))
            .or_else(|| static_candidate(provider, tier))
            .or_else(|| self.positional_candidate(provider, tier))?;

        (!self.claimed_elsewhere(&candidate, tier)).then_some(candidate)
    }

    /// The curated default the provider actually offers, so discovery cannot
    /// replace a still available default with whatever it lists first.
    fn discovered_static_candidate(&self, provider: &str, tier: ModelTier) -> Option<String> {
        static_prefixes(provider, tier)
            .find(|prefix| self.discovered(provider, prefix).is_some())
            .map(|prefix| format!("{provider}/{prefix}"))
    }

    /// Lowest ID wins, so the tier default survives provider list reordering.
    fn metadata_candidate(&self, provider: &str, tier: ModelTier) -> Option<String> {
        self.known_models
            .get(provider)?
            .iter()
            .filter(|model| model.tier == Some(tier))
            .map(|model| model.id.as_str())
            .min()
            .map(|id| format!("{provider}/{id}"))
    }

    fn positional_candidate(&self, provider: &str, tier: ModelTier) -> Option<String> {
        let models = self.known_models.get(provider).filter(|m| !m.is_empty())?;
        let slot = match tier {
            ModelTier::Strong => 0,
            ModelTier::Medium => 1,
            ModelTier::Weak => 2,
            ModelTier::Compaction => return None,
        };
        Some(format!(
            "{provider}/{}",
            models[slot.min(models.len() - 1)].id
        ))
    }

    fn claimed_elsewhere(&self, spec: &str, tier: ModelTier) -> bool {
        self.overrides.iter().any(|(&t, s)| s == spec && t != tier)
    }

    pub fn spec_for_tier_any(&self, tier: ModelTier) -> Option<String> {
        if let Some(spec) = self.overrides.get(&tier) {
            return Some(spec.clone());
        }
        for provider in self.known_models.keys() {
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

fn static_candidate(provider: &str, tier: ModelTier) -> Option<String> {
    static_prefixes(provider, tier)
        .next()
        .map(|prefix| format!("{provider}/{prefix}"))
}

fn static_prefixes(provider: &str, tier: ModelTier) -> impl Iterator<Item = &'static str> {
    crate::manifest::ManifestRegistry::get(provider)
        .into_iter()
        .flat_map(|manifest| manifest.models)
        .filter(move |entry| entry.default && entry.tier == tier)
        .flat_map(|entry| entry.prefixes.iter().copied())
}

fn tier_for_position(pos: usize) -> ModelTier {
    [ModelTier::Strong, ModelTier::Medium, ModelTier::Weak][pos.min(2)]
}

// On-disk format: { "tier": "spec", ... } keyed by tier, matching the in-memory
// `BTreeMap<ModelTier, String>`. Tier-keyed storage preserves a model assigned
// to multiple tiers; a spec-keyed file would collapse them to a single entry.
// Legacy files were spec-keyed and are inverted on read.

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
    use test_case::test_case;

    fn make_map(overrides: &[(ModelTier, &str)], models: &[&str]) -> ModelRegistry {
        let mut reg = ModelRegistry::default();
        reg.set_overrides(overrides.iter().map(|(t, s)| (*t, s.to_string())).collect());
        if !models.is_empty() {
            reg.set_known_models(
                &Arc::<str>::from("ollama"),
                models
                    .iter()
                    .map(|s| ModelInfo::id_only(s.to_string()))
                    .collect(),
            );
        }
        reg
    }

    #[test]
    fn tier_for_resolution_priority() {
        let mut reg = make_map(&[], &["pos0", "pos1", "pos2"]);
        reg.set("ollama/pos1".into(), ModelTier::Weak);

        let t = |spec, static_tier| reg.tier_for(spec, "ollama", static_tier);

        assert_eq!(t("ollama/pos1", Some(ModelTier::Strong)), ModelTier::Weak);
        assert_eq!(t("ollama/pos0", Some(ModelTier::Weak)), ModelTier::Weak);
        assert_eq!(t("ollama/pos0", None), ModelTier::Strong);
        assert_eq!(t("ollama/pos1", None), ModelTier::Weak);
        assert_eq!(t("ollama/pos2", None), ModelTier::Weak);
        assert_eq!(t("ollama/unknown", None), ModelTier::Medium);
    }

    fn make_tiered(models: &[(&str, ModelTier)]) -> ModelRegistry {
        let mut reg = ModelRegistry::default();
        reg.set_known_models(
            &Arc::<str>::from("copilot"),
            models
                .iter()
                .map(|&(id, tier)| ModelInfo {
                    tier: Some(tier),
                    ..ModelInfo::id_only(id.into())
                })
                .collect(),
        );
        reg
    }

    #[test]
    fn discovered_category_tier_beats_position_and_static_fallback() {
        let reg = make_tiered(&[
            ("terra", ModelTier::Medium),
            ("luna", ModelTier::Weak),
            ("gpt-5.6-sol", ModelTier::Strong),
        ]);

        assert_eq!(
            reg.tier_for("copilot/gpt-5.6-sol", "copilot", Some(ModelTier::Medium)),
            ModelTier::Strong
        );
        assert_eq!(
            reg.tier_for("copilot/terra", "copilot", None),
            ModelTier::Medium
        );
        assert_eq!(
            reg.tier_for("copilot/luna", "copilot", None),
            ModelTier::Weak
        );
        assert_eq!(
            reg.spec_for_tier("copilot", ModelTier::Strong),
            Some("copilot/gpt-5.6-sol".into())
        );
    }

    #[test_case(&[("gpt-5.4", ModelTier::Strong), ("claude-opus-4.7", ModelTier::Strong)], "copilot/gpt-5.4"; "curated default beats discovered tier")]
    #[test_case(&[("claude-opus-4.6", ModelTier::Strong), ("alpha", ModelTier::Strong)], "copilot/claude-opus-4.6"; "later curated prefix when first is unavailable")]
    #[test_case(&[("zeta", ModelTier::Strong), ("alpha", ModelTier::Strong)], "copilot/alpha"; "lowest id when no curated default is entitled")]
    fn spec_for_tier_prefers_entitled_curated_default(
        models: &[(&str, ModelTier)],
        expected: &str,
    ) {
        let reg = make_tiered(models);
        assert_eq!(
            reg.spec_for_tier("copilot", ModelTier::Strong),
            Some(expected.into())
        );
    }

    #[test]
    fn spec_for_tier_ignores_discovery_list_order() {
        let models = [
            ("zeta", ModelTier::Strong),
            ("alpha", ModelTier::Strong),
            ("mid", ModelTier::Medium),
        ];
        let mut reversed = models;
        reversed.reverse();

        assert_eq!(
            make_tiered(&models).spec_for_tier("copilot", ModelTier::Strong),
            make_tiered(&reversed).spec_for_tier("copilot", ModelTier::Strong)
        );
    }

    #[test]
    fn tier_for_prefers_strongest_over_multi_tier_spec() {
        let mut reg = make_map(&[], &[]);
        reg.set("ollama/multi".into(), ModelTier::Medium);
        reg.set("ollama/multi".into(), ModelTier::Strong);
        reg.set("ollama/multi".into(), ModelTier::Compaction);
        reg.set("ollama/compact-only".into(), ModelTier::Compaction);

        let t = |spec| reg.tier_for(spec, "ollama", None);

        assert_eq!(t("ollama/multi"), ModelTier::Strong);
        assert_eq!(t("ollama/compact-only"), ModelTier::Compaction);
    }

    #[test]
    fn spec_for_tier_resolution() {
        let reg = make_map(
            &[(ModelTier::Strong, "ollama/custom")],
            &["big", "mid", "small"],
        );
        let s = |t| reg.spec_for_tier("ollama", t);

        assert_eq!(s(ModelTier::Strong), Some("ollama/custom".into()));
        assert_eq!(s(ModelTier::Medium), Some("ollama/mid".into()));
        assert_eq!(s(ModelTier::Weak), Some("ollama/small".into()));

        let scoped = make_map(&[(ModelTier::Strong, "openai/gpt-foo")], &[]);
        assert_eq!(scoped.spec_for_tier("ollama", ModelTier::Strong), None);

        let conflict = make_map(&[(ModelTier::Weak, "ollama/big")], &["big", "mid", "small"]);
        assert_eq!(conflict.spec_for_tier("ollama", ModelTier::Strong), None);
    }

    #[test]
    fn spec_for_tier_any_cross_provider() {
        let reg = make_map(
            &[
                (ModelTier::Weak, "zai/glm-5"),
                (ModelTier::Strong, "openai/gpt-foo"),
            ],
            &["big", "mid", "small"],
        );
        assert_eq!(
            reg.spec_for_tier_any(ModelTier::Strong),
            Some("openai/gpt-foo".into())
        );
        assert_eq!(
            reg.spec_for_tier_any(ModelTier::Weak),
            Some("zai/glm-5".into())
        );
        assert_eq!(
            reg.spec_for_tier_any(ModelTier::Medium),
            Some("ollama/mid".into())
        );
    }

    #[test]
    fn discovered_looks_up_by_id() {
        let mut reg = ModelRegistry::default();
        reg.set_known_models(
            &Arc::<str>::from("llama-cpp"),
            vec![
                ModelInfo {
                    id: "model-a".into(),
                    context_window: Some(32_000),
                    max_output_tokens: None,
                    pricing: None,
                    supports_thinking: None,
                    supports_vision: None,
                    tier: None,
                    provider_info: None,
                },
                ModelInfo {
                    id: "model-b".into(),
                    context_window: Some(128_000),
                    max_output_tokens: None,
                    pricing: None,
                    supports_thinking: None,
                    supports_vision: None,
                    tier: None,
                    provider_info: None,
                },
            ],
        );
        let info = reg.discovered("llama-cpp", "model-a").unwrap();
        assert_eq!(info.id, "model-a");
        assert_eq!(info.context_window, Some(32_000));
        assert!(reg.discovered("llama-cpp", "model-x").is_none());
        assert!(reg.discovered("ollama", "model-a").is_none());
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
        let mut reg = make_map(&[(ModelTier::Strong, "ollama/a")], &[]);
        reg.unset("ollama/a", ModelTier::Strong);
        assert!(!reg.has_override("ollama/a", ModelTier::Strong));
        assert!(reg.overrides.is_empty());
    }

    #[test]
    fn unset_ignores_mismatched_spec() {
        let mut reg = make_map(&[(ModelTier::Strong, "ollama/a")], &[]);
        reg.unset("ollama/b", ModelTier::Strong);
        assert!(reg.has_override("ollama/a", ModelTier::Strong));
    }

    #[test]
    fn unset_ignores_mismatched_tier() {
        let mut reg = make_map(&[(ModelTier::Strong, "ollama/a")], &[]);
        reg.unset("ollama/a", ModelTier::Weak);
        assert!(reg.has_override("ollama/a", ModelTier::Strong));
    }

    #[test]
    fn has_override_returns_false_for_no_override() {
        let reg = make_map(&[], &[]);
        assert!(!reg.has_override("ollama/a", ModelTier::Strong));
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

    #[test]
    fn write_then_read_preserves_multi_tier_assignment() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(TIERS_FILE);

        let mut m = BTreeMap::new();
        m.insert(ModelTier::Strong, "ollama/qwen3".into());
        m.insert(ModelTier::Medium, "ollama/qwen3".into());
        m.insert(ModelTier::Weak, "ollama/qwen3:8b".into());
        write_overrides(&path, &m);

        let loaded = read_overrides(&path);
        assert_eq!(loaded.get(&ModelTier::Strong).unwrap(), "ollama/qwen3");
        assert_eq!(loaded.get(&ModelTier::Medium).unwrap(), "ollama/qwen3");
        assert_eq!(loaded.get(&ModelTier::Weak).unwrap(), "ollama/qwen3:8b");
    }
}
