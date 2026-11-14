//! Capability lookup for models a gateway named after their upstream.
//!
//! A gateway model id comes from the upstream, not from Maki's slug. Two
//! explicit things can name the row: the gateway's provider id (when Maki
//! knows that provider) and a `like` override. When neither does, a unique
//! exact id among the shipped declarations is used. Anything else stays
//! unknown, because a prefix match or a family guess would borrow a
//! neighbour's capabilities and claim things the model cannot do.

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
}

impl CapsSource {
    /// The spec whose thinking behavior this source describes, if it has one.
    pub(crate) fn spec(&self) -> Option<&'static ProviderSpec> {
        match self {
            Self::Curated { slug, .. } => ProviderRegistry::compiled(slug),
            Self::Plugin { slug, .. } => plugin::spec(slug),
            Self::Catalog { .. } => None,
        }
    }

    pub(crate) fn model(&self) -> Option<&str> {
        match self {
            Self::Curated { model, .. } | Self::Plugin { model, .. } => model.as_deref(),
            Self::Catalog { model, .. } => Some(model),
        }
    }
}

/// Capabilities from one spec's own table or declarations. Used when the
/// gateway names the provider, which outranks matching a model name.
pub(crate) fn caps_for_spec(spec: &'static ProviderSpec, model_id: &str) -> Option<ModelCaps> {
    if ProviderRegistry::compiled(spec.slug).is_some() {
        caps_in(spec.models(), model_id, Some(spec.slug), |model| {
            CapsSource::Curated {
                slug: spec.slug,
                model: Some(model),
            }
        })
    } else {
        let entries = plugin::model_entries(spec.slug);
        caps_in(&entries, model_id, Some(spec.slug), |model| {
            CapsSource::Plugin {
                slug: spec.slug.to_string(),
                model: Some(model),
            }
        })
    }
}

/// Resolve an explicit `like` target: `slug/model` borrows that row, `slug`
/// borrows the provider's family and fallback limits.
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
        context_window: Some(spec.fallback_context_window),
        max_output_tokens: spec.fallback_max_output,
        source: Some(source),
        ..ModelCaps::default()
    })
}

/// A unique exact id among the shipped declarations. No prefix, no
/// models.dev scan and no family guess: when two rows disagree the name is
/// ambiguous and the caller has to pin it with `like`.
pub(crate) fn unique_exact(model_id: &str) -> Option<ModelCaps> {
    let mut found = Vec::new();
    let mut plugin_specs = plugin::specs();
    plugin_specs.sort_by_key(|spec| spec.slug);
    for variant in variants(model_id) {
        for spec in ProviderRegistry::all_compiled() {
            if let Some(entry) = exact_in(spec.models(), variant) {
                found.push(caps_from_entry(
                    entry,
                    Some(spec.slug),
                    variant,
                    CapsSource::Curated {
                        slug: spec.slug,
                        model: Some(canonical(entry, variant)),
                    },
                ));
            }
        }
        for &spec in &plugin_specs {
            let entries = plugin::model_entries(spec.slug);
            if let Some(entry) = exact_in(&entries, variant) {
                found.push(caps_from_entry(
                    entry,
                    Some(spec.slug),
                    variant,
                    CapsSource::Plugin {
                        slug: spec.slug.to_string(),
                        model: Some(canonical(entry, variant)),
                    },
                ));
            }
        }
    }
    let first = found.first()?;
    if found.iter().skip(1).any(|caps| !same_caps(caps, first)) {
        return None;
    }
    Some(first.clone())
}

fn exact_in<'a>(entries: &'a [ModelEntry], model_id: &str) -> Option<&'a ModelEntry> {
    entries.iter().find(|entry| {
        entry
            .prefixes
            .iter()
            .any(|prefix| prefix.eq_ignore_ascii_case(model_id) || names_exactly(entry, model_id))
    })
}

fn canonical(entry: &ModelEntry, fallback: &str) -> String {
    entry
        .prefixes
        .first()
        .map_or(fallback, String::as_str)
        .to_string()
}

fn caps_in(
    entries: &[ModelEntry],
    model_id: &str,
    slug: Option<&str>,
    source: impl Fn(String) -> CapsSource,
) -> Option<ModelCaps> {
    let (entry, _exact, _prefix) = best(entries, model_id)?;
    Some(caps_from_entry(
        entry,
        slug,
        model_id,
        source(canonical(entry, model_id)),
    ))
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
        // A spec-level yes is a usable fallback; a spec-level no is not,
        // because providers like Zai turn thinking on per model elsewhere.
        supports_thinking: entry
            .supports_thinking
            .or_else(|| spec.filter(|spec| spec.supports_thinking).map(|_| true)),
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
    if let Some(spec) = spec {
        caps.context_window = caps.context_window.or(Some(spec.fallback_context_window));
        caps.max_output_tokens = caps.max_output_tokens.or(spec.fallback_max_output);
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
    fn a_curated_alias_target_resolves() {
        let caps = resolve_target("openai/gpt-6-sol").unwrap();
        assert_eq!(caps.family, Some(ModelFamily::Gpt));
        assert_eq!(caps.context_window, Some(1_050_000));
        assert!(matches!(
            caps.source,
            Some(CapsSource::Curated { slug: "openai", .. })
        ));
    }

    #[test]
    fn a_bare_provider_alias_borrows_family_and_fallbacks() {
        let caps = resolve_target("zai").unwrap();
        assert_eq!(caps.family, Some(ModelFamily::Glm));
        assert_eq!(caps.context_window, Some(128_000));
        assert_eq!(caps.max_output_tokens, Some(16_000));
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
    fn gateway_namespaces_reach_a_unique_exact_row() {
        let caps = unique_exact("us.openai.gpt-6-sol").unwrap();
        assert_eq!(caps.family, Some(ModelFamily::Gpt));
        assert!(matches!(
            caps.source,
            Some(CapsSource::Curated { slug: "openai", .. })
        ));

        let caps = unique_exact("zai-org/GLM-5.3").unwrap();
        assert_eq!(caps.family, Some(ModelFamily::Glm));
    }

    #[test]
    fn a_name_two_tables_disagree_on_stays_unknown() {
        assert!(
            unique_exact("gpt-5.4").is_none(),
            "OpenAI and Copilot both ship a gpt-5.4 row"
        );
        assert!(
            unique_exact("anthropic.claude-opus-5").is_none(),
            "Anthropic and Copilot both ship claude-opus-5"
        );
        assert!(unique_exact("gpt-6-sol").is_some());
    }

    #[test]
    fn unknown_names_are_not_guessed() {
        assert!(unique_exact("mystery-model").is_none());
        assert!(
            unique_exact("my-claude-fork").is_none(),
            "a substring is not a model"
        );
    }
}
