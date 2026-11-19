//! Auth state for a provider that takes either an API key or stored OAuth tokens.

use std::future::Future;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use maki_storage::auth::{OAuthTokens, load_tokens_async};
use maki_storage::{StateDir, blocking, joined};
use tracing::debug;

use super::{ResolvedAuth, UNAUTHORIZED_STATUS, needs_refresh, refreshed_tokens};
use crate::AgentError;
use crate::provider::BoxFuture;

type ResolvedTokens = (ResolvedAuth, Option<OAuthTokens>);

pub(crate) struct OAuthFlow {
    pub provider: &'static str,
    pub refresh_tokens: fn(&OAuthTokens) -> BoxFuture<'_, Result<OAuthTokens, AgentError>>,
    pub build_resolved: fn(&OAuthTokens) -> Result<ResolvedAuth, AgentError>,
    pub resolve: fn(&StateDir) -> Result<ResolvedTokens, AgentError>,
    /// Runs when a refresh failed for good, before falling back to `resolve`.
    pub on_dead_tokens: fn(),
}

/// Replaced as a whole, so nobody sees a bearer next to the tokens it was not
/// built from.
enum Held {
    ApiKey(ResolvedAuth),
    OAuth {
        auth: ResolvedAuth,
        tokens: OAuthTokens,
    },
}

impl Held {
    fn auth(&self) -> &ResolvedAuth {
        match self {
            Self::ApiKey(auth) | Self::OAuth { auth, .. } => auth,
        }
    }

    fn tokens(&self) -> Option<&OAuthTokens> {
        match self {
            Self::ApiKey(_) => None,
            Self::OAuth { tokens, .. } => Some(tokens),
        }
    }
}

impl From<ResolvedTokens> for Held {
    fn from((auth, tokens): ResolvedTokens) -> Self {
        match tokens {
            Some(tokens) => Self::OAuth { auth, tokens },
            None => Self::ApiKey(auth),
        }
    }
}

enum Source {
    Stored {
        held: Arc<ArcSwap<Held>>,
        storage: StateDir,
    },
    /// Written by the Aperture or plugin route that owns it.
    External(Arc<Mutex<ResolvedAuth>>),
}

pub(crate) struct ManagedAuth {
    flow: &'static OAuthFlow,
    source: Source,
}

impl ManagedAuth {
    pub fn from_storage(flow: &'static OAuthFlow) -> Result<Self, AgentError> {
        let storage = StateDir::resolve()?;
        let held = Held::from((flow.resolve)(&storage)?);
        Ok(Self::stored(flow, held, storage))
    }

    fn stored(flow: &'static OAuthFlow, held: Held, storage: StateDir) -> Self {
        Self {
            flow,
            source: Source::Stored {
                held: Arc::new(ArcSwap::from_pointee(held)),
                storage,
            },
        }
    }

    pub fn externally_managed(flow: &'static OAuthFlow, auth: Arc<Mutex<ResolvedAuth>>) -> Self {
        Self {
            flow,
            source: Source::External(auth),
        }
    }

    pub fn current(&self) -> ResolvedAuth {
        match &self.source {
            Source::Stored { held, .. } => held.load().auth().clone(),
            Source::External(auth) => auth.lock().unwrap().clone(),
        }
    }

    /// The tokens `current` was built from, as of the last catch-up with the
    /// stored ones.
    pub fn held_tokens(&self) -> Option<OAuthTokens> {
        match &self.source {
            Source::Stored { held, .. } => held.load().tokens().cloned(),
            Source::External(_) => None,
        }
    }

    /// The tokens on disk now, which a peer may have rotated, cleared or
    /// logged in since this process last looked.
    pub async fn stored_tokens(&self) -> Option<OAuthTokens> {
        let Source::Stored { storage, .. } = &self.source else {
            return None;
        };
        load_tokens_async(storage, self.flow.provider).await
    }

    /// Installs the result inside a task of its own, so a dropped caller still
    /// leaves it in use.
    async fn refresh(&self) -> Result<(), AgentError> {
        let Source::Stored { held, storage } = &self.source else {
            return Err(AgentError::Config {
                message: "OAuth refresh not available for externally-managed auth".into(),
            });
        };
        let rejected = held.load().auth().access_token().map(str::to_owned);
        let (held, storage, flow) = (Arc::clone(held), storage.clone(), self.flow);
        joined(tokio::spawn(async move {
            match refreshed_tokens(
                &storage,
                flow.provider,
                rejected.as_deref(),
                flow.refresh_tokens,
            )
            .await
            {
                Ok(tokens) => {
                    let auth = (flow.build_resolved)(&tokens)?;
                    held.store(Arc::new(Held::OAuth { auth, tokens }));
                    Ok(())
                }
                Err(e) if e.is_retryable() => Err(e),
                Err(e) => {
                    (flow.on_dead_tokens)();
                    if let Ok(fallback) = (flow.resolve)(&storage) {
                        held.store(Arc::new(fallback.into()));
                    }
                    Err(e)
                }
            }
        }))
        .await?;
        debug!(provider = self.flow.provider, "refreshed OAuth token");
        Ok(())
    }

    pub async fn refresh_if_oauth(&self) -> Result<(), AgentError> {
        if self.stored_tokens().await.is_some() {
            self.refresh().await
        } else {
            Ok(())
        }
    }

    /// Whether the provider is on OAuth tokens. Catches up with the stored
    /// tokens first, when the held ones expired or a peer changed them.
    pub async fn uses_oauth(&self) -> Result<bool, AgentError> {
        let Source::Stored { held, storage } = &self.source else {
            return Ok(false);
        };
        let stored = self.stored_tokens().await;
        if !needs_refresh(stored.as_ref(), held.load().tokens()) {
            return Ok(stored.is_some());
        }
        match self.refresh().await {
            Err(e) if !e.is_retryable() => {
                (self.flow.resolve)(storage)
                    .map_err(|e| AgentError::api(UNAUTHORIZED_STATUS, e.to_string()))?;
            }
            result => result?,
        }
        Ok(self.stored_tokens().await.is_some())
    }

    pub async fn with_retry<T, F, Fut>(&self, f: F) -> Result<T, AgentError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<T, AgentError>>,
    {
        let result = f().await;
        if result.as_ref().is_err_and(AgentError::is_auth_error)
            && self.stored_tokens().await.is_some()
        {
            match self.refresh().await {
                Ok(()) => return f().await,
                Err(e) if e.is_retryable() => return Err(e),
                Err(_) => {}
            }
        }
        result
    }

    pub async fn reload(&self) -> Result<(), AgentError> {
        let Source::Stored { held, storage } = &self.source else {
            return Ok(());
        };
        let (resolve, storage) = (self.flow.resolve, storage.clone());
        let resolved = blocking(move || resolve(&storage)).await?;
        held.store(Arc::new(resolved.into()));
        debug!(provider = self.flow.provider, "reloaded auth from storage");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use maki_storage::auth::save_tokens;
    use tempfile::TempDir;

    use super::*;

    const PROVIDER: &str = "managed-auth-test";
    const STORED: &str = "stored-access";
    const REFRESHED: &str = "refreshed-access";
    const REFRESH_TOKEN: &str = "stored-refresh";
    const API_KEY: &str = "fallback-key";
    const ERROR_MESSAGE: &str = "refresh failed";
    const TRANSIENT_STATUS: u16 = 503;
    const REFRESHED_EXPIRES: u64 = u64::MAX;
    const FAR_FUTURE: u64 = REFRESHED_EXPIRES - 1;

    static TRANSIENT_HOOK_RAN: AtomicBool = AtomicBool::new(false);
    static DEAD_HOOK_RAN: AtomicBool = AtomicBool::new(false);

    const TRANSIENT_FLOW: OAuthFlow = OAuthFlow {
        provider: PROVIDER,
        refresh_tokens: |_| {
            Box::pin(async { Err(AgentError::api(TRANSIENT_STATUS, ERROR_MESSAGE)) })
        },
        build_resolved: |tokens| ResolvedAuth::bearer(PROVIDER, &tokens.access),
        resolve: |_| Ok((ResolvedAuth::bearer(PROVIDER, API_KEY)?, None)),
        on_dead_tokens: || TRANSIENT_HOOK_RAN.store(true, Ordering::Relaxed),
    };

    const DEAD_FLOW: OAuthFlow = OAuthFlow {
        refresh_tokens: |_| {
            Box::pin(async { Err(AgentError::api(UNAUTHORIZED_STATUS, ERROR_MESSAGE)) })
        },
        on_dead_tokens: || DEAD_HOOK_RAN.store(true, Ordering::Relaxed),
        ..TRANSIENT_FLOW
    };

    const REFRESHING_FLOW: OAuthFlow = OAuthFlow {
        refresh_tokens: |tokens| {
            let refreshed = OAuthTokens {
                access: REFRESHED.into(),
                expires: REFRESHED_EXPIRES,
                ..tokens.clone()
            };
            Box::pin(async { Ok(refreshed) })
        },
        ..TRANSIENT_FLOW
    };

    fn stored_auth(flow: &'static OAuthFlow) -> (TempDir, ManagedAuth) {
        let dir = TempDir::new().expect("temp dir");
        let storage = StateDir::from_path(dir.path().to_path_buf());
        let tokens = OAuthTokens {
            access: STORED.into(),
            refresh: REFRESH_TOKEN.into(),
            expires: FAR_FUTURE,
            account_id: None,
        };
        save_tokens(&storage, PROVIDER, &tokens).expect("save tokens");
        let auth = ResolvedAuth::bearer(PROVIDER, STORED).expect("auth");
        let held = Held::OAuth { auth, tokens };
        (dir, ManagedAuth::stored(flow, held, storage))
    }

    #[tokio::test]
    async fn dead_tokens_run_the_hook_and_fall_back() {
        let (_dir, auth) = stored_auth(&DEAD_FLOW);
        auth.refresh().await.expect_err("refresh should fail");
        assert!(DEAD_HOOK_RAN.load(Ordering::Relaxed));
        assert_eq!(auth.current().access_token(), Some(API_KEY));
        assert!(auth.held_tokens().is_none());
    }

    #[tokio::test]
    async fn transient_failure_keeps_the_tokens() {
        let (_dir, auth) = stored_auth(&TRANSIENT_FLOW);
        let error = auth.refresh().await.expect_err("refresh should fail");
        assert!(error.is_retryable());
        assert!(!TRANSIENT_HOOK_RAN.load(Ordering::Relaxed));
        assert_eq!(auth.current().access_token(), Some(STORED));
        assert!(auth.held_tokens().is_some());
        assert!(auth.stored_tokens().await.is_some());
    }

    #[tokio::test]
    async fn a_refresh_installs_auth_and_tokens_together() {
        let (_dir, auth) = stored_auth(&REFRESHING_FLOW);
        auth.refresh().await.expect("refresh");
        let stored = auth.stored_tokens().await.expect("stored tokens");
        assert_eq!(stored.access, REFRESHED);
        assert!(!needs_refresh(Some(&stored), auth.held_tokens().as_ref()));
        assert_eq!(auth.current().access_token(), Some(REFRESHED));
    }

    #[tokio::test]
    async fn fresh_stored_tokens_are_oauth() {
        let (_dir, auth) = stored_auth(&TRANSIENT_FLOW);
        assert!(auth.uses_oauth().await.expect("nothing to refresh"));
    }

    #[tokio::test]
    async fn an_api_key_without_stored_tokens_is_not_oauth() {
        let dir = TempDir::new().expect("temp dir");
        let key = ResolvedAuth::bearer(PROVIDER, API_KEY).expect("auth");
        let storage = StateDir::from_path(dir.path().to_path_buf());
        let auth = ManagedAuth::stored(&TRANSIENT_FLOW, Held::ApiKey(key), storage);
        assert!(!auth.uses_oauth().await.expect("nothing to refresh"));
    }
}
