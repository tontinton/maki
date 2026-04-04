use std::sync::{Arc, Mutex};

use maki_storage::DataDir;
use tracing::{debug, warn};

use crate::providers::ResolvedAuth;
use crate::{AgentError, provider::BoxFuture};

use super::super::openai_auth;

#[derive(Clone, Copy)]
enum AuthMode {
    Api,
    OAuth,
    External,
}

pub(crate) struct OpenAiAuthState {
    auth: Arc<Mutex<ResolvedAuth>>,
    storage: Option<DataDir>,
    mode: AuthMode,
}

impl OpenAiAuthState {
    pub(crate) fn new_api() -> Result<Self, AgentError> {
        let storage = DataDir::resolve()?;
        let resolved = openai_auth::resolve_api(&storage)?;
        Ok(Self {
            auth: Arc::new(Mutex::new(resolved)),
            storage: Some(storage),
            mode: AuthMode::Api,
        })
    }

    pub(crate) fn new_oauth() -> Result<Self, AgentError> {
        let storage = DataDir::resolve()?;
        let resolved = openai_auth::resolve_oauth(&storage)?;
        Ok(Self {
            auth: Arc::new(Mutex::new(resolved)),
            storage: Some(storage),
            mode: AuthMode::OAuth,
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>) -> Self {
        Self {
            auth,
            storage: None,
            mode: AuthMode::External,
        }
    }

    pub(crate) fn current_auth(&self) -> ResolvedAuth {
        self.auth.lock().unwrap().clone()
    }

    pub(crate) fn is_oauth(&self) -> bool {
        matches!(self.mode, AuthMode::OAuth)
    }

    pub(crate) async fn refresh_oauth(
        &self,
        provider_name: &'static str,
        validate: fn(&ResolvedAuth) -> Result<(), AgentError>,
    ) -> Result<(), AgentError> {
        let storage = self.storage.clone().ok_or_else(|| AgentError::Config {
            message: "OAuth refresh not available for externally-managed auth".into(),
        })?;
        let resolved = smol::unblock(move || {
            let tokens = maki_storage::auth::load_tokens(&storage, openai_auth::PROVIDER)
                .ok_or_else(|| AgentError::Api {
                    status: 401,
                    message: "OpenAI OAuth tokens not found on disk".into(),
                })?;
            match openai_auth::refresh_tokens(&tokens) {
                Ok(fresh) => {
                    maki_storage::auth::save_tokens(&storage, openai_auth::PROVIDER, &fresh)?;
                    let resolved = openai_auth::build_oauth_resolved(&fresh);
                    validate(&resolved)?;
                    Ok(resolved)
                }
                Err(e) => {
                    warn!(provider = provider_name, error = %e, "OpenAI OAuth refresh failed, clearing stale tokens");
                    let _ = maki_storage::auth::delete_tokens(&storage, openai_auth::PROVIDER);
                    Err(e)
                }
            }
        })
        .await?;
        *self.auth.lock().unwrap() = resolved;
        debug!(provider = provider_name, "refreshed OpenAI OAuth token");
        Ok(())
    }

    pub(crate) async fn reload_auth(&self) -> Result<(), AgentError> {
        let storage = self.storage.clone().ok_or_else(|| AgentError::Config {
            message: "Auth reload not available for externally-managed auth".into(),
        })?;
        let mode = self.mode;
        let resolved = smol::unblock(move || match mode {
            AuthMode::Api => openai_auth::resolve_api(&storage),
            AuthMode::OAuth => openai_auth::resolve_oauth(&storage),
            AuthMode::External => Err(AgentError::Config {
                message: "Auth reload not available for externally-managed auth".into(),
            }),
        })
        .await?;
        *self.auth.lock().unwrap() = resolved;
        Ok(())
    }

    pub(crate) fn refresh_auth_boxed(
        &self,
        provider_name: &'static str,
        validate: fn(&ResolvedAuth) -> Result<(), AgentError>,
    ) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            if self.is_oauth() {
                self.refresh_oauth(provider_name, validate).await
            } else {
                Ok(())
            }
        })
    }

    pub(crate) async fn with_oauth_retry<T, F, Fut>(
        &self,
        provider_name: &'static str,
        validate: fn(&ResolvedAuth) -> Result<(), AgentError>,
        f: F,
    ) -> Result<T, AgentError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, AgentError>>,
    {
        let result = f().await;
        if self.is_oauth()
            && matches!(&result, Err(e) if e.is_auth_error())
            && self.refresh_oauth(provider_name, validate).await.is_ok()
        {
            return f().await;
        }
        result
    }
}

pub(crate) fn accept_auth(_: &ResolvedAuth) -> Result<(), AgentError> {
    Ok(())
}
