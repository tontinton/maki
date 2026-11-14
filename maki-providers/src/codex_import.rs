//! Codex providers a login picker can offer, and the write that imports them.

use std::io;
use std::path::{Path, PathBuf};

use maki_config::codex::{self, CodexImport};
use maki_config::providers::ProvidersConfig;
use maki_storage::StateDir;
use maki_storage::model::{persist_model, read_model};
use maki_storage::paths::tilde;

use crate::providers::{catalog, plugin};
use crate::spec::ProviderRegistry;

/// What Codex's config holds under slugs maki has no use for yet, with the
/// file it came from. `None` when there is nothing to offer.
pub fn offer(config: &ProvidersConfig) -> Option<(PathBuf, CodexImport)> {
    let path = codex::config_path()?;
    let mut import = codex::load(&path)?;
    let is_free = |slug: &str| slug_is_free(slug, config, &catalog::serves_slug);
    import.providers.retain(|slug, _| is_free(slug));
    import.unsupported.retain(|(slug, _)| is_free(slug));
    (!import.providers.is_empty()).then_some((path, import))
}

/// Whether an import may take `slug`. A `providers.toml` entry outranks a
/// models.dev provider and would be handed the key saved for it, so a slug
/// the catalog serves counts as taken like any other. `serves_slug` answers
/// from the cached catalog, so the check holds before the list is fetched.
fn slug_is_free(slug: &str, config: &ProvidersConfig, serves_slug: &dyn Fn(&str) -> bool) -> bool {
    !ProviderRegistry::is_shipped(slug)
        && !plugin::owns_slug(slug)
        && config.get(slug).is_none()
        && !serves_slug(slug)
}

/// The picker row for an offer, the same in every login list.
pub fn label(path: &Path, import: &CodexImport) -> String {
    let slugs: Vec<&str> = import.providers.keys().map(String::as_str).collect();
    format!("Import from Codex ({}): {}", tilde(path), slugs.join(", "))
}

/// Writes the entries and returns the model maki starts with from now on,
/// if the import changed it.
pub fn import(
    import: &CodexImport,
    mut config: ProvidersConfig,
    storage: &StateDir,
) -> io::Result<Option<String>> {
    for (slug, def) in &import.providers {
        config.upsert(slug.clone(), def.clone());
    }
    config.save()?;
    Ok(adopt_model(import, storage))
}

/// Codex's model becomes the saved one on a first login only. A model the
/// user already picked stays theirs.
fn adopt_model(import: &CodexImport, storage: &StateDir) -> Option<String> {
    let model = import
        .providers
        .values()
        .find_map(|def| def.default_model.as_deref())?;
    read_model(storage).is_none().then(|| {
        persist_model(storage, model);
        model.to_owned()
    })
}

#[cfg(test)]
mod tests {
    use maki_config::providers::ProviderDef;
    use test_case::test_case;

    use super::*;

    const FREE_SLUG: &str = "team-proxy";
    const CONFIGURED_SLUG: &str = "my-gateway";
    const COMPILED_SLUG: &str = "anthropic";
    const BUNDLED_PLUGIN_SLUG: &str = "openrouter";
    const CATALOG_SLUG: &str = "fireworks-ai";
    const CODEX_MODEL: &str = "team-proxy/gpt-5.5";
    const SAVED_MODEL: &str = "anthropic/claude-sonnet-5";

    #[test_case(FREE_SLUG, true ; "nothing_answers_for_it")]
    #[test_case(COMPILED_SLUG, false ; "compiled_provider")]
    #[test_case(BUNDLED_PLUGIN_SLUG, false ; "bundled_plugin")]
    #[test_case(CONFIGURED_SLUG, false ; "providers_toml_entry")]
    #[test_case(CATALOG_SLUG, false ; "catalog_provider")]
    fn an_import_only_takes_free_slugs(slug: &str, expected: bool) {
        let mut config = ProvidersConfig::default();
        config.upsert(CONFIGURED_SLUG.to_owned(), ProviderDef::default());
        let serves_slug = |slug: &str| slug == CATALOG_SLUG;
        assert_eq!(slug_is_free(slug, &config, &serves_slug), expected);
    }

    #[test_case(None, Some(CODEX_MODEL), Some(CODEX_MODEL) ; "first_login_starts_on_it")]
    #[test_case(Some(SAVED_MODEL), Some(CODEX_MODEL), Some(SAVED_MODEL) ; "saved_model_stays")]
    #[test_case(None, None, None ; "codex_names_no_model")]
    fn codex_model_is_adopted_on_a_first_login_only(
        saved: Option<&str>,
        codex: Option<&str>,
        expected: Option<&str>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(dir.path().to_path_buf());
        if let Some(model) = saved {
            persist_model(&storage, model);
        }
        let mut import = CodexImport::default();
        import.providers.insert(
            FREE_SLUG.to_owned(),
            ProviderDef {
                default_model: codex.map(str::to_owned),
                ..Default::default()
            },
        );

        let adopted = adopt_model(&import, &storage);

        assert_eq!(read_model(&storage).as_deref(), expected);
        assert_eq!(adopted.is_some(), saved.is_none() && codex.is_some());
    }
}
