use std::sync::{Arc, Mutex};

use maki_config::PROVIDER_BUILTINS;
use maki_config::providers::{BuiltInProvider, Protocol, ProviderPlan};

use crate::AgentError;
use crate::manifest;
use crate::model::{ModelEntry, ModelFamily, ModelTier};
use crate::pricing::PricingSchedule;
use crate::provider::Provider;
use crate::providers::{
    ResolvedAuth, Timeouts, anthropic, aperture, copilot, custom, google, llama_cpp, ollama,
    openai, opencode, plugin, xai, zai,
};

/// The slugs a plugin may name as its `base`, in documentation order. A `base`
/// borrows a bespoke `impl Provider` whole, so every entry must be a
/// [`Build::Native`] row. Written out rather than derived from the rows, so a
/// provider leaving it is a deliberate, documented break and never the side
/// effect of a port.
pub const BASES: &[&str] = &[
    anthropic::SPEC.slug,
    openai::SPEC.slug,
    google::SPEC.slug,
    copilot::SPEC.slug,
    ollama::SPEC.slug,
    llama_cpp::SPEC.slug,
    zai::SPEC.slug,
    opencode::ZEN_SPEC.slug,
    xai::SPEC.slug,
    aperture::SPEC.slug,
];

/// Stands in for the model table when a provider curates none.
pub const GENERIC_DISCOVERY_NOTE: &str =
    "No hardcoded model catalog. Use any model ID supported by this provider.";

/// Everything maki knows about one provider without asking the network.
///
/// This struct is the checklist for adding a native provider. Every field is
/// mandatory in the literal, and that is the point: there is deliberately no
/// `Default`, no `#[non_exhaustive]`, and no row uses `..`, so adding a field
/// here breaks every row until someone answers for each one.
///
/// The checklist is two files: this row, and `models/<slug>.toml` next to it
/// carrying the provider's curated table. A Lua plugin provider gets one of
/// these built from its declaration instead, see [`plugin::spec`].
#[derive(Debug)]
pub struct ProviderSpec {
    pub slug: &'static str,
    pub display_name: &'static str,
    pub api_key_env: &'static str,

    pub family: ModelFamily,
    pub supports_thinking: bool,
    /// The endpoint honours `defer_loading` and expands `tool_reference`
    /// blocks. A property of the backend, not the protocol, so a slug that
    /// borrows this row's protocol does not inherit it; see
    /// [`crate::Model::supports_deferred_tools`].
    pub supports_deferred_tools: bool,
    pub accepts_arbitrary_models: bool,
    pub fallback_max_output: Option<u32>,
    pub fallback_context_window: u32,
    /// This provider's curated table, embedded from `models/<slug>.toml`. Read
    /// it through [`ProviderSpec::models`], which serves the parsed rows.
    /// [`NO_CURATED_MODELS`] for a provider whose ids all come from a live
    /// catalog, and for a declaration, which carries its rows on
    /// [`Build::Declared`].
    pub models_toml: &'static str,
    /// Set by the providers whose rates move with the wall clock, so the hours
    /// sit next to the prices they scale. Everyone else bills flat.
    pub pricing_schedule: Option<&'static PricingSchedule>,

    pub build: Build,
    /// `Some` iff Aperture can proxy onto this provider. Lives on the spec, not
    /// on [`Native`], so the route survives a provider losing its bespoke impl.
    pub aperture: Option<ApertureRoute>,
    /// `None` for slugs that are never a `maki auth login` target and so have
    /// no `maki-config` row. `Some` is the *only* authoring site for that row;
    /// see [`ProviderSpec::config_row`].
    pub login: Option<LoginConfig>,
    pub docs: GeneratedDocs,
}

/// For a provider whose ids all come from a live catalog.
pub const NO_CURATED_MODELS: &str = "";

pub type NewFn = fn(Timeouts) -> Result<Box<dyn Provider>, AgentError>;

/// The third argument is the system prefix, which a fn-pointer type has no
/// room to name.
pub type WithAuthFn = fn(Arc<Mutex<ResolvedAuth>>, Timeouts, Option<String>) -> Box<dyn Provider>;

/// How maki builds the provider this row describes.
#[derive(Debug, Clone, Copy)]
pub enum Build {
    /// A bespoke `impl Provider`.
    Native(Native),
    /// A Lua plugin's declaration, with the model rows it states.
    Declared(&'static [ModelEntry]),
    /// `catalog::try_create`, out of models.dev. The row carries metadata only.
    Catalog,
}

/// How maki builds a provider that has its own `impl Provider`.
#[derive(Debug, Clone, Copy)]
pub struct Native {
    /// Built from config and env by maki itself.
    pub new: NewFn,
    /// Build against auth someone else resolved. Used by plugin providers
    /// that extend a base slug and by Aperture's gateway routing.
    pub with_auth: WithAuthFn,
}

#[derive(Debug, Clone, Copy)]
pub struct ApertureRoute {
    /// Path prefix maki sends to the gateway, which appends the whole incoming
    /// request path to the upstream's base url. Empty where the upstream base
    /// url already carries its own path, and each provider says why at its own
    /// spec. Mandatory rather than defaulted, so a new provider cannot silently
    /// inherit `/v1`.
    pub path_prefix: &'static str,
}

/// The half of a spec only `maki-config` reads. `maki-config` cannot see
/// `maki-providers`, so it gets this through `inventory` as a projection.
#[derive(Debug)]
pub struct LoginConfig {
    pub protocol: Protocol,
    pub default_base_url: &'static str,
    pub default_model: &'static str,
    pub plans: Option<&'static [(&'static str, ProviderPlan)]>,
    pub login_url: Option<&'static str>,
    /// Prompt for a base url during login, as local inference servers need.
    pub needs_url: bool,
}

#[derive(Debug)]
pub struct GeneratedDocs {
    pub api_urls: &'static [&'static str],
    pub features: Option<&'static str>,
    pub auth: AuthDoc,
    pub catalog: CatalogDoc,
    /// Emitted after the model table.
    pub trailing_notes: &'static [&'static str],
}

#[derive(Debug)]
pub enum AuthDoc {
    /// `` `ENV_VAR` ``
    EnvVar,
    /// `` `ENV_VAR` `` followed by a note (OAuth availability, shared keys).
    EnvVarWith(&'static str),
    /// The env var is not the story: Ollama's `OLLAMA_HOST`, Aperture's
    /// `APERTURE_HOST`.
    Custom(&'static str),
}

#[derive(Debug)]
pub enum CatalogDoc {
    /// Render [`ProviderSpec::models`] as the tier table.
    Table,
    /// The table is empty; say where models come from instead.
    Discovered(&'static str),
}

impl ProviderSpec {
    /// The one way to reach this provider's curated table.
    pub fn models(&self) -> &'static [ModelEntry] {
        match self.build {
            Build::Declared(models) => models,
            Build::Native(_) | Build::Catalog => manifest::table(self.slug),
        }
    }

    pub const fn native(&self) -> Option<Native> {
        match self.build {
            Build::Native(native) => Some(native),
            Build::Declared(_) | Build::Catalog => None,
        }
    }

    /// The `maki-config` view of this provider, derived. Const-panics for a
    /// spec with no `login`, so submitting a login-less provider to the
    /// inventory is a compile error rather than a wrong row.
    pub const fn config_row(&self) -> BuiltInProvider {
        match &self.login {
            Some(login) => BuiltInProvider {
                slug: self.slug,
                display_name: self.display_name,
                default_api_key_env: self.api_key_env,
                protocol: login.protocol,
                default_base_url: login.default_base_url,
                default_model: Some(login.default_model),
                plans: login.plans,
                login_url: login.login_url,
                needs_url: login.needs_url,
            },
            None => panic!("provider has no login config; do not submit it to the inventory"),
        }
    }
}

/// The order users see: doc sections, offline model listing, picker batches.
/// Written out rather than collected with `inventory`, whose iteration order is
/// unspecified and would silently reshuffle the generated docs.
///
/// Each entry copies a module's `SPEC` const, so the two live at different
/// addresses: identify a spec by `slug`, never by pointer.
const BUILTINS: &[ProviderSpec] = &[
    anthropic::SPEC,
    openai::SPEC,
    google::SPEC,
    copilot::SPEC,
    ollama::SPEC,
    llama_cpp::SPEC,
    zai::SPEC,
    opencode::ZEN_SPEC,
    xai::SPEC,
    aperture::SPEC,
    opencode::GO_SPEC,
];

pub struct ProviderRegistry;

impl ProviderRegistry {
    /// The row that describes `slug` itself: compiled in, or built from a
    /// plugin's declaration. The one to reach for unless plugin providers
    /// must be left out on purpose, see [`Self::compiled`].
    pub fn get(slug: &str) -> Option<&'static ProviderSpec> {
        Self::compiled(slug).or_else(|| plugin::spec(slug))
    }

    /// Like [`Self::get`], but a `providers.toml` slug resolves to its base
    /// provider's spec, so thinking support, display name and tier defaults
    /// still answer for a stub that declares no models.
    ///
    /// The fallback resolves through [`Self::compiled`], never back through
    /// here, so the lookup cannot recurse.
    pub fn for_slug(slug: &str) -> Option<&'static ProviderSpec> {
        Self::get(slug).or_else(|| custom::base_spec(slug))
    }

    /// Every row [`Self::get`] answers for: the compiled-in ones in their
    /// order, then the plugin providers by slug.
    pub fn all() -> Vec<&'static ProviderSpec> {
        BUILTINS.iter().chain(plugin::specs()).collect()
    }

    /// A row compiled into this binary, never a plugin's.
    pub fn compiled(slug: &str) -> Option<&'static ProviderSpec> {
        BUILTINS.iter().find(|s| s.slug == slug)
    }

    pub fn all_compiled() -> &'static [ProviderSpec] {
        BUILTINS
    }

    /// Whether maki ships `slug`, compiled in or as a bundled plugin, loaded
    /// or not. Such a slug is never a third party's to take.
    pub fn is_shipped(slug: &str) -> bool {
        Self::compiled(slug).is_some() || PROVIDER_BUILTINS.contains(&slug)
    }

    pub fn find_default_for_tier(slug: &str, tier: ModelTier) -> Option<&'static ModelEntry> {
        Self::for_slug(slug)?
            .models()
            .iter()
            .find(|e| e.default && e.tier == tier)
    }
}

/// Which mechanism constructs a provider for this slug. The variant order is
/// the resolution order, and this is the only place it is written down.
///
/// A different question from [`ProviderRegistry::get`], which answers what maki
/// knows about a slug: a catalog-backed builtin has a spec row and is still
/// `Catalog` here.
pub enum Owner {
    /// A registered declaration.
    Plugin,
    /// Carries the constructor rather than the spec, so "builtin" and
    /// "buildable" cannot come apart.
    Builtin(NewFn),
    /// `providers.toml`
    Custom,
    /// models.dev
    Catalog,
    Unknown,
}

impl Owner {
    pub fn of(slug: &str) -> Self {
        // Registration refuses a built-in slug, so a declaration never
        // shadows a row below.
        if plugin::is_registered(slug) {
            return Self::Plugin;
        }
        let builtin = ProviderRegistry::compiled(slug);
        if let Some(native) = builtin.and_then(ProviderSpec::native) {
            return Self::Builtin(native.new);
        }
        if custom::base_spec(slug).is_some() {
            return Self::Custom;
        }
        if builtin.is_some() {
            return Self::Catalog;
        }
        Self::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::catalog::CATALOG_BACKED_BUILTINS;
    use std::collections::HashSet;

    /// `provider.rs` skips a catalog row for any slug the registry knows. That
    /// is only the right filter while the registry is exactly what maki builds
    /// plus what it deliberately reads from the catalog. A duplicate slug would
    /// vanish into the set, so count before comparing.
    #[test]
    fn builtins_are_the_native_and_catalog_backed_slugs() {
        let registry: HashSet<&str> = BUILTINS.iter().map(|s| s.slug).collect();
        assert_eq!(registry.len(), BUILTINS.len(), "duplicate slug in BUILTINS");
        for spec in BUILTINS {
            let built = match spec.build {
                Build::Native(_) => true,
                Build::Declared(_) => false,
                Build::Catalog => CATALOG_BACKED_BUILTINS.contains(&spec.slug),
            };
            assert!(built, "nothing builds {}", spec.slug);
        }
        for slug in CATALOG_BACKED_BUILTINS {
            assert!(registry.contains(slug), "{slug} has no spec row");
        }
    }

    /// A `base` borrows a native constructor, so a slug on the list that lost
    /// its `impl Provider` would pass registration and fail every `create`.
    #[test]
    fn every_base_is_a_native_builtin() {
        for base in BASES {
            let spec = ProviderRegistry::get(base);
            assert!(spec.and_then(ProviderSpec::native).is_some(), "{base}");
        }
    }

    /// The picker lists the inventory, so a spec without an entry is a
    /// provider the user cannot reach. OpenRouter shipped that way for months.
    /// The other direction catches an `inventory::submit!` left on a row that
    /// no longer has a `login`.
    #[test]
    fn login_specs_are_exactly_the_inventory_rows() {
        for spec in BUILTINS {
            let registered = inventory::iter::<BuiltInProvider>()
                .into_iter()
                .any(|b| b.slug == spec.slug);
            assert_eq!(
                spec.login.is_some(),
                registered,
                "spec {:?} declares login={} but inventory registration={}",
                spec.slug,
                spec.login.is_some(),
                registered,
            );
        }
    }

    /// A malformed table is a runtime panic now, not a compile error, so this
    /// forces every one of them in CI. Never `#[ignore]` it.
    #[test]
    fn every_builtin_model_table_parses() {
        for spec in BUILTINS {
            spec.models();
        }
    }

    /// Closes the third side of the slug/file mapping: `include_str!` catches a
    /// missing file, the parser catches a wrong one, and this catches an orphan
    /// table or one written but never wired into a spec.
    #[test]
    fn model_files_match_curated_providers() {
        const MODELS_DIR: &str = "maki-providers/models should hold named files";
        let files: HashSet<String> =
            std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/models"))
                .expect(MODELS_DIR)
                .map(|entry| {
                    let path = entry.expect(MODELS_DIR).path();
                    let stem = path.file_stem().expect(MODELS_DIR);
                    stem.to_string_lossy().into_owned()
                })
                .collect();
        let curated: HashSet<String> = BUILTINS
            .iter()
            .filter(|spec| spec.models_toml != NO_CURATED_MODELS)
            .map(|spec| spec.slug.to_owned())
            .collect();
        assert_eq!(files, curated);
    }

    /// A spec row alone does not make a provider buildable. Hand `opencode-go`
    /// a [`Build::Native`] and `provider_for_slug` would quietly stop asking models.dev
    /// for it.
    #[test]
    fn a_spec_row_without_a_constructor_still_belongs_to_the_catalog() {
        assert!(matches!(Owner::of(anthropic::SLUG), Owner::Builtin(_)));
        assert!(matches!(Owner::of(opencode::GO_SLUG), Owner::Catalog));
    }
}
