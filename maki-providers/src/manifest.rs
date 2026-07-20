use crate::model::{ModelEntry, ModelFamily, ModelTier};
use crate::providers::{
    anthropic, copilot, custom, deepseek, dynamic, google, llama_cpp, mistral, ollama, openai,
    openrouter, synthetic, tensorx, zai,
};

#[derive(Debug, Clone, Copy)]
pub struct ProviderManifest {
    pub slug: &'static str,
    pub display_name: &'static str,
    pub family: ModelFamily,
    pub supports_thinking: bool,
    pub accepts_arbitrary_models: bool,
    pub fallback_max_output: Option<u32>,
    pub fallback_context_window: u32,
    pub models: &'static [ModelEntry],
}

const ANTHROPIC: ProviderManifest = ProviderManifest {
    slug: "anthropic",
    display_name: "Anthropic",
    family: ModelFamily::Claude,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(128_000),
    fallback_context_window: 200_000,
    models: anthropic::models(),
};

const OPENAI: ProviderManifest = ProviderManifest {
    slug: "openai",
    display_name: "OpenAI",
    family: ModelFamily::Gpt,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(100_000),
    fallback_context_window: 200_000,
    models: openai::models(),
};

const GOOGLE: ProviderManifest = ProviderManifest {
    slug: "google",
    display_name: "Google",
    family: ModelFamily::Gemini,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(65_536),
    fallback_context_window: 1_000_000,
    models: google::models(),
};

const COPILOT: ProviderManifest = ProviderManifest {
    slug: "copilot",
    display_name: "Copilot",
    family: ModelFamily::Generic,
    supports_thinking: false,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(100_000),
    fallback_context_window: 200_000,
    models: copilot::models(),
};

const OLLAMA: ProviderManifest = ProviderManifest {
    slug: "ollama",
    display_name: "Ollama",
    family: ModelFamily::Generic,
    supports_thinking: false,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(16_384),
    fallback_context_window: 128_000,
    models: ollama::models(),
};

const LLAMA_CPP: ProviderManifest = ProviderManifest {
    slug: "llama-cpp",
    display_name: "LlamaCpp",
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: None,
    fallback_context_window: 128_000,
    models: llama_cpp::models(),
};

const MISTRAL: ProviderManifest = ProviderManifest {
    slug: "mistral",
    display_name: "Mistral",
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(32_000),
    fallback_context_window: 128_000,
    models: mistral::models(),
};

const ZAI: ProviderManifest = ProviderManifest {
    slug: "zai",
    display_name: "Z.AI",
    family: ModelFamily::Glm,
    supports_thinking: false,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(16_000),
    fallback_context_window: 128_000,
    models: zai::models(),
};

const DEEPSEEK: ProviderManifest = ProviderManifest {
    slug: "deepseek",
    display_name: "DeepSeek",
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(384_000),
    fallback_context_window: 1_000_000,
    models: deepseek::models(),
};

const OPENROUTER: ProviderManifest = ProviderManifest {
    slug: "openrouter",
    display_name: "OpenRouter",
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(128_000),
    fallback_context_window: 200_000,
    models: openrouter::models(),
};

const SYNTHETIC: ProviderManifest = ProviderManifest {
    slug: "synthetic",
    display_name: "Synthetic",
    family: ModelFamily::Synthetic,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(32_000),
    fallback_context_window: 128_000,
    models: synthetic::models(),
};

const TENSORX: ProviderManifest = ProviderManifest {
    slug: "tensorx",
    display_name: "TensorX",
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: None,
    fallback_context_window: 200_000,
    models: tensorx::models(),
};

const OPENCODE: ProviderManifest = ProviderManifest {
    slug: "opencode",
    display_name: "Opencode",
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(128_000),
    fallback_context_window: 256_000,
    models: &[],
};

const BUILTINS: &[ProviderManifest] = &[
    ANTHROPIC, OPENAI, GOOGLE, COPILOT, OLLAMA, LLAMA_CPP, MISTRAL, ZAI, DEEPSEEK, OPENROUTER,
    SYNTHETIC, TENSORX, OPENCODE,
];

pub struct ManifestRegistry;

impl ManifestRegistry {
    pub fn get(slug: &str) -> Option<&'static ProviderManifest> {
        BUILTINS.iter().find(|m| m.slug == slug)
    }

    /// Like `get`, but resolves dynamic and custom (providers.toml) slugs to
    /// their base provider's manifest so capability lookups (thinking, display
    /// name, tier defaults) still work for stubs that declare no models. `None`
    /// for an unknown slug, so callers pick a fallback instead of silently
    /// inheriting a zeroed manifest.
    pub fn for_slug(slug: &str) -> Option<&'static ProviderManifest> {
        Self::get(slug)
            .or_else(|| dynamic::base_for_slug(slug).and_then(|base| Self::get(&base.to_string())))
            .or_else(|| custom::base_kind(slug).and_then(|base| Self::get(&base.to_string())))
    }

    pub fn builtins() -> &'static [ProviderManifest] {
        BUILTINS
    }

    pub fn find_default_for_tier(slug: &str, tier: ModelTier) -> Option<&'static ModelEntry> {
        Self::for_slug(slug)?
            .models
            .iter()
            .find(|e| e.default && e.tier == tier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderKind;
    use maki_config::providers::BuiltInProvider;
    use std::str::FromStr;
    use strum::IntoEnumIterator;

    #[test]
    fn every_builtin_manifest_matches_provider_kind_for_mirrored_fields() {
        for manifest in BUILTINS {
            let kind = ProviderKind::from_str(manifest.slug)
                .unwrap_or_else(|_| panic!("manifest slug {} has no ProviderKind", manifest.slug));
            // Slug must equal `ProviderKind`'s `Display`, or slug-based routing
            // and spec formatting would silently diverge.
            assert_eq!(kind.to_string(), manifest.slug, "{}", manifest.slug);
            assert_eq!(
                manifest.display_name,
                kind.display_name(),
                "{}",
                manifest.slug
            );
            assert_eq!(manifest.family, kind.family(), "{}", manifest.slug);
            assert_eq!(
                manifest.fallback_max_output,
                kind.fallback_max_output(),
                "{}",
                manifest.slug,
            );
            assert_eq!(
                manifest.fallback_context_window,
                kind.fallback_context_window(),
                "{}",
                manifest.slug,
            );
        }
    }

    #[test]
    fn for_slug_returns_none_for_unknown_slug() {
        assert!(ManifestRegistry::for_slug("totally-unknown-slug").is_none());
    }

    #[test]
    fn for_slug_returns_builtin_directly() {
        let manifest = ManifestRegistry::for_slug("anthropic").unwrap();
        assert_eq!(manifest.slug, "anthropic");
        assert_eq!(manifest.display_name, "Anthropic");
    }

    #[test]
    fn builtin_count_matches_provider_kind_count() {
        let kind_count = ProviderKind::iter().count();
        assert_eq!(
            BUILTINS.len(),
            kind_count,
            "BUILTINS has {} manifests but ProviderKind has {} variants",
            BUILTINS.len(),
            kind_count,
        );
    }

    #[test]
    fn every_builtin_provider_inventory_entry_has_matching_manifest() {
        for builtin in inventory::iter::<BuiltInProvider>() {
            let manifest = ManifestRegistry::get(builtin.slug).unwrap_or_else(|| {
                panic!(
                    "BuiltInProvider slug {:?} has no ProviderManifest",
                    builtin.slug,
                )
            });
            assert_eq!(
                manifest.display_name, builtin.display_name,
                "display_name mismatch between manifest and BuiltInProvider for slug {:?}",
                builtin.slug,
            );
        }
    }
}
