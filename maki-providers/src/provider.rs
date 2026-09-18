use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use flume::Sender;
use serde_json::Value;
use tracing::{debug, warn};

use maki_config::ModelPolicy;
use maki_storage::id::SessionRef;

use crate::model::{Model, ModelInfo};
use crate::model_registry::set_known_models;
use crate::providers::catalog::{
    available_if_warm, catalog_providers, catalog_providers_if_available, try_create,
};
use crate::providers::{KeyRotation, Timeouts, custom, plugin};
use crate::spec::{Owner, ProviderRegistry};
use crate::{AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait Provider: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>>;

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>>;

    /// Fetch provider-side usage quota (remaining percentage / reset times).
    /// `Ok(None)` means the provider does not expose a programmatic usage endpoint.
    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async { Ok(None) })
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async { Ok(()) })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async { Ok(()) })
    }

    /// The keys this provider rotates through, and where the current one lives.
    /// `None` is one fixed credential that never changes. This is the only hook
    /// for rotation: both the count and the swap come off it, so they can never
    /// describe different pools, which is what a per-provider `rotate_key` let
    /// happen before.
    fn keys(&self) -> Option<KeyRotation<'_>> {
        None
    }

    fn adjust_model(&self, _model: &mut Model) {}
}

pub fn provider_for_slug(slug: &str, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    match Owner::of(slug) {
        Owner::Builtin(new) => new(timeouts),
        Owner::Plugin => plugin::create(slug, timeouts),
        Owner::Custom => custom::create(slug, timeouts),
        Owner::Catalog | Owner::Unknown => try_create(slug, timeouts).unwrap_or_else(|| {
            Err(AgentError::Config {
                message: format!("unknown provider '{slug}'"),
            })
        }),
    }
}

pub fn provider_available(slug: &str) -> bool {
    provider_for_slug(slug, Timeouts::default()).is_ok()
}

/// Non-blocking variant of [`provider_available`] for offline model discovery:
/// catalog-backed slugs consult only the already-warm catalog, so a cold cache
/// reports them unavailable instead of blocking on a network fetch.
fn provider_available_offline(slug: &str) -> bool {
    match Owner::of(slug) {
        Owner::Builtin(_) | Owner::Plugin | Owner::Custom => provider_available(slug),
        Owner::Catalog | Owner::Unknown => available_if_warm(slug),
    }
}

pub fn from_model(model: &mut Model, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    let provider = provider_for_slug(&model.provider, timeouts)?;
    provider.adjust_model(model);
    debug!(provider = %model.provider, model = %model.id, "provider created");
    Ok(provider)
}

/// Adjust a model against its provider's static table without retaining the
/// provider. Used to reconcile a resumed model so it matches one started
/// fresh (e.g. inherited thinking support for a routed Aperture model).
pub fn adjust_model(model: &mut Model, timeouts: Timeouts) -> Result<(), AgentError> {
    provider_for_slug(&model.provider, timeouts)?.adjust_model(model);
    Ok(())
}

pub fn from_model_fallback(model: &mut Model, timeouts: Timeouts) -> Box<dyn Provider> {
    match from_model(model, timeouts) {
        Ok(provider) => provider,
        Err(e) => {
            warn!(error = %e, "provider creation failed, using unconfigured provider");
            Box::new(UnconfiguredProvider)
        }
    }
}

struct UnconfiguredProvider;

const NOT_CONFIGURED: &str = "no provider configured — run /login or `maki auth login`";

impl Provider for UnconfiguredProvider {
    fn stream_message<'a>(
        &'a self,
        _model: &'a Model,
        _messages: &'a [Message],
        _system: &'a str,
        _tools: &'a Value,
        _event_tx: &'a Sender<ProviderEvent>,
        _opts: RequestOptions,
        _session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async {
            Err(AgentError::Config {
                message: NOT_CONFIGURED.to_string(),
            })
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async {
            Err(AgentError::Config {
                message: NOT_CONFIGURED.to_string(),
            })
        })
    }
}

pub async fn from_model_async(
    model: &mut Model,
    timeouts: Timeouts,
) -> Result<Box<dyn Provider>, AgentError> {
    let slug = Arc::clone(&model.provider);
    let id = model.id.clone();
    let provider = smol::unblock(move || provider_for_slug(&slug, timeouts)).await?;
    provider.adjust_model(model);
    debug!(provider = %model.provider, model = %id, "provider created");
    Ok(provider)
}

pub struct ModelBatch {
    pub models: Vec<String>,
    pub warnings: Vec<String>,
}

/// Offline version of model discovery: returns specs from static tables
/// and registered plugin providers. See [`fetch_all_models`] for live lookups.
/// Never blocks on catalog download; catalog-backed providers appear only once
/// the catalog has warmed in the background.
pub fn available_model_specs(policy: &ModelPolicy) -> Vec<String> {
    let mut specs: Vec<String> = ProviderRegistry::builtins()
        .iter()
        .filter(|m| provider_available_offline(m.slug))
        .flat_map(|m| {
            m.models()
                .iter()
                .flat_map(|entry| entry.prefixes.iter())
                .map(move |p| format!("{}/{}", m.slug, p))
        })
        .collect();
    for slug in plugin::registered_slugs() {
        specs.extend(plugin::plugin_model_specs_for(&slug));
    }
    for spec in custom::declared_model_specs() {
        if !specs.contains(&spec) {
            specs.push(spec);
        }
    }
    if let Some(catalog) = catalog_providers_if_available() {
        for cat in catalog {
            // Any slug the registry knows was listed above; see
            // `spec::tests::builtins_are_the_native_and_catalog_backed_slugs`.
            if ProviderRegistry::get(&cat.slug).is_some()
                || matches!(Owner::of(&cat.slug), Owner::Plugin | Owner::Custom)
            {
                continue;
            }
            if !provider_available(&cat.slug) {
                continue;
            }
            for model_id in cat.models.keys() {
                let spec = format!("{}/{}", cat.slug, model_id);
                if !specs.contains(&spec) {
                    specs.push(spec);
                }
            }
        }
    }
    specs.retain(|spec| policy.allows(spec));
    specs
}

pub async fn fetch_all_models(
    policy: &ModelPolicy,
    mut on_ready: impl FnMut(ModelBatch),
    on_done: Option<Box<dyn FnOnce() + Send>>,
) {
    let (tx, rx) = flume::unbounded();
    let timeouts = Timeouts::default();

    for spec in ProviderRegistry::builtins() {
        let slug = spec.slug;
        let Ok(provider) = smol::unblock(move || provider_for_slug(slug, timeouts)).await else {
            warn!(provider = slug, "failed to create provider, skipping");
            continue;
        };
        let display_name = spec.display_name;
        let tx = tx.clone();
        smol::spawn(async move {
            let batch = match provider.list_models().await {
                Ok(models) => {
                    let mut specs: Vec<String> =
                        models.iter().map(|m| format!("{slug}/{}", m.id)).collect();
                    set_known_models(slug, models);
                    for entry in spec.models() {
                        for prefix in entry.prefixes {
                            let spec = format!("{slug}/{prefix}");
                            if !specs.contains(&spec) {
                                specs.push(spec);
                            }
                        }
                    }
                    ModelBatch {
                        models: specs,
                        warnings: Vec::new(),
                    }
                }
                Err(e) => {
                    warn!(provider = slug, error = %e, "failed to list models, using static fallback");
                    let fallback: Vec<String> = spec
                        .models()
                        .iter()
                        .flat_map(|entry| entry.prefixes.iter())
                        .map(|p| format!("{slug}/{p}"))
                        .collect();
                    ModelBatch {
                        models: fallback,
                        warnings: vec![format!(
                            "{display_name}: {e} (using static fallback)"
                        )],
                    }
                }
            };
            let _ = tx.send_async(batch).await;
        })
        .detach();
    }

    for slug in plugin::registered_slugs() {
        let tx = tx.clone();
        smol::spawn(async move {
            let static_fallback = |reason: String| {
                warn!(
                    slug,
                    error = reason,
                    "plugin model listing failed, using static fallback"
                );
                ModelBatch {
                    models: plugin::plugin_model_specs_for(&slug),
                    warnings: vec![format!("{slug}: {reason} (using static fallback)")],
                }
            };
            let batch = match plugin::create(&slug, timeouts) {
                Ok(provider) => match provider.list_models().await {
                    Ok(models) => ModelBatch {
                        models: models.iter().map(|m| format!("{slug}/{}", m.id)).collect(),
                        warnings: Vec::new(),
                    },
                    Err(e) => static_fallback(e.to_string()),
                },
                Err(e) => static_fallback(e.to_string()),
            };
            let _ = tx.send_async(batch).await;
        })
        .detach();
    }

    let tx_catalog = tx.clone();
    smol::spawn(async move {
        let catalog = smol::unblock(catalog_providers).await;
        for cat in catalog {
            // No `Owner::Custom` here, unlike `available_model_specs` above:
            // a long-standing asymmetry, changing it is a behaviour change.
            if ProviderRegistry::get(&cat.slug).is_some()
                || matches!(Owner::of(&cat.slug), Owner::Plugin)
            {
                continue;
            }
            if !provider_available(&cat.slug) {
                continue;
            }
            let slug = cat.slug;
            let models: Vec<String> = cat.models.keys().map(|id| format!("{slug}/{id}")).collect();
            let _ = tx_catalog
                .send_async(ModelBatch {
                    models,
                    warnings: Vec::new(),
                })
                .await;
        }
    })
    .detach();

    let tx_custom = tx.clone();
    smol::spawn(async move {
        let declared = custom::declared_model_specs();
        if !declared.is_empty() {
            let _ = tx_custom
                .send_async(ModelBatch {
                    models: declared,
                    warnings: Vec::new(),
                })
                .await;
        }
        let custom_specs = smol::unblock(move || custom::discover_models(timeouts)).await;
        if !custom_specs.is_empty() {
            let _ = tx_custom
                .send_async(ModelBatch {
                    models: custom_specs,
                    warnings: Vec::new(),
                })
                .await;
        }
    })
    .detach();

    drop(tx);

    while let Ok(mut batch) = rx.recv_async().await {
        batch.models.retain(|spec| policy.allows(spec));
        on_ready(batch);
    }
    if let Some(done) = on_done {
        done();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allowed: &[&str], excluded: &[&str]) -> ModelPolicy {
        ModelPolicy::new(
            &allowed
                .iter()
                .map(|pattern| (*pattern).into())
                .collect::<Vec<_>>(),
            &excluded
                .iter()
                .map(|pattern| (*pattern).into())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn available_specs_apply_model_policy() {
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-test-model-policy") };
        let policy = policy(&["openai/*"], &["*/gpt-5.6-terra"]);

        let specs = available_model_specs(&policy);
        unsafe { std::env::remove_var("OPENAI_API_KEY") };

        assert!(!specs.is_empty());
        assert!(specs.iter().all(|spec| spec.starts_with("openai/")));
        assert!(!specs.iter().any(|spec| spec == "openai/gpt-5.6-terra"));
    }

    #[test]
    fn provider_for_slug_unknown_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        crate::providers::catalog::warm_empty_catalog_for_tests(maki_storage::StateDir::from_path(
            tmp.path().to_path_buf(),
        ));
        let result = provider_for_slug("nonexistent-provider-xyz", Timeouts::default());
        match result {
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("unknown provider"),
                    "expected 'unknown provider' message, got: {msg}"
                );
            }
            Ok(_) => panic!("expected error for unknown provider"),
        }
    }
}
