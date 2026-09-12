use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_storage::id::SessionRef;
use serde_json::Value;

use crate::model::{Model, ModelInfo};
use crate::provider::{BoxFuture, Provider};
use crate::providers::catalog::{
    CatalogMeta, CatalogTransport, EndpointType, FreeTier, ProviderQuirks,
    init_shared_catalog_if_needed,
};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse};

use super::{ResolvedAuth, Timeouts, with_prefix};

/// Zen lives under the bare `opencode` models.dev entry; Go has its own.
pub(crate) const ZEN_SLUG: &str = "opencode";
pub(crate) const GO_SLUG: &str = "opencode-go";
pub(crate) const SLUGS: &[&str] = &[ZEN_SLUG, GO_SLUG];

/// OpenCode asked clients to send one stable ID per conversation and warned
/// that requests without it may start failing:
/// https://github.com/tontinton/maki/issues/935
const SESSION_HEADER: &str = "x-opencode-session";
const PUBLIC_KEY: &str = "public";

pub(crate) const QUIRKS: ProviderQuirks = ProviderQuirks {
    free_tier: Some(FreeTier {
        public_key: PUBLIC_KEY,
        config_slug: ZEN_SLUG,
    }),
    session_header: Some(SESSION_HEADER),
};

/// Plan quotas, served on 401 as well as 429. They clear on a weekly or monthly
/// boundary, so retrying or rotating keys only burns time. OpenCode's genuinely
/// short-lived per-minute cap is `RateLimitError` and is deliberately absent.
const QUOTA_ERROR_TYPES: [&str; 6] = [
    "CreditsError",
    "MonthlyLimitError",
    "UserLimitError",
    "FreeUsageLimitError",
    "GoUsageLimitError",
    "BlackUsageLimitError",
];

/// Types that arrive on 401 without the token being stale: `ModelError` covers
/// "trial ended", "model disabled" and "no provider available", none of which a
/// fresh login fixes and none of which is a usage cap.
const NON_LOGIN_ERROR_TYPES: [&str; 1] = ["ModelError"];

pub(crate) const QUOTA_FALLBACK_MESSAGE: &str = "provider usage limit reached";
pub(crate) const NON_LOGIN_FALLBACK_MESSAGE: &str = "model unavailable, check your OpenCode plan";

pub struct Opencode {
    transport: CatalogTransport,
    auth: Option<Arc<Mutex<ResolvedAuth>>>,
    system_prefix: Option<String>,
}

impl Opencode {
    pub fn new(timeouts: Timeouts) -> Self {
        Self {
            transport: CatalogTransport::new(timeouts),
            auth: None,
            system_prefix: None,
        }
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: Timeouts) -> Self {
        Self {
            auth: Some(auth),
            ..Self::new(timeouts)
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    async fn do_list_models(&self) -> Result<Vec<ModelInfo>, AgentError> {
        Ok(smol::unblock(move || {
            let guard = init_shared_catalog_if_needed().lock().unwrap();
            guard.provider(ZEN_SLUG).map_or_else(Vec::new, |data| {
                data.available_models(&guard.state_dir, data.free_models_enabled())
            })
        })
        .await)
    }

    async fn lookup(
        &self,
        sub_provider: &str,
        actual_id: &str,
        session_id: Option<&SessionRef>,
    ) -> Result<(CatalogMeta, EndpointType, ResolvedAuth), AgentError> {
        let sub_provider = sub_provider.to_string();
        let actual_id = actual_id.to_string();
        let session_id = session_id.cloned();
        let auth_override = self.auth.clone();
        smol::unblock(move || {
            let guard = init_shared_catalog_if_needed().lock().unwrap();
            let (meta, provider_data) = guard.lookup(&sub_provider, &actual_id)?;
            let override_auth = auth_override
                .filter(|_| SLUGS.contains(&provider_data.slug.as_str()))
                .map(|auth| {
                    let mut auth = auth.lock().unwrap().clone();
                    if auth.base_url.is_none() {
                        auth.base_url = provider_data.base_url.clone();
                    }
                    auth
                });
            let auth = match override_auth {
                Some(auth) => auth,
                None => provider_data
                    .catalog_auth(&guard.state_dir, provider_data.free_models_enabled())?
                    .unlocked(&sub_provider)?
                    .clone(),
            };
            let auth = provider_data.request_auth(auth, session_id.as_ref());
            Ok((meta.clone(), provider_data.api_format, auth))
        })
        .await
    }
}

impl Provider for Opencode {
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
            let (sub_provider, actual_id) =
                model.id.split_once('/').unwrap_or((ZEN_SLUG, &model.id));

            let (meta, api_format, auth) = self.lookup(sub_provider, actual_id, session_id).await?;

            let mut buf = String::new();
            let system = with_prefix(&self.system_prefix, system, &mut buf);

            let stream_model = Model {
                id: actual_id.to_string(),
                // `..model` carries the agent's turn budget, which
                // [`Model::output_tokens`] clamps to this cap on read.
                max_output_tokens: Some(meta.max_output()),
                context_window: meta.context_window(),
                ..model.clone()
            };

            self.transport
                .stream(
                    api_format,
                    &stream_model,
                    messages,
                    system,
                    tools,
                    event_tx,
                    &auth,
                    &opts,
                    maki_config::providers::resolve_top_p(
                        maki_config::providers::ProvidersConfig::load().get(sub_provider),
                    ),
                )
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(self.do_list_models())
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async { Ok(()) })
    }
}

/// An OpenCode failure that re-authenticating cannot clear.
pub(crate) struct NonLoginError {
    pub(crate) message: String,
    /// Plan quota, so also not worth a retry or a key rotation.
    pub(crate) is_quota: bool,
}

/// OpenCode wraps billing and plan failures in `{"error":{"type","message"}}`
/// and serves them on the same statuses as a genuinely expired token, so the
/// shared error type has to ask us before it tells the user to log in again.
pub(crate) fn non_login_error(status: u16, body: &str) -> Option<NonLoginError> {
    if !matches!(status, 401 | 429) {
        return None;
    }
    let body: Value = serde_json::from_str(body).ok()?;
    let error_type = body.pointer("/error/type")?.as_str()?;
    let is_quota = QUOTA_ERROR_TYPES.contains(&error_type);
    if !is_quota && !NON_LOGIN_ERROR_TYPES.contains(&error_type) {
        return None;
    }
    let fallback = if is_quota {
        QUOTA_FALLBACK_MESSAGE
    } else {
        NON_LOGIN_FALLBACK_MESSAGE
    };
    Some(NonLoginError {
        message: body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .filter(|message| !message.trim().is_empty())
            .unwrap_or(fallback)
            .to_owned(),
        is_quota,
    })
}
