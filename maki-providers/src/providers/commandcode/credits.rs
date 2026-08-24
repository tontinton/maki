//! Command Code metered credits, the pay-as-you-go Provider plan.
//!
//! Unlike the token plans in [`super::plan`], this is an ordinary
//! OpenAI-compatible endpoint, so the shared compat layer does the work and
//! this file only supplies the account's key, the per-model reasoning dialect
//! and the catalog that `/provider/v1/models` cannot fully describe.

use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_storage::id::SessionRef;
use serde_json::Value;

use crate::model::{Model, ModelInfo};
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse};

use super::super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::super::{KeyPool, ResolvedAuth, Timeouts};
use super::{CREDITS_BASE_URL, CREDITS_SLUG, ENV_VAR, resolve_auth_from_key, resolve_key_pool};

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: CREDITS_SLUG,
    api_key_env: ENV_VAR,
    base_url: CREDITS_BASE_URL,
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "Command Code Credits",
};

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: CREDITS_SLUG,
    display_name: "Command Code Credits",
    protocol: maki_config::providers::Protocol::Openai,
    default_base_url: CREDITS_BASE_URL,
    default_api_key_env: ENV_VAR,
    default_model: "command-code-credits/claude-opus-5",
    plans: None,
    login_url: Some("https://commandcode.ai/studio/keys"),
    needs_url: false,
});

pub struct CommandCodeCredits {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
}

impl CommandCodeCredits {
    pub fn new(timeouts: Timeouts) -> Result<Self, AgentError> {
        let pool = resolve_key_pool(CREDITS_SLUG)?;
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth: Arc::new(Mutex::new(ResolvedAuth::bearer(pool.current()))),
            key_pool: Some(pool),
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: Timeouts) -> Self {
        Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth,
            key_pool: None,
        }
    }
}

impl Provider for CommandCodeCredits {
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
            let mut body = self.compat.build_body(model, messages, system, tools);
            // Only models the catalog snapshot knows accept an effort, and each
            // accepts its own subset; everything else lets the endpoint choose.
            if let Some(dialect) = super::effort_dialect(&model.id) {
                opts.thinking
                    .apply_reasoning_effort(&mut body, &dialect, model);
            }
            self.compat
                .do_stream(model, &[], &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            // Through the compat layer so a `providers.toml` base URL resolves
            // the same way it does for streaming.
            self.compat
                .fetch_and_parse_models(&auth, super::parse_model)
                .await
        })
    }

    fn adjust_model(&self, model: &mut Model) {
        super::adjust_model(model);
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            let pool = resolve_key_pool(CREDITS_SLUG)?;
            let base_url = self.auth.lock().unwrap().base_url.clone();
            *self.auth.lock().unwrap() = resolve_auth_from_key(pool.current(), base_url);
            Ok(())
        })
    }

    fn rotate_key(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async {
            Ok(self
                .key_pool
                .as_ref()
                .is_some_and(|p| p.rotate_auth(&self.auth, ResolvedAuth::bearer)))
        })
    }
}
