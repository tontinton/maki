//! Opencode Zen and Opencode Go providers.
//!
//! Both backends read the same models.dev catalog and dispatch each model
//! to either an OpenAI-compatible chat completions endpoint or an
//! Anthropic messages endpoint depending on the provider's `npm` package.
//!
//! They differ only in which entries they admit from the catalog and how
//! they resolve auth, both of which live in [`backend`].

use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use flume::Sender;
use isahc::HttpClient;
use serde_json::Value;
use tracing::warn;

mod backend;
mod catalog;
mod request;

pub(crate) use backend::Backend;

use backend::Catalog;
use request::{GO_CHAT, ZEN_CHAT};

use crate::model::Model;
use crate::provider::{BoxFuture, Provider};
use crate::providers::openai_compat::OpenAiCompatProvider;
use crate::providers::{ResolvedAuth, Timeouts, http_client};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse};

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: "opencode-zen",
    display_name: "Opencode Zen",
    protocol: maki_config::providers::Protocol::Openai,
    default_base_url: "https://opencode.ai/zen/v1",
    default_api_key_env: "OPENCODE_API_KEY",
    default_model: "opencode-zen/claude-sonnet-4-5",
    plans: None,
    login_url: Some("https://opencode.ai/auth"),
    needs_url: false,
});

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: "opencode-go",
    display_name: "Opencode Go",
    protocol: maki_config::providers::Protocol::Openai,
    default_base_url: "https://opencode.ai/zen/go/v1",
    default_api_key_env: "OPENCODE_API_KEY",
    default_model: "opencode-go/deepseek-v4-flash",
    plans: None,
    login_url: Some("https://opencode.ai/auth"),
    needs_url: false,
});

/// One provider for both Zen and Go. Behavior is selected at construction
/// time by [`Backend`]; see [`Opencode::zen`] and [`Opencode::go`].
pub struct Opencode {
    backend: Backend,
    client: HttpClient,
    chat_compat: OpenAiCompatProvider,
    auth: Option<Arc<Mutex<ResolvedAuth>>>,
    system_prefix: Option<String>,
    stream_timeout: Duration,
}

static ZEN_CATALOG: OnceLock<RwLock<Catalog>> = OnceLock::new();
static GO_CATALOG: OnceLock<RwLock<Catalog>> = OnceLock::new();

impl Opencode {
    fn new_impl(
        backend: Backend,
        timeouts: Timeouts,
        auth: Option<Arc<Mutex<ResolvedAuth>>>,
    ) -> Self {
        let slot = match backend {
            Backend::Zen => &ZEN_CATALOG,
            Backend::Go => &GO_CATALOG,
        };
        slot.get_or_init(|| RwLock::new(smol::block_on(backend::build_catalog_async(backend))));
        let empty = slot.get().unwrap().read().unwrap().entries.is_empty();
        if empty {
            warn!(?backend, "opencode catalog is empty — no models available");
        }
        let chat_compat = OpenAiCompatProvider::new(
            match backend {
                Backend::Zen => ZEN_CHAT,
                Backend::Go => GO_CHAT,
            },
            timeouts,
        );
        Self {
            backend,
            client: http_client(timeouts),
            chat_compat,
            auth,
            system_prefix: None,
            stream_timeout: timeouts.stream,
        }
    }

    pub fn zen(timeouts: Timeouts) -> Result<Self, AgentError> {
        Ok(Self::new_impl(Backend::Zen, timeouts, None))
    }

    pub fn go(timeouts: Timeouts) -> Result<Self, AgentError> {
        Ok(Self::new_impl(Backend::Go, timeouts, None))
    }

    pub(crate) fn with_auth(
        backend: Backend,
        auth: Arc<Mutex<ResolvedAuth>>,
        timeouts: Timeouts,
    ) -> Self {
        Self::new_impl(backend, timeouts, Some(auth))
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }
}

fn catalog_for(backend: Backend) -> &'static OnceLock<RwLock<Catalog>> {
    match backend {
        Backend::Zen => &ZEN_CATALOG,
        Backend::Go => &GO_CATALOG,
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
        _session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let model_for_stream = model.clone();
            let backend = self.backend;

            let (meta, auth) = {
                let guard = catalog_for(backend).get().unwrap().read().unwrap();
                backend.lookup(&guard, &model_for_stream.id, self.auth.as_ref())?
            };

            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);

            let actual_id = match backend {
                Backend::Zen => backend.strip_prefix(&model_for_stream.id, &meta.provider_id),
                Backend::Go => model_for_stream.id.clone(),
            };

            let model = Model {
                id: actual_id,
                max_output_tokens: meta.output,
                context_window: meta.context,
                ..model_for_stream
            };

            match meta.api_format {
                catalog::EndpointType::ChatCompletions => {
                    request::chat_completions(
                        &self.chat_compat,
                        &model,
                        messages,
                        system,
                        tools,
                        event_tx,
                        &auth,
                        &opts,
                    )
                    .await
                }
                catalog::EndpointType::Messages => {
                    request::anthropic_messages(
                        &self.client,
                        self.stream_timeout,
                        &model,
                        messages,
                        system,
                        tools,
                        event_tx,
                        &auth,
                        &opts,
                    )
                    .await
                }
            }
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async move {
            let guard = catalog_for(self.backend).get().unwrap().read().unwrap();
            Ok(self.backend.all_models(&guard))
        })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        let backend = self.backend;
        Box::pin(async move {
            let new_catalog = backend::build_catalog_async(backend).await;
            *catalog_for(backend).get().unwrap().write().unwrap() = new_catalog;
            Ok(())
        })
    }

    fn adjust_model(&self, model: &mut Model) {
        let catalog = catalog_for(self.backend).get().unwrap().read().unwrap();
        let meta = self.backend.meta_for(&catalog, &model.id);
        if let Some(meta) = meta {
            model.vision = meta.vision;
        };
    }
}
