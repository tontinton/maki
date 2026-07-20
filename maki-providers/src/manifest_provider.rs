use std::sync::{Arc, Mutex, OnceLock};

use flume::Sender;
use maki_config::providers::{ProvidersConfig, resolve_base_url};
use maki_storage::id::SessionRef;
use serde_json::Value;
use tracing::debug;

use crate::AgentError;
use crate::auth_source::{AuthSource, EnvAuthSource};
use crate::manifest::{AuthKind, ManifestRegistry};
use crate::model::{Model, ModelInfo};
use crate::provider::{BoxFuture, Provider};
use crate::providers::ResolvedAuth;
use crate::providers::{
    KeyPool, Timeouts, anthropic, deepseek, google, mistral, openrouter, synthetic, tensorx, zai,
};
use crate::{Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse};

pub struct ManifestProvider {
    slug: &'static str,
    auth: Arc<Mutex<ResolvedAuth>>,
    auth_source: Box<dyn AuthSource>,
    engine: OnceLock<Box<dyn Provider>>,
    timeouts: Timeouts,
}

impl ManifestProvider {
    pub fn try_for_slug(
        slug: &str,
        timeouts: Timeouts,
    ) -> Result<Option<Box<dyn Provider>>, AgentError> {
        let Some(manifest) = ManifestRegistry::get(slug) else {
            return Ok(None);
        };
        if !matches!(manifest.auth_kind, AuthKind::Env) {
            return Ok(None);
        }
        let provider =
            build_env(manifest.slug, timeouts)?.map(|p| Box::new(p) as Box<dyn Provider>);
        Ok(provider)
    }

    fn engine(&self) -> Result<&dyn Provider, AgentError> {
        if let Some(engine) = self.engine.get() {
            return Ok(engine.as_ref());
        }
        // Auth is resolved eagerly at construction (build_env today, build_oauth
        // in the OAuth follow-up PR) so the engine reads a populated handle here.
        // AuthSource's reload/refresh/rotate_key mutate the same Arc the engine
        // reads per-request, so no re-resolve is needed at engine-build time.
        let built = build_engine(self.slug, self.auth.clone(), self.timeouts)?;
        if self.engine.set(built).is_err() {
            // Another caller won the race; use its engine.
            return Ok(self
                .engine
                .get()
                .expect("engine set by concurrent init")
                .as_ref());
        }
        debug!(slug = self.slug, "built manifest provider engine");
        Ok(self.engine.get().expect("just set").as_ref())
    }
}

const ENV_ENVELOPED_SLUGS: &[&str] = &[
    "anthropic",
    "google",
    "mistral",
    "deepseek",
    "zai",
    "synthetic",
    "tensorx",
    "openrouter",
];

fn build_env(
    slug: &'static str,
    timeouts: Timeouts,
) -> Result<Option<ManifestProvider>, AgentError> {
    // Partition check: any env builtin not listed here falls through to
    // `provider_for_slug`'s next routing tier. Listing a slug here without a
    // match arm below is an authoring bug (unreachable!), not a missing-key
    // runtime error.
    if !ENV_ENVELOPED_SLUGS.contains(&slug) {
        return Ok(None);
    }
    let auth_source: Box<dyn AuthSource> = match slug {
        "anthropic" => Box::new(EnvAuthSource::new(
            "anthropic",
            anthropic::ENV_VAR,
            anthropic::resolve_auth_from_key,
        )),
        "google" => Box::new(EnvAuthSource::new(
            "google",
            google::ENV_VAR,
            google::resolve_auth_from_key,
        )),
        "mistral" => Box::new(EnvAuthSource::new(
            "mistral",
            mistral::CONFIG.api_key_env,
            ResolvedAuth::bearer,
        )),
        "deepseek" => Box::new(EnvAuthSource::new(
            "deepseek",
            deepseek::CONFIG.api_key_env,
            ResolvedAuth::bearer,
        )),
        "zai" => {
            let config = ProvidersConfig::try_load().map_err(|error| AgentError::Config {
                message: format!("failed to load provider config for 'zai': {error}"),
            })?;
            let base_url = resolve_base_url("zai", config.get("zai"));
            Box::new(EnvAuthSource::new(
                "zai",
                zai::CONFIG_STANDARD.api_key_env,
                move |key| bearer_with_base_url(key, base_url.clone()),
            ))
        }
        "synthetic" => Box::new(EnvAuthSource::new(
            "synthetic",
            synthetic::CONFIG.api_key_env,
            ResolvedAuth::bearer,
        )),
        "tensorx" => Box::new(EnvAuthSource::new(
            "tensorx",
            tensorx::CONFIG.api_key_env,
            ResolvedAuth::bearer,
        )),
        "openrouter" => Box::new(EnvAuthSource::with_resolver(
            "openrouter",
            openrouter::CONFIG.api_key_env,
            ResolvedAuth::bearer,
            env_only,
        )),
        _ => unreachable!("slug matched ENV_ENVELOPED_SLUGS but has no auth_source arm"),
    };
    let auth = Arc::new(Mutex::new(ResolvedAuth {
        base_url: None,
        headers: Vec::new(),
    }));
    // Eager auth resolution preserves the precondition the old per-shim
    // factories enforced: `provider_for_slug` fails fast when no API key is
    // available, so `provider_available` reflects key presence and the setup
    // flow does not surface a confusing error on first stream.
    auth_source.resolve(&auth)?;
    Ok(Some(ManifestProvider {
        slug,
        auth,
        auth_source,
        engine: OnceLock::new(),
        timeouts,
    }))
}

fn env_only(_slug: &'static str, env_var: &'static str) -> Result<KeyPool, AgentError> {
    KeyPool::from_env(env_var)
}

fn bearer_with_base_url(api_key: &str, base_url: Option<String>) -> ResolvedAuth {
    let mut auth = ResolvedAuth::bearer(api_key);
    auth.base_url = base_url;
    auth
}

fn build_engine(
    slug: &str,
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
) -> Result<Box<dyn Provider>, AgentError> {
    match slug {
        "anthropic" => Ok(Box::new(anthropic::Anthropic::with_auth(auth, timeouts))),
        "google" => Ok(Box::new(google::Google::with_auth(auth, timeouts))),
        "mistral" => Ok(Box::new(mistral::Mistral::with_auth(auth, timeouts))),
        "deepseek" => Ok(Box::new(deepseek::DeepSeek::with_auth(auth, timeouts))),
        "zai" => Ok(Box::new(zai::Zai::with_auth(auth, timeouts))),
        "synthetic" => Ok(Box::new(synthetic::Synthetic::with_auth(auth, timeouts))),
        "tensorx" => Ok(Box::new(tensorx::TensorX::with_auth(auth, timeouts))),
        "openrouter" => Ok(Box::new(openrouter::OpenRouter::with_auth(auth, timeouts))),
        other => unreachable!("slug '{other}' reached build_engine but not ENV_ENVELOPED_SLUGS"),
    }
}

impl Provider for ManifestProvider {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let engine = self.engine()?;
            engine
                .stream_message(model, messages, system, tools, event_tx, opts, session_id)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async move {
            let engine = self.engine()?;
            engine.list_models().await
        })
    }

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async move {
            let engine = self.engine()?;
            engine.fetch_usage().await
        })
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move { self.auth_source.refresh(&self.auth) })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move { self.auth_source.reload(&self.auth) })
    }

    fn rotate_key(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async move { self.auth_source.rotate_key(&self.auth) })
    }

    fn adjust_model(&self, model: &mut Model) {
        if let Ok(engine) = self.engine() {
            engine.adjust_model(model);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL_HOST_SLUGS: &[&str] = &["ollama", "llama-cpp", "opencode"];
    const OAUTH_SLUGS: &[&str] = &["openai", "copilot"];

    #[test]
    fn try_for_slug_local_manifests_return_ok_none() {
        for slug in LOCAL_HOST_SLUGS {
            assert!(
                ManifestProvider::try_for_slug(slug, Timeouts::default())
                    .unwrap()
                    .is_none(),
                "{slug} must return Ok(None) so provider_for_slug falls through to ProviderKind::create"
            );
        }
    }

    #[test]
    fn try_for_slug_oauth_manifests_return_ok_none() {
        for slug in OAUTH_SLUGS {
            assert!(
                ManifestProvider::try_for_slug(slug, Timeouts::default())
                    .unwrap()
                    .is_none(),
                "{slug} is OAuth and stays on ProviderKind::create until the OAuth follow-up PR"
            );
        }
    }

    #[test]
    fn every_builtin_manifest_is_in_exactly_one_routing_bucket() {
        for manifest in ManifestRegistry::builtins() {
            let buckets = [
                (
                    "ENV_ENVELOPED",
                    ENV_ENVELOPED_SLUGS.contains(&manifest.slug),
                ),
                ("LOCAL_HOST", LOCAL_HOST_SLUGS.contains(&manifest.slug)),
                ("OAUTH", OAUTH_SLUGS.contains(&manifest.slug)),
            ];
            let count = buckets.iter().filter(|(_, in_bucket)| *in_bucket).count();
            assert_eq!(
                count, 1,
                "manifest slug {:?} appears in {} routing buckets, expected 1",
                manifest.slug, count,
            );
        }
    }

    #[test]
    fn routing_buckets_match_auth_kind_from_manifest() {
        for (bucket_slugs, expected) in [
            (ENV_ENVELOPED_SLUGS, AuthKind::Env),
            (LOCAL_HOST_SLUGS, AuthKind::Env),
            (OAUTH_SLUGS, AuthKind::OAuth),
        ] {
            for slug in bucket_slugs {
                let manifest = ManifestRegistry::get(slug).unwrap_or_else(|| {
                    panic!("routing bucket slug {slug:?} has no ProviderManifest")
                });
                assert_eq!(
                    manifest.auth_kind, expected,
                    "slug {slug:?} in routing bucket but manifest has auth_kind {:?}, expected {:?}",
                    manifest.auth_kind, expected,
                );
            }
        }
    }

    #[test]
    fn try_for_slug_unknown_returns_none() {
        assert!(
            ManifestProvider::try_for_slug("not-a-provider", Timeouts::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn env_enveloped_slugs_have_manifest_and_build_env_arm() {
        // Every slug in ENV_ENVELOPED_SLUGS must have both (a) a ProviderManifest
        // and (b) a match arm in build_env. Without (a) try_for_slug returns
        // Ok(None) before reaching build_env, so the panic check below wouldn't
        // fire; the manifest assertion guards that. Without (b) build_env hits
        // unreachable!() and panics — caught by catch_unwind. The panic fires
        // before resolve runs, so the test is environment-agnostic.
        for slug in ENV_ENVELOPED_SLUGS {
            assert!(
                ManifestRegistry::get(slug).is_some(),
                "slug {slug:?} in ENV_ENVELOPED_SLUGS has no ProviderManifest",
            );
            let result = std::panic::catch_unwind(|| {
                ManifestProvider::try_for_slug(slug, Timeouts::default())
            });
            assert!(
                result.is_ok(),
                "slug {slug:?} in ENV_ENVELOPED_SLUGS panicked build_env \
                 (missing match arm): {:?}",
                result.err(),
            );
        }
    }

    #[test]
    fn env_auth_source_resolve_fails_when_env_missing() {
        let env_var: &'static str =
            Box::leak(format!("MAKI_TEST_AUTH_NONE_{}", fastrand::u32(..)).into_boxed_str());
        let source = EnvAuthSource::new("test", env_var, ResolvedAuth::bearer);
        let auth = Arc::new(Mutex::new(ResolvedAuth {
            base_url: None,
            headers: Vec::new(),
        }));
        let err = source.resolve(&auth).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains(env_var),
            "error must reference missing env var: {msg}"
        );
    }

    #[test]
    fn bearer_with_base_url_preserves_both_values() {
        let auth = bearer_with_base_url("sk-test", Some("https://example.com".into()));
        assert_eq!(
            auth.headers
                .iter()
                .find(|(name, _)| name == "authorization")
                .map(|(_, value)| value.as_str()),
            Some("Bearer sk-test")
        );
        assert_eq!(auth.base_url.as_deref(), Some("https://example.com"));
    }
}
