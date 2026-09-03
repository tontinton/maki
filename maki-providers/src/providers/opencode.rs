use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_storage::id::SessionRef;
use serde_json::Value;

use crate::model::{Model, ModelInfo};
use crate::provider::{BoxFuture, Provider};
use crate::providers::catalog::{
    CatalogMeta, CatalogTransport, EndpointType, FreeTier, ProviderQuirks, config_error,
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
                    .resolve_auth(&guard.state_dir)?
                    .ok_or_else(|| {
                        config_error(format!(
                            "authentication required for provider '{sub_provider}', run `maki auth login {sub_provider}`"
                        ))
                    })?,
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
                max_output_tokens: Some(meta.output),
                context_window: meta.context,
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
