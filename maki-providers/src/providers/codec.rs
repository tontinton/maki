use std::sync::{Arc, Mutex};

use flume::Sender;
use serde_json::Value;

use maki_config::providers::Protocol;
use maki_storage::id::SessionRef;

use super::ResolvedAuth;
use super::Timeouts;
use super::openai::responses;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::model::{Model, ModelInfo};
use crate::provider::{BoxFuture, Provider};
use crate::spec::{ProviderRegistry, ProviderSpec};
use crate::types::ThinkingFallback;
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse};

/// Shared by every slug that speaks openai without being the openai provider:
/// `providers.toml` entries and plugin registrations alike. Each of those
/// resolves its own origin and key, so the fallbacks here stay empty, and
/// `provider_name` is a log label rather than a slug.
static COMPAT_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "",
    api_key_env: "",
    base_url: "",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "codec",
};

/// The native provider a custom or plugin slug borrows its codec and fallbacks
/// from. Resolved through [`ProviderRegistry::get`], never `for_slug`, so the
/// lookup cannot recurse back into here.
pub(crate) fn protocol_spec(protocol: Protocol) -> Option<&'static ProviderSpec> {
    ProviderRegistry::get(match protocol {
        Protocol::Openai | Protocol::OpenaiResponses => super::openai::SLUG,
        Protocol::Anthropic => super::anthropic::SLUG,
        Protocol::Google => super::google::SLUG,
    })
}

/// Applied to the final request body, after the codec built it and after
/// `apply_thinking`, so a hook sees exactly what goes on the wire.
pub trait BodyHook: Send + Sync {
    fn call<'a>(
        &'a self,
        body: Value,
        model: &'a Model,
        opts: RequestOptions,
    ) -> BoxFuture<'a, Result<Value, AgentError>>;
}

/// The one place the protocol -> codec dispatch is spelled out. Every codec
/// here honours `system_prefix` except google, which drops it (see
/// [`super::google`]) and refuses it at registration instead.
pub fn build(
    protocol: Protocol,
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
    build_body: Option<Arc<dyn BodyHook>>,
) -> Box<dyn Provider> {
    match protocol {
        Protocol::Anthropic => Box::new(
            super::anthropic::Anthropic::with_auth(auth, timeouts)
                .with_system_prefix(system_prefix),
        ),
        Protocol::Openai | Protocol::OpenaiResponses => Box::new(CompatProvider {
            compat: OpenAiCompatProvider::new(&COMPAT_CONFIG, timeouts),
            auth,
            protocol,
            system_prefix,
            build_body,
        }),
        Protocol::Google => Box::new(super::google::Google::with_auth(auth, timeouts)),
    }
}

pub(crate) struct CompatProvider {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    protocol: Protocol,
    system_prefix: Option<String>,
    build_body: Option<Arc<dyn BodyHook>>,
}

impl Provider for CompatProvider {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        _session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);

            if self.protocol == Protocol::OpenaiResponses {
                let mut body = responses::build_body(model, messages, system, tools);
                // TODO: wire thinking budget into responses API when llama.cpp supports it
                if let Some(hook) = &self.build_body {
                    body = hook.call(body, model, opts).await?;
                }
                return responses::do_stream(
                    self.compat.client(),
                    model,
                    &body,
                    event_tx,
                    &auth,
                    self.compat.stream_timeout(),
                )
                .await;
            }

            let mut body = self.compat.build_body(model, messages, system, tools);
            opts.thinking
                .apply_thinking(&mut body, model, ThinkingFallback::None);
            if let Some(hook) = &self.build_body {
                body = hook.call(body, model, opts).await?;
            }
            self.compat
                .do_stream(model, &[], &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        let auth = self.auth.lock().unwrap().clone();
        Box::pin(async move { self.compat.do_list_models(&auth).await })
    }
}
