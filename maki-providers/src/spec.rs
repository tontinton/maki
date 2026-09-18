use std::sync::{Arc, Mutex};

use maki_config::providers::{BuiltInProvider, Protocol, ProviderPlan};

use crate::AgentError;
use crate::model::{ModelEntry, ModelFamily, ModelTier};
use crate::pricing::PricingSchedule;
use crate::provider::Provider;
use crate::providers::{
    ResolvedAuth, Timeouts, anthropic, aperture, copilot, custom, deepseek, dynamic, google,
    llama_cpp, mistral, ollama, openai, opencode, openrouter, regolo, requesty, synthetic, tensorx,
    xai, zai,
};

/// Stands in for the model table when a provider curates none.
pub const GENERIC_DISCOVERY_NOTE: &str =
    "No hardcoded model catalog. Use any model ID supported by this provider.";

/// Everything maki knows about one provider without asking the network.
///
/// This struct is the checklist for adding a provider. Every field is
/// mandatory in the literal, and that is the point: there is deliberately no
/// `Default`, no `#[non_exhaustive]`, and no row uses `..`, so adding a field
/// here breaks every row until someone answers for each one.
#[derive(Debug)]
pub struct ProviderSpec {
    pub slug: &'static str,
    pub display_name: &'static str,
    pub api_key_env: &'static str,

    pub family: ModelFamily,
    pub supports_thinking: bool,
    pub accepts_arbitrary_models: bool,
    pub fallback_max_output: Option<u32>,
    pub fallback_context_window: u32,
    pub models: &'static [ModelEntry],
    /// Set by the providers whose rates move with the wall clock, so the hours
    /// sit next to the prices they scale. Everyone else bills flat.
    pub pricing_schedule: Option<&'static PricingSchedule>,

    /// `None` for a slug `catalog::try_create` constructs out of models.dev;
    /// that row carries metadata only.
    pub native: Option<Native>,
    /// `None` for slugs that are never a `maki auth login` target and so have
    /// no `maki-config` row. `Some` is the *only* authoring site for that row;
    /// see [`ProviderSpec::config_row`].
    pub login: Option<LoginConfig>,
    pub docs: GeneratedDocs,
}

pub type NewFn = fn(Timeouts) -> Result<Box<dyn Provider>, AgentError>;

/// The third argument is the system prefix, which a fn-pointer type has no
/// room to name.
pub type WithAuthFn = fn(Arc<Mutex<ResolvedAuth>>, Timeouts, Option<String>) -> Box<dyn Provider>;

/// How maki builds a provider it does not read out of the catalog.
#[derive(Debug, Clone, Copy)]
pub struct Native {
    /// Built from config and env by maki itself.
    pub new: NewFn,
    /// Build against auth someone else resolved. Used by `providers.d`
    /// scripts and by Aperture's gateway routing.
    pub with_auth: WithAuthFn,
    /// `Some` iff Aperture can proxy onto this provider. Lives here on
    /// purpose: a route is only expressible for a provider that has
    /// `with_auth`.
    pub aperture: Option<ApertureRoute>,
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
    /// Render `models` as the tier table.
    Table,
    /// `models` is empty; say where models come from instead.
    Discovered(&'static str),
}

impl ProviderSpec {
    /// Whether maki builds this provider itself, as opposed to reading it out
    /// of the models.dev catalog.
    pub const fn is_native(&self) -> bool {
        self.native.is_some()
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
                default_model: login.default_model,
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
    mistral::SPEC,
    zai::SPEC,
    deepseek::SPEC,
    openrouter::SPEC,
    requesty::SPEC,
    synthetic::SPEC,
    regolo::SPEC,
    tensorx::SPEC,
    opencode::ZEN_SPEC,
    xai::SPEC,
    aperture::SPEC,
    opencode::GO_SPEC,
];

pub struct ProviderRegistry;

impl ProviderRegistry {
    pub fn get(slug: &str) -> Option<&'static ProviderSpec> {
        BUILTINS.iter().find(|s| s.slug == slug)
    }

    /// Like `get`, but a dynamic or `providers.toml` slug resolves to its base
    /// provider's spec, so thinking support, display name and tier defaults
    /// still answer for a stub that declares no models.
    ///
    /// Both fallbacks resolve through [`Self::get`], never back through here,
    /// so the lookup cannot recurse.
    pub fn for_slug(slug: &str) -> Option<&'static ProviderSpec> {
        Self::get(slug)
            .or_else(|| dynamic::base_for_slug(slug))
            .or_else(|| custom::base_spec(slug))
    }

    pub fn builtins() -> &'static [ProviderSpec] {
        BUILTINS
    }

    /// The slugs a `providers.d` script or a `providers.toml` entry may not
    /// claim, and the ones a dynamic script may name as its `base`.
    pub fn native_slugs() -> impl Iterator<Item = &'static str> {
        BUILTINS.iter().filter(|s| s.is_native()).map(|s| s.slug)
    }

    pub fn find_default_for_tier(slug: &str, tier: ModelTier) -> Option<&'static ModelEntry> {
        Self::for_slug(slug)?
            .models
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
    /// Carries the constructor rather than the spec, so "builtin" and
    /// "buildable" cannot come apart.
    Builtin(NewFn),
    /// `providers.d/*`
    Script,
    /// `providers.toml`
    Custom,
    /// models.dev
    Catalog,
    Unknown,
}

impl Owner {
    pub fn of(slug: &str) -> Self {
        if let Some(native) = ProviderRegistry::get(slug).and_then(|s| s.native) {
            return Self::Builtin(native.new);
        }
        if dynamic::display_name(slug).is_some() {
            return Self::Script;
        }
        if custom::base_spec(slug).is_some() {
            return Self::Custom;
        }
        if ProviderRegistry::get(slug).is_some() {
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
        let expected: HashSet<&str> = ProviderRegistry::native_slugs()
            .chain(CATALOG_BACKED_BUILTINS.iter().copied())
            .collect();
        assert_eq!(registry, expected);
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

    /// A spec row alone does not make a provider buildable. Hand `opencode-go`
    /// a `native` and `provider_for_slug` would quietly stop asking models.dev
    /// for it.
    #[test]
    fn a_spec_row_without_a_constructor_still_belongs_to_the_catalog() {
        assert!(matches!(Owner::of(anthropic::SLUG), Owner::Builtin(_)));
        assert!(matches!(Owner::of(opencode::GO_SLUG), Owner::Catalog));
    }
}
