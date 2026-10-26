//! The provider universe the `/login` wizard and the model picker both
//! render: builtins, `providers.toml` customs, and models.dev catalog
//! entries, each resolved to its auth state as of the call.

use maki_config::providers::{self, ProvidersConfig};
use maki_providers::catalog_providers_if_available;
use maki_storage::StateDir;
use maki_storage::auth::{load_provider_credentials, load_tokens};

/// Where a provider's credentials currently come from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Auth {
    /// A key or OAuth tokens in storage.
    Saved,
    /// The key env var is set in this process.
    Env,
    /// Nothing stored, nothing exported.
    Missing,
}

#[derive(Clone)]
pub(crate) struct ProviderEntry {
    pub slug: String,
    pub display_name: String,
    pub auth: Auth,
    /// A `providers.toml` provider with a base_url but no key of its own:
    /// usable as-is, though `/login` still offers to set a key.
    pub configured: bool,
    /// Catalog providers are listed under their own section.
    pub from_catalog: bool,
}

impl ProviderEntry {
    pub(crate) fn usable(&self) -> bool {
        self.auth != Auth::Missing || self.configured
    }
}

fn auth_of(storage: &StateDir, slug: &str, env_key: Option<&str>) -> Auth {
    if load_provider_credentials(storage, slug).is_some() || load_tokens(storage, slug).is_some() {
        return Auth::Saved;
    }
    let env_set = env_key.is_some_and(|key| std::env::var(key).is_ok());
    if env_set { Auth::Env } else { Auth::Missing }
}

/// The catalog download has not landed yet, so its providers are unknowable.
pub(crate) fn provider_entries(storage: &StateDir) -> (Vec<ProviderEntry>, bool) {
    let config = ProvidersConfig::load();
    let mut entries: Vec<ProviderEntry> = providers::all_builtins()
        .iter()
        .map(|b| ProviderEntry {
            slug: b.slug.to_string(),
            display_name: b.display_name.to_string(),
            auth: auth_of(storage, b.slug, Some(b.default_api_key_env)),
            configured: config.get(b.slug).is_some_and(|d| d.base_url.is_some()),
            from_catalog: false,
        })
        .collect();

    for (slug, def) in &config.providers {
        if slug == "opencode" || providers::builtin_provider(slug).is_some() {
            continue;
        }
        entries.push(ProviderEntry {
            slug: slug.clone(),
            display_name: providers::resolve_display_name(slug, config.get(slug)),
            auth: auth_of(storage, slug, def.api_key_env.as_deref()),
            configured: false,
            from_catalog: false,
        });
    }

    let Some(catalog) = catalog_providers_if_available() else {
        return (entries, false);
    };
    for cat in catalog {
        // A slug the config or a builtin already answers for must not come
        // a second time from the catalog — same rule discovery applies.
        if entries.iter().any(|e| e.slug == cat.slug) {
            continue;
        }
        entries.push(ProviderEntry {
            slug: cat.slug.clone(),
            display_name: cat.display_name.clone(),
            auth: if cat.load_key_from_storage(storage).is_some() {
                Auth::Saved
            } else if cat.env_key_set().is_some() {
                Auth::Env
            } else {
                Auth::Missing
            },
            configured: false,
            from_catalog: true,
        });
    }
    (entries, true)
}
