use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_storage::id::SessionRef;
use serde_json::{Value, json};

use crate::model::Model;
use crate::provider::{BoxFuture, Provider};
use crate::providers::ResolvedAuth;
use crate::providers::managed_auth::ManagedAuth;
use crate::providers::openai::responses;
use crate::providers::openai_compat::{
    DEFAULT_MAX_TOKENS_FIELD, OpenAiCompatConfig, OpenAiCompatProvider,
};
use crate::{
    AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse, dialect,
};

use super::{auth, catalog};

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: Cow::Borrowed(super::SLUG),
    api_key_env: Cow::Borrowed(auth::API_KEY_ENV),
    base_url: Cow::Borrowed("https://api.x.ai/v1"),
    max_tokens_field: Cow::Borrowed(DEFAULT_MAX_TOKENS_FIELD),
    include_stream_usage: true,
    usage_cost: false,
    provider_name: Cow::Borrowed("xAI"),
};

const ENCRYPTED_REASONING: &str = "reasoning.encrypted_content";

pub struct Xai {
    compat: OpenAiCompatProvider,
    auth: ManagedAuth,
    system_prefix: Option<String>,
}

impl Xai {
    pub fn new(timeouts: crate::providers::Timeouts) -> Result<Self, AgentError> {
        Ok(Self {
            auth: ManagedAuth::from_storage(&auth::OAUTH_FLOW)?,
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<ResolvedAuth>>,
        timeouts: crate::providers::Timeouts,
    ) -> Self {
        Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth: ManagedAuth::externally_managed(&auth::OAUTH_FLOW, auth),
            system_prefix: None,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }
}

fn apply_grok_reasoning(body: &mut Value, opts: &RequestOptions, model: &Model) {
    if !model.supports_thinking() {
        return;
    }
    if let Some(effort) = opts.thinking.effort_str(&dialect::GROK, model) {
        body["reasoning"] = json!({ "effort": effort });
    }
    let include = body["include"].as_array_mut();
    match include {
        Some(arr) => {
            if !arr.iter().any(|v| v.as_str() == Some(ENCRYPTED_REASONING)) {
                arr.push(json!(ENCRYPTED_REASONING));
            }
        }
        None => body["include"] = json!([ENCRYPTED_REASONING]),
    }
}

fn proxy_request_headers(model: &Model, session_id: Option<&SessionRef>) -> Vec<(String, String)> {
    let session = session_id
        .map(ToString::to_string)
        .unwrap_or_else(random_id);
    vec![
        ("accept".into(), "text/event-stream".into()),
        ("x-grok-conv-id".into(), session.clone()),
        ("x-grok-session-id".into(), session),
        ("x-grok-req-id".into(), random_id()),
        (
            "x-grok-model-override".into(),
            model.id.to_ascii_lowercase(),
        ),
    ]
}

fn random_id() -> String {
    format!("{:016x}{:016x}", fastrand::u64(..), fastrand::u64(..))
}

impl Provider for Xai {
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
            self.auth.refresh_if_stale().await?;
            let mut buf = String::new();
            let system = super::super::with_prefix(&self.system_prefix, system, &mut buf);

            if self.auth.is_oauth() {
                let mut body = responses::build_body(model, messages, system, tools);
                apply_grok_reasoning(&mut body, &opts, model);
                if let Some(session) = session_id {
                    body["prompt_cache_key"] = json!(session.as_str());
                }
                let stream_timeout = self.compat.stream_timeout();
                return self
                    .auth
                    .with_retry(|| async {
                        let mut auth = self.auth.current();
                        auth.headers
                            .extend(proxy_request_headers(model, session_id));
                        responses::do_stream(
                            self.compat.client(),
                            model,
                            &body,
                            event_tx,
                            &auth,
                            stream_timeout,
                        )
                        .await
                    })
                    .await;
            }

            let top_p = self.auth.current().top_p;
            let mut body =
                self.compat
                    .build_body(model, messages, system, tools, opts.thinking, top_p);
            opts.thinking
                .apply_reasoning_effort(&mut body, &dialect::GROK, model);
            self.auth
                .with_retry(|| async {
                    let auth = self.auth.current();
                    self.compat
                        .do_stream(model, &[], &body, event_tx, &auth)
                        .await
                })
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async {
            self.auth.refresh_if_stale().await?;
            if self.auth.is_oauth() {
                return self
                    .auth
                    .with_retry(|| async {
                        let auth = self.auth.current();
                        let access = bearer_token(&auth).ok_or_else(|| AgentError::Config {
                            message: "xAI OAuth token missing from resolved auth".into(),
                        })?;
                        catalog::list_models(&access, false).await
                    })
                    .await;
            }
            self.auth
                .with_retry(|| async {
                    let auth = self.auth.current();
                    self.compat.do_list_models(&auth).await
                })
                .await
        })
    }

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async { Ok(None) })
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(self.auth.refresh_if_oauth())
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(self.auth.reload())
    }

    fn adjust_model(&self, model: &mut Model) {
        let Some(cached) = catalog::cached_model(&model.id) else {
            return;
        };
        model.context_window = cached.context_window;
        model.max_output_tokens = Some(cached.max_tokens);
        if cached.pricing.input > 0.0 || cached.pricing.output > 0.0 {
            model.pricing.input = cached.pricing.input;
            model.pricing.output = cached.pricing.output;
            model.pricing.cache_write = cached.pricing.cache_write;
            model.pricing.cache_read = cached.pricing.cache_read;
        }
        model.supports_vision_override = Some(cached.vision);
        model.thinking_override = Some(if cached.reasoning {
            crate::model::ThinkingSupport::Yes
        } else {
            crate::model::ThinkingSupport::No
        });
    }
}

fn bearer_token(auth: &ResolvedAuth) -> Option<String> {
    auth.headers.iter().find_map(|(key, value)| {
        if !key.eq_ignore_ascii_case("authorization") {
            return None;
        }
        value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))
            .map(ToOwned::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ThinkingConfig;
    use crate::{ModelFamily, ModelPricing, ModelTier};

    fn test_model(thinking: bool) -> Model {
        Model {
            id: "grok-4.6".into(),
            provider: "xai".into(),
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            supports_tool_examples_override: None,
            thinking_override: Some(if thinking {
                crate::model::ThinkingSupport::Yes
            } else {
                crate::model::ThinkingSupport::No
            }),
            supports_vision_override: Some(true),
            supports_fast_override: None,
            pricing: ModelPricing::ZERO,
            subsidised_by: None,
            discovered_free: false,
            max_output_tokens: Some(131_072),
            turn_output_tokens: None,
            context_window: 500_000,
            thinking_fields: None,
        }
    }

    #[test]
    fn grok_reasoning_sets_effort_and_include() {
        let model = test_model(true);
        let mut body = json!({"model": "grok-4.6"});
        apply_grok_reasoning(
            &mut body,
            &RequestOptions {
                thinking: ThinkingConfig::Effort(maki_storage::sessions::Effort::High),
                ..RequestOptions::default()
            },
            &model,
        );
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["include"][0], ENCRYPTED_REASONING);
    }

    #[test]
    fn grok_reasoning_skipped_when_model_has_no_thinking() {
        let model = test_model(false);
        let mut body = json!({"model": "grok-4.6"});
        apply_grok_reasoning(&mut body, &RequestOptions::default(), &model);
        assert!(body.get("reasoning").is_none());
        assert!(body.get("include").is_none());
    }

    #[test]
    fn bearer_token_extracts_access() {
        let auth = ResolvedAuth::for_test(
            None,
            vec![("authorization".into(), "Bearer tok-123".into())],
        );
        assert_eq!(bearer_token(&auth).as_deref(), Some("tok-123"));
    }
}
