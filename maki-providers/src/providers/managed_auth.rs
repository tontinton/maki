//! Auth state for a provider that takes either an API key or stored OAuth tokens.

use std::future::Future;
use std::sync::{Arc, Mutex};

use maki_storage::StateDir;
use maki_storage::auth::{OAuthTokens, load_tokens_async};
use tokio::task::spawn_blocking;
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

pub(crate) struct ManagedAuth {
    flow: &'static OAuthFlow,
    auth: Arc<Mutex<ResolvedAuth>>,
    /// The stored OAuth tokens `auth` was built from, `None` for an API key.
    tokens: Arc<Mutex<Option<OAuthTokens>>>,
    storage: Option<StateDir>,
}

impl ManagedAuth {
    pub fn from_storage(flow: &'static OAuthFlow) -> Result<Self, AgentError> {
        let storage = StateDir::resolve()?;
        let (resolved, tokens) = (flow.resolve)(&storage)?;
        Ok(Self {
            flow,
            auth: Arc::new(Mutex::new(resolved)),
            tokens: Arc::new(Mutex::new(tokens)),
            storage: Some(storage),
        })
    }

    pub fn externally_managed(flow: &'static OAuthFlow, auth: Arc<Mutex<ResolvedAuth>>) -> Self {
        Self {
            flow,
            auth,
            tokens: Arc::default(),
            storage: None,
        }
    }

    pub fn current(&self) -> ResolvedAuth {
        self.auth.lock().unwrap().clone()
    }

    /// The tokens `current` was built from, as of the last catch-up with the
    /// stored ones.
    pub fn held_tokens(&self) -> Option<OAuthTokens> {
        self.tokens.lock().unwrap().clone()
    }

    /// The tokens on disk now, which a peer may have rotated, cleared or
    /// logged in since this process last looked.
    pub async fn stored_tokens(&self) -> Option<OAuthTokens> {
        load_tokens_async(self.storage.as_ref()?, self.flow.provider).await
    }

    /// Installs the result inside a task of its own, so a dropped caller still
    /// leaves it in use.
    async fn refresh(&self) -> Result<(), AgentError> {
        let storage = self.storage.clone().ok_or_else(|| AgentError::Config {
            message: "OAuth refresh not available for externally-managed auth".into(),
        })?;
        let rejected = self.auth.lock().unwrap().access_token().map(str::to_owned);
        let shared = Arc::clone(&self.auth);
        let held = Arc::clone(&self.tokens);
        let flow = self.flow;
        tokio::spawn(async move {
            match refreshed_tokens(
                &storage,
                flow.provider,
                rejected.as_deref(),
                flow.refresh_tokens,
            )
            .await
            {
                Ok(fresh) => {
                    *shared.lock().unwrap() = (flow.build_resolved)(&fresh)?;
                    *held.lock().unwrap() = Some(fresh);
                    Ok(())
                }
                Err(e) if e.is_retryable() => Err(e),
                Err(e) => {
                    (flow.on_dead_tokens)();
                    if let Ok((fallback, tokens)) = (flow.resolve)(&storage) {
                        *shared.lock().unwrap() = fallback;
                        *held.lock().unwrap() = tokens;
                    }
                    Err(e)
                }
            }
        })
        .await??;
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

    /// Catches up with the stored tokens when the held ones expired or a peer
    /// changed them.
    pub async fn refresh_if_stale(&self) -> Result<(), AgentError> {
        self.uses_oauth().await.map(drop)
    }

    /// Whether the provider is on OAuth tokens. Catches up with the stored
    /// tokens first, like [`Self::refresh_if_stale`].
    pub async fn uses_oauth(&self) -> Result<bool, AgentError> {
        let Some(storage) = self.storage.as_ref() else {
            return Ok(false);
        };
        let stored = self.stored_tokens().await;
        if !needs_refresh(stored.as_ref(), self.tokens.lock().unwrap().as_ref()) {
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
        let Some(storage) = self.storage.clone() else {
            return Ok(());
        };
        let resolve = self.flow.resolve;
        let (resolved, tokens) = spawn_blocking(move || resolve(&storage)).await??;
        *self.auth.lock().unwrap() = resolved;
        *self.tokens.lock().unwrap() = tokens;
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
    const REFRESH_TOKEN: &str = "stored-refresh";
    const API_KEY: &str = "fallback-key";
    const ERROR_MESSAGE: &str = "refresh failed";
    const TRANSIENT_STATUS: u16 = 503;
    const FAR_FUTURE: u64 = u64::MAX;

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
        let auth = ManagedAuth {
            flow,
            auth: Arc::new(Mutex::new(
                ResolvedAuth::bearer(PROVIDER, STORED).expect("auth"),
            )),
            tokens: Arc::new(Mutex::new(Some(tokens))),
            storage: Some(storage),
        };
        (dir, auth)
    }

    #[tokio::test]
    async fn dead_tokens_run_the_hook_and_fall_back() {
        let (_dir, auth) = stored_auth(&DEAD_FLOW);
        auth.refresh().await.expect_err("refresh should fail");
        assert!(DEAD_HOOK_RAN.load(Ordering::Relaxed));
        assert_eq!(auth.current().access_token(), Some(API_KEY));
        assert!(auth.tokens.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn transient_failure_keeps_the_tokens() {
        let (_dir, auth) = stored_auth(&TRANSIENT_FLOW);
        let error = auth.refresh().await.expect_err("refresh should fail");
        assert!(error.is_retryable());
        assert!(!TRANSIENT_HOOK_RAN.load(Ordering::Relaxed));
        assert_eq!(auth.current().access_token(), Some(STORED));
        assert!(auth.tokens.lock().unwrap().is_some());
        assert!(auth.stored_tokens().await.is_some());
    }

    #[tokio::test]
    async fn fresh_stored_tokens_are_oauth() {
        let (_dir, auth) = stored_auth(&TRANSIENT_FLOW);
        assert!(auth.uses_oauth().await.expect("nothing to refresh"));
    }

    #[tokio::test]
    async fn an_api_key_without_stored_tokens_is_not_oauth() {
        let dir = TempDir::new().expect("temp dir");
        let auth = ManagedAuth {
            flow: &TRANSIENT_FLOW,
            auth: Arc::new(Mutex::new(
                ResolvedAuth::bearer(PROVIDER, API_KEY).expect("auth"),
            )),
            tokens: Arc::default(),
            storage: Some(StateDir::from_path(dir.path().to_path_buf())),
        };
        assert!(!auth.uses_oauth().await.expect("nothing to refresh"));
    }
}
