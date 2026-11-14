//! Cross-provider capability lookup for models named by a gateway.
//!
//! Aperture exposes `<vendor>/<model>` ids that a user or an upstream named,
//! not Maki's slugs. A native provider already knows what its models can do,
//! so this module finds that row by name: curated tables first, then plugin
//! declarations, then the warm models.dev catalog, and finally a family guess
//! from the id. It returns where each answer came from so
//! `maki models --explain` can show its work and a user can pin a mismatch
//! with `like`.

use maki_config::providers::{ModelTier, ThinkingFields};

use crate::model::{ModelEntry, ModelFamily, ModelPricing, names_exactly};
use crate::providers::{catalog, plugin};
use crate::spec::{ProviderRegistry, ProviderSpec};

/// Everything a known model row can lend to a gateway model.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ModelCaps {
    pub(crate) family: Option<ModelFamily>,
    pub(crate) supports_thinking: Option<bool>,
    pub(crate) requires_thinking: bool,
    pub(crate) supports_vision: Option<bool>,
    pub(crate) supports_tool_examples: Option<bool>,
    pub(crate) supports_deferred_tools: Option<bool>,
    pub(crate) tier: Option<ModelTier>,
    pub(crate) context_window: Option<u32>,
    pub(crate) max_output_tokens: Option<u32>,
    pub(crate) pricing: Option<ModelPricing>,
    pub(crate) thinking_fields: Option<ThinkingFields>,
    pub(crate) source: Option<CapsSource>,
}

/// Where a [`ModelCaps`] came from. `model` is `None` for a provider-level
/// `like`, which borrows defaults rather than a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CapsSource {
    Curated {
        slug: &'static str,
        model: Option<String>,
    },
    Plugin {
        slug: String,
        model: Option<String>,
    },
    Catalog {
        slug: String,
        model: String,
    },
    Heuristic,
}

impl CapsSource {
    /// The spec whose thinking behavior this source describes, if it has one.
    pub(crate) fn spec(&self) -> Option<&'static ProviderSpec> {
        match self {
            Self::Curated { slug, .. } => ProviderRegistry::compiled(slug),
            Self::Plugin { slug, .. } => plugin::spec(slug),
            Self::Catalog { .. } | Self::Heuristic => None,
        }
    }

    pub(crate) fn model(&self) -> Option<&str> {
        match self {
            Self::Curated { model, .. } | Self::Plugin { model, .. } => model.as_deref(),
            Self::Catalog { model, .. } => Some(model),
            Self::Heuristic => None,
        }
    }
}

struct Candidate {
    exact: bool,
    prefix: usize,
    /// Lower wins on a tie: a curated row was checked against the vendor, a
    /// plugin row was written for its provider, the catalog is a third party.
    priority: u8,
    caps: ModelCaps,
}

/// Find capabilities for an id a gateway published. `None` when nothing in the
/// tables resembles it.
pub(crate) fn resolve(model_id: &str) -> Option<ModelCaps> {
    let mut candidates = Vec::new();
    for variant in variants(model_id) {
        collect_curated(variant, &mut candidates);
        collect_plugins(variant, &mut candidates);
        collect_catalog(variant, &mut candidates);
    }
    let mut caps = pick(candidates).unwrap_or_default();
    caps.family = caps.family.or_else(|| heuristic_family(model_id));
    if caps.source.is_none() && caps.family.is_some() {
        caps.source = Some(CapsSource::Heuristic);
    }
    (caps.source.is_some() || caps.family.is_some()).then_some(caps)
}

/// Capabilities from one spec's own table or declarations. Used when the
/// gateway names its route, which outranks a guess from the model name.
pub(crate) fn caps_for_spec(spec: &'static ProviderSpec, model_id: &str) -> Option<ModelCaps> {
    let compiled = ProviderRegistry::compiled(spec.slug).is_some();
    let entries = if compiled {
        spec.models().to_vec()
    } else {
        plugin::model_entries(spec.slug)
    };
    candidate_for(entries.as_slice(), model_id, 0, Some(spec.slug), |model| {
        if compiled {
            CapsSource::Curated {
                slug: spec.slug,
                model: Some(model),
            }
        } else {
            CapsSource::Plugin {
                slug: spec.slug.to_string(),
                model: Some(model),
            }
        }
    })
    .map(|candidate| candidate.caps)
}

/// Resolve an explicit `like` target: `slug/model` borrows that row, `slug`
/// borrows the provider's defaults.
pub(crate) fn resolve_target(target: &str) -> Option<ModelCaps> {
    let Some((slug, model_id)) = target.split_once('/') else {
        return provider_defaults(target);
    };
    if let Some(spec) = ProviderRegistry::compiled(slug)
        && let Some(caps) = caps_in(spec.models(), model_id, Some(spec.slug), |model| {
            CapsSource::Curated {
                slug: spec.slug,
                model: Some(model),
            }
        })
    {
        return Some(caps);
    }
    if let Some(spec) = plugin::spec(slug) {
        let entries = plugin::model_entries(spec.slug);
        if let Some(caps) = caps_in(&entries, model_id, Some(spec.slug), |model| {
            CapsSource::Plugin {
                slug: spec.slug.to_string(),
                model: Some(model),
            }
        }) {
            return Some(caps);
        }
    }
    catalog::model_meta_if_available(slug, model_id)
        .as_ref()
        .map(|meta| caps_from_catalog(slug, meta, model_id))
}

fn provider_defaults(slug: &str) -> Option<ModelCaps> {
    let (spec, source) = match ProviderRegistry::compiled(slug) {
        Some(spec) => (
            spec,
            CapsSource::Curated {
                slug: spec.slug,
                model: None,
            },
        ),
        None => {
            let spec = plugin::spec(slug)?;
            (
                spec,
                CapsSource::Plugin {
                    slug: spec.slug.to_string(),
                    model: None,
                },
            )
        }
    };
    Some(ModelCaps {
        family: Some(spec.family),
        source: Some(source),
        ..ModelCaps::default()
    })
}

fn collect_curated(variant: &str, candidates: &mut Vec<Candidate>) {
    for spec in ProviderRegistry::all_compiled() {
        if let Some(candidate) =
            candidate_for(spec.models(), variant, 0, Some(spec.slug), |model| {
                CapsSource::Curated {
                    slug: spec.slug,
                    model: Some(model),
                }
            })
        {
            candidates.push(candidate);
        }
    }
}

fn collect_plugins(variant: &str, candidates: &mut Vec<Candidate>) {
    for spec in plugin::specs() {
        let entries = plugin::model_entries(spec.slug);
        if let Some(candidate) = candidate_for(&entries, variant, 1, Some(spec.slug), |model| {
            CapsSource::Plugin {
                slug: spec.slug.to_string(),
                model: Some(model),
            }
        }) {
            candidates.push(candidate);
        }
    }
}

fn collect_catalog(variant: &str, candidates: &mut Vec<Candidate>) {
    for (slug, meta) in catalog::metas_by_model_if_available(variant) {
        candidates.push(Candidate {
            exact: true,
            prefix: variant.len(),
            priority: 2,
            caps: caps_from_catalog(&slug, &meta, variant),
        });
    }
}

fn candidate_for(
    entries: &[ModelEntry],
    model_id: &str,
    priority: u8,
    slug: Option<&str>,
    source: impl Fn(String) -> CapsSource,
) -> Option<Candidate> {
    let (entry, exact, prefix) = best(entries, model_id)?;
    let canonical = entry
        .prefixes
        .first()
        .map_or(model_id, String::as_str)
        .to_string();
    Some(Candidate {
        exact,
        prefix,
        priority,
        caps: caps_from_entry(entry, slug, model_id, source(canonical)),
    })
}

fn caps_in(
    entries: &[ModelEntry],
    model_id: &str,
    slug: Option<&str>,
    source: impl Fn(String) -> CapsSource,
) -> Option<ModelCaps> {
    candidate_for(entries, model_id, 0, slug, source).map(|candidate| candidate.caps)
}

/// The longest prefix of `model_id` an entry names, preferring an exact name.
fn best<'a>(entries: &'a [ModelEntry], model_id: &str) -> Option<(&'a ModelEntry, bool, usize)> {
    let id = model_id.to_ascii_lowercase();
    let mut best: Option<(&'a ModelEntry, bool, usize)> = None;
    for entry in entries {
        for prefix in &entry.prefixes {
            if !id.starts_with(&prefix.to_ascii_lowercase()) {
                continue;
            }
            let exact = prefix.eq_ignore_ascii_case(model_id) || names_exactly(entry, model_id);
            let replace = best.is_none_or(|(_, best_exact, best_len)| {
                (exact, prefix.len()) > (best_exact, best_len)
            });
            if replace {
                best = Some((entry, exact, prefix.len()));
            }
        }
    }
    best
}

/// One answer, or nothing when two sources of equal confidence disagree.
fn pick(mut candidates: Vec<Candidate>) -> Option<ModelCaps> {
    candidates.sort_by(|a, b| {
        b.exact
            .cmp(&a.exact)
            .then(b.prefix.cmp(&a.prefix))
            .then(a.priority.cmp(&b.priority))
    });
    let best = candidates.first()?;
    let best_priority = candidates
        .iter()
        .filter(|c| c.exact == best.exact && c.prefix == best.prefix)
        .map(|c| c.priority)
        .min()?;
    let mut same_rank = candidates.iter().filter(|c| {
        c.exact == best.exact && c.prefix == best.prefix && c.priority == best_priority
    });
    let first = same_rank.next()?;
    if same_rank.any(|c| !same_caps(&c.caps, &first.caps)) {
        return None;
    }
    Some(first.caps.clone())
}

fn same_caps(a: &ModelCaps, b: &ModelCaps) -> bool {
    a.family == b.family
        && a.supports_thinking == b.supports_thinking
        && a.requires_thinking == b.requires_thinking
        && a.supports_vision == b.supports_vision
        && a.supports_tool_examples == b.supports_tool_examples
        && a.supports_deferred_tools == b.supports_deferred_tools
        && a.tier == b.tier
        && a.context_window == b.context_window
        && a.max_output_tokens == b.max_output_tokens
        && a.pricing == b.pricing
        && a.thinking_fields == b.thinking_fields
}

fn caps_from_entry(
    entry: &ModelEntry,
    slug: Option<&str>,
    model_id: &str,
    source: CapsSource,
) -> ModelCaps {
    let spec = source.spec();
    let family = entry.family.or_else(|| spec.map(|spec| spec.family));
    let mut caps = ModelCaps {
        family,
        supports_thinking: entry
            .supports_thinking
            .or_else(|| spec.map(|spec| spec.supports_thinking)),
        requires_thinking: entry.requires_thinking,
        supports_vision: entry
            .supports_vision
            .or_else(|| family.map(ModelFamily::supports_vision)),
        supports_tool_examples: entry
            .supports_tool_examples
            .or_else(|| family.map(ModelFamily::supports_tool_examples)),
        supports_deferred_tools: entry.supports_deferred_tools,
        tier: Some(entry.tier),
        context_window: entry.context_window,
        max_output_tokens: entry.max_output_tokens,
        pricing: entry.pricing.clone(),
        thinking_fields: entry.thinking_fields.clone(),
        source: Some(source),
    };
    let canonical = entry.prefixes.first().map_or(model_id, String::as_str);
    if let Some(meta) = slug.and_then(|slug| catalog::model_meta_if_available(slug, canonical)) {
        caps.context_window = caps.context_window.or(meta.context);
        caps.max_output_tokens = caps.max_output_tokens.or(meta.output);
        caps.supports_thinking = caps.supports_thinking.or(meta.supports_thinking);
        caps.supports_vision = caps.supports_vision.or(meta.supports_vision);
        caps.pricing = caps.pricing.or(meta.pricing);
    }
    caps
}

fn caps_from_catalog(slug: &str, meta: &catalog::CatalogMeta, model: &str) -> ModelCaps {
    ModelCaps {
        supports_thinking: meta.supports_thinking,
        supports_vision: meta.supports_vision,
        context_window: meta.context,
        max_output_tokens: meta.output,
        pricing: meta.pricing.clone(),
        source: Some(CapsSource::Catalog {
            slug: slug.to_string(),
            model: model.to_string(),
        }),
        ..ModelCaps::default()
    }
}

/// The id and every suffix after a `.` or `/`, longest first. Gateways
/// namespace names (`us.openai.gpt-6-sol`, `anthropic.claude-opus-5`,
/// `deepseek-ai/DeepSeek-V4.1-Flash`) and the row is written without it.
fn variants(model_id: &str) -> Vec<&str> {
    let mut variants = vec![model_id];
    for (index, ch) in model_id.char_indices() {
        if ch == '.' || ch == '/' {
            variants.push(&model_id[index + ch.len_utf8()..]);
        }
    }
    variants.retain(|variant| !variant.is_empty());
    variants
}

fn heuristic_family(model_id: &str) -> Option<ModelFamily> {
    let id = model_id.to_ascii_lowercase();
    if id.contains("claude") {
        Some(ModelFamily::Claude)
    } else if id.contains("gemini") {
        Some(ModelFamily::Gemini)
    } else if id.contains("glm") {
        Some(ModelFamily::Glm)
    } else if id.contains("gpt")
        || id.starts_with("o1")
        || id.starts_with("o3")
        || id.starts_with("o4")
    {
        Some(ModelFamily::Gpt)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(prefixes: &[&str]) -> ModelEntry {
        ModelEntry {
            prefixes: prefixes.iter().map(|prefix| prefix.to_string()).collect(),
            ..ModelEntry::default()
        }
    }

    #[test]
    fn variants_drop_gateway_namespaces() {
        assert_eq!(
            variants("us.openai.gpt-6-sol"),
            ["us.openai.gpt-6-sol", "openai.gpt-6-sol", "gpt-6-sol"]
        );
        assert!(variants("deepseek-ai/DeepSeek-V4.1-Flash").contains(&"DeepSeek-V4.1-Flash"));
    }

    #[test]
    fn best_prefers_an_exact_name_then_the_longest_prefix() {
        let entries = [entry(&["glm-5", "glm"]), entry(&["glm-5.3"])];

        let (best, exact, len) = super::best(&entries, "glm-5.3-flash").unwrap();
        assert_eq!(best.prefixes[0], "glm-5.3");
        assert!(!exact);
        assert_eq!(len, "glm-5.3".len());

        let (best, exact, _) = super::best(&entries, "GLM-5.3").unwrap();
        assert_eq!(best.prefixes[0], "glm-5.3");
        assert!(exact);
    }

    #[test]
    fn equal_rank_disagreement_is_unknown() {
        let candidate = |family| Candidate {
            exact: true,
            prefix: 3,
            priority: 0,
            caps: ModelCaps {
                family: Some(family),
                ..ModelCaps::default()
            },
        };
        assert!(
            pick(vec![
                candidate(ModelFamily::Gpt),
                candidate(ModelFamily::Claude),
            ])
            .is_none()
        );
    }

    #[test]
    fn curated_tables_answer_an_alias_target() {
        let caps = resolve_target("openai/gpt-6-sol").unwrap();
        assert_eq!(caps.family, Some(ModelFamily::Gpt));
        assert_eq!(caps.context_window, Some(1_050_000));
        assert!(matches!(
            caps.source,
            Some(CapsSource::Curated { slug: "openai", .. })
        ));
    }

    #[test]
    fn a_bare_provider_alias_borrows_its_defaults() {
        let caps = resolve_target("zai").unwrap();
        assert_eq!(caps.family, Some(ModelFamily::Glm));
        assert_eq!(
            caps.supports_thinking, None,
            "the provider default may deny what a row allows"
        );
        assert!(matches!(
            caps.source,
            Some(CapsSource::Curated {
                slug: "zai",
                model: None
            })
        ));
        assert!(resolve_target("nobody").is_none());
    }

    #[test]
    fn gateway_namespaces_still_reach_their_row() {
        let caps = resolve("us.openai.gpt-6-sol").unwrap();
        assert_eq!(caps.family, Some(ModelFamily::Gpt));
        assert!(matches!(
            caps.source,
            Some(CapsSource::Curated { slug: "openai", .. })
        ));

        let caps = resolve("anthropic.claude-opus-5").unwrap();
        assert_eq!(caps.family, Some(ModelFamily::Claude));

        let caps = resolve("zai-org/GLM-5.3").unwrap();
        assert_eq!(caps.family, Some(ModelFamily::Glm));
    }

    #[test]
    fn a_stranger_gets_only_a_family_guess() {
        assert!(resolve("mystery-model").is_none());

        let caps = resolve("my-claude-fork").unwrap();
        assert_eq!(caps.family, Some(ModelFamily::Claude));
        assert_eq!(caps.supports_thinking, None);
        assert_eq!(caps.source, Some(CapsSource::Heuristic));
    }
}
