use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use flume::Sender;
use serde_json::Value;
use tokio::task::spawn_blocking;
use tokio::time::timeout;
use tracing::{debug, warn};

use maki_config::ModelPolicy;
use maki_storage::id::SessionRef;

use crate::model::{Model, ModelInfo};
use crate::model_registry::{discovery_complete, set_known_models};
use crate::providers::catalog::{
    available_if_warm, catalog_providers, catalog_providers_if_available, try_create,
};
use crate::providers::{KeyRotation, Timeouts, custom, plugin};
use crate::spec::{Owner, ProviderRegistry, ProviderSpec};
use crate::{AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse};

/// A headless run waits for its listing, so a hung endpoint must not hold up
/// the first turn for as long as a stalled stream may take.
const DISCOVERY_WAIT: Duration = Duration::from_secs(10);

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

    /// The models this provider serves right now. A failed listing must return
    /// an error, never a static or cached fallback: discovery reads a clean
    /// answer as proof that a model missing from it is retired, and hides that
    /// model from the picker's recents.
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>>;

    /// The context window the server runs `model` with right now. Only a
    /// server that sizes the window when it loads a model can differ from
    /// discovery, so everyone else says `None`. Asked before every request.
    fn served_context_window<'a>(&'a self, _model: &'a Model) -> BoxFuture<'a, Option<u32>> {
        Box::pin(async { None })
    }

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

const NOT_CONFIGURED: &str = "no provider configured: run /login or `maki auth login`";

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
    let provider = spawn_blocking(move || provider_for_slug(&slug, timeouts)).await??;
    provider.adjust_model(model);
    debug!(provider = %model.provider, model = %id, "provider created");
    Ok(provider)
}

pub struct ModelBatch {
    pub models: Vec<String>,
    /// The providers this batch covers, independent of `ModelPolicy`: the
    /// policy filters `models`, but the picker still needs to know which
    /// providers answered.
    pub providers: Vec<String>,
    pub warnings: Vec<String>,
}

pub(crate) fn provider_of(spec: &str) -> &str {
    spec.split_once('/').map_or(spec, |(provider, _)| provider)
}

fn providers_of(specs: &[String]) -> Vec<String> {
    let mut providers: Vec<String> = Vec::new();
    for provider in specs.iter().map(|spec| provider_of(spec)) {
        if !providers.iter().any(|known| known == provider) {
            providers.push(provider.to_string());
        }
    }
    providers
}

/// The offline twin of [`fetch_all_models`]. It never waits for the catalog
/// download, so catalog-backed providers only show up once the catalog has
/// warmed in the background.
pub fn available_model_specs(policy: &ModelPolicy) -> Vec<String> {
    let mut specs: Vec<String> = ProviderRegistry::all()
        .into_iter()
        .filter(|m| !m.catalog_is_authoritative)
        .filter(|m| provider_available_offline(m.slug))
        .flat_map(|m| {
            m.models()
                .iter()
                .flat_map(|entry| entry.prefixes.iter())
                .map(move |p| format!("{}/{}", m.slug, p))
        })
        .collect();
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

    for spec in ProviderRegistry::all() {
        let slug = spec.slug;
        let Ok(provider) = spawn_blocking(move || provider_for_slug(slug, timeouts))
            .await
            .expect("provider construction panicked")
        else {
            warn!(provider = slug, "failed to create provider, skipping");
            continue;
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            let listed = provider.list_models().await;
            let batch = listing_batch(spec, &listed);
            if let Ok(models) = listed {
                set_known_models(slug, models);
            }
            let _ = tx.send_async(batch).await;
        });
    }

    let tx_catalog = tx.clone();
    tokio::spawn(async move {
        for cat in catalog_providers().await {
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
                    providers: vec![slug],
                    warnings: Vec::new(),
                })
                .await;
        }
    });

    let tx_custom = tx.clone();
    tokio::spawn(async move {
        let (declared, declared_providers) = custom::declared_batch();
        if !declared.is_empty() {
            let _ = tx_custom
                .send_async(ModelBatch {
                    providers: declared_providers,
                    models: declared,
                    warnings: Vec::new(),
                })
                .await;
        }
        let custom_specs = custom::discover_models(timeouts).await;
        if !custom_specs.is_empty() {
            let _ = tx_custom
                .send_async(ModelBatch {
                    providers: providers_of(&custom_specs),
                    models: custom_specs,
                    warnings: Vec::new(),
                })
                .await;
        }
    });

    drop(tx);

    while let Ok(mut batch) = rx.recv_async().await {
        batch.models.retain(|spec| policy.allows(spec));
        on_ready(batch);
    }
    if let Some(done) = on_done {
        done();
    }
}

fn listing_batch(spec: &ProviderSpec, listed: &Result<Vec<ModelInfo>, AgentError>) -> ModelBatch {
    let slug = spec.slug;
    let curated = || {
        spec.models()
            .iter()
            .flat_map(|entry| &entry.prefixes)
            .map(move |prefix| format!("{slug}/{prefix}"))
    };
    let (models, warnings) = match listed {
        Ok(models) => {
            let mut specs: Vec<String> =
                models.iter().map(|m| format!("{slug}/{}", m.id)).collect();
            if !spec.catalog_is_authoritative {
                for row in curated() {
                    if !specs.contains(&row) {
                        specs.push(row);
                    }
                }
            }
            (specs, Vec::new())
        }
        Err(
            e @ AgentError::Api {
                status: 401 | 403, ..
            },
        ) if spec.catalog_is_authoritative => {
            warn!(provider = slug, error = %e, "failed to list models");
            (Vec::new(), vec![format!("{}: {e}", spec.display_name)])
        }
        Err(e) => {
            warn!(provider = slug, error = %e, "failed to list models, using static fallback");
            let warning = format!("{}: {e} (using static fallback)", spec.display_name);
            (curated().collect(), vec![warning])
        }
    };
    ModelBatch {
        models,
        providers: vec![slug.to_string()],
        warnings,
    }
}

/// One provider's share of [`fetch_all_models`], for a provider whose rows can't
/// describe everything it serves. Once anyone has discovered it, nobody asks
/// again. It waits at most [`DISCOVERY_WAIT`] and only fills the registry, it is
/// [`refresh_model`] that reads it back into a model.
pub async fn discover_provider(model: &Model, provider: &dyn Provider) -> Result<(), AgentError> {
    let Some(spec) = ProviderRegistry::get(&model.provider)
        .filter(|spec| spec.accepts_arbitrary_models && !discovery_complete(spec.slug))
    else {
        return Ok(());
    };
    remember_models(spec.slug, provider, DISCOVERY_WAIT).await
}

/// A `Model` is a snapshot of the registry, and discovery can land long after it
/// was built, so a headless session calls this before every turn. If the spec no
/// longer resolves, the old model is still better than none.
pub fn refresh_model(model: &mut Model, provider: &dyn Provider) {
    let spec = model.spec();
    let Ok(mut resolved) = Model::from_spec(&spec) else {
        return;
    };
    provider.adjust_model(&mut resolved);
    if resolved != *model {
        debug!(
            spec = %spec,
            old_window = model.context_window,
            new_window = resolved.context_window,
            old_max_output = ?model.max_output_tokens,
            new_max_output = ?resolved.max_output_tokens,
            "model metadata refreshed from the registry"
        );
        *model = resolved;
    }
}

async fn remember_models(
    slug: &str,
    provider: &dyn Provider,
    wait: Duration,
) -> Result<(), AgentError> {
    let models = timeout(wait, provider.list_models())
        .await
        .map_err(io::Error::from)??;
    set_known_models(slug, models);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::future::{pending, ready};

    use test_case::test_case;

    use super::*;

    const SPEC_WITHOUT_THINKING: &str = "copilot";
    const LANDED_SLUG: &str = "ollama";
    const CURATED_SPEC: &str = "anthropic/claude-sonnet-4-20250514";
    const CURATED_SLUG: &str = "zai";
    const UNREGISTERED_SLUG: &str = "no-such-provider";
    const LISTED_MODEL: &str = "listed-with-thinking";
    const LISTED_WINDOW: u32 = 123_456;
    const ADJUSTED_MAX_OUTPUT: u32 = 654;
    const LISTING_FAILED: &str = "no listing";
    const ONLY_LISTS_MODELS: &str = "only asked for its models";
    const NO_ANSWER_KEEPS_LISTING: &str = "no answer keeps the last listing";
    const ROWS_COVER_IT: &str = "a provider its rows fully describe is never listed";
    const WAIT: Duration = Duration::from_millis(10);

    type Listed = Result<Vec<ModelInfo>, AgentError>;

    struct Listing(fn() -> BoxFuture<'static, Listed>);

    impl Provider for Listing {
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
            unreachable!("{ONLY_LISTS_MODELS}")
        }

        fn list_models(&self) -> BoxFuture<'_, Listed> {
            (self.0)()
        }

        fn adjust_model(&self, model: &mut Model) {
            model.max_output_tokens = Some(ADJUSTED_MAX_OUTPUT);
        }
    }

    fn lists_thinking() -> BoxFuture<'static, Listed> {
        Box::pin(ready(Ok(vec![ModelInfo {
            supports_thinking: Some(true),
            context_window: Some(LISTED_WINDOW),
            ..ModelInfo::id_only(LISTED_MODEL.into())
        }])))
    }

    fn fails() -> BoxFuture<'static, Listed> {
        Box::pin(ready(Err(AgentError::Config {
            message: LISTING_FAILED.into(),
        })))
    }

    fn hangs() -> BoxFuture<'static, Listed> {
        Box::pin(pending())
    }

    fn must_not_list() -> BoxFuture<'static, Listed> {
        unreachable!("{ROWS_COVER_IT}")
    }

    #[tokio::test]
    async fn listing_decides_over_the_spec() {
        let resolve =
            || Model::from_spec(&format!("{SPEC_WITHOUT_THINKING}/{LISTED_MODEL}")).unwrap();
        let mut model = resolve();
        let mut resolved_before_listing = resolve();
        assert!(!model.supports_thinking());

        discover_provider(&model, &Listing(fails))
            .await
            .unwrap_err();
        refresh_model(&mut model, &Listing(fails));
        assert!(!model.supports_thinking());

        discover_provider(&model, &Listing(lists_thinking))
            .await
            .unwrap();
        refresh_model(&mut model, &Listing(lists_thinking));
        assert!(model.supports_thinking());
        assert_eq!(model.context_window, LISTED_WINDOW);
        assert_eq!(model.max_output_tokens, Some(ADJUSTED_MAX_OUTPUT));

        discover_provider(&resolved_before_listing, &Listing(fails))
            .await
            .unwrap();
        refresh_model(&mut resolved_before_listing, &Listing(fails));
        assert_eq!(resolved_before_listing.context_window, LISTED_WINDOW);

        for no_answer in [fails, hangs] {
            remember_models(SPEC_WITHOUT_THINKING, &Listing(no_answer), WAIT)
                .await
                .unwrap_err();
            assert!(resolve().supports_thinking(), "{NO_ANSWER_KEEPS_LISTING}");
        }
    }

    #[tokio::test]
    async fn rows_that_cover_a_provider_need_no_listing() {
        let model = Model::from_spec(CURATED_SPEC).unwrap();
        discover_provider(&model, &Listing(must_not_list))
            .await
            .unwrap();
    }

    #[test_case(LANDED_SLUG ; "provider_that_takes_any_model")]
    #[test_case(CURATED_SLUG ; "provider_with_a_curated_table")]
    fn refresh_model_reads_landed_discovery(slug: &str) {
        let mut model = Model::from_spec(&format!("{slug}/{LISTED_MODEL}")).unwrap();
        set_known_models(
            slug,
            vec![ModelInfo {
                context_window: Some(LISTED_WINDOW),
                ..ModelInfo::id_only(LISTED_MODEL.into())
            }],
        );

        refresh_model(&mut model, &Listing(fails));

        assert_eq!(model.context_window, LISTED_WINDOW);
        assert_eq!(model.max_output_tokens, Some(ADJUSTED_MAX_OUTPUT));
    }

    #[test]
    fn refresh_model_keeps_what_it_cannot_resolve() {
        let unresolvable = Model {
            provider: UNREGISTERED_SLUG.into(),
            ..Model::from_spec(&format!("{LANDED_SLUG}/{LISTED_MODEL}")).unwrap()
        };
        let mut model = unresolvable.clone();

        refresh_model(&mut model, &Listing(fails));

        assert_eq!(model, unresolvable);
    }

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
    fn available_specs_leave_an_authoritative_catalog_to_discovery() {
        unsafe { std::env::set_var("GH_COPILOT_TOKEN", "ghu-test-offline-specs") };
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-test-offline-specs") };
        let specs = available_model_specs(&policy(&["copilot/*", "openai/*"], &[]));
        unsafe { std::env::remove_var("GH_COPILOT_TOKEN") };
        unsafe { std::env::remove_var("OPENAI_API_KEY") };

        assert!(!specs.is_empty());
        assert!(specs.iter().all(|spec| spec.starts_with("openai/")));
    }

    #[test_case("copilot", None, false ; "authoritative_live_answer")]
    #[test_case("copilot", Some(401), false ; "authoritative_auth_failure")]
    #[test_case("copilot", Some(500), true ; "authoritative_other_failure")]
    #[test_case("openai", None, true ; "curated_live_answer")]
    #[test_case("openai", Some(401), true ; "curated_auth_failure")]
    #[test_case("openai", Some(500), true ; "curated_other_failure")]
    fn listing_batch_lists_curated_rows(slug: &str, failed_with: Option<u16>, lists_curated: bool) {
        let spec = ProviderRegistry::get(slug).unwrap();
        let listed = match failed_with {
            None => Ok(vec![ModelInfo::id_only(LISTED_MODEL.into())]),
            Some(status) => Err(AgentError::Api {
                status,
                message: LISTING_FAILED.into(),
                retry_after: None,
            }),
        };
        let models = listing_batch(spec, &listed).models;

        let curated = format!("{slug}/{}", spec.models()[0].prefixes[0]);
        assert_eq!(models.contains(&curated), lists_curated);
        assert_eq!(
            models.contains(&format!("{slug}/{LISTED_MODEL}")),
            failed_with.is_none()
        );
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
