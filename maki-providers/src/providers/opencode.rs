use std::sync::{Arc, Mutex};
use std::time::Duration;

use flume::Sender;
use isahc::{HttpClient, Request};
use maki_storage::id::SessionRef;
use serde_json::{Value, json};
use tracing::debug;

use crate::model::{Model, ModelInfo};
use crate::provider::{BoxFuture, Provider};
use crate::providers::anthropic::shared;
use crate::providers::catalog::{
    CatalogMeta, EndpointType, config_error, init_shared_catalog_if_needed,
};
use crate::providers::http_client;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, dialect};

use super::{ResolvedAuth, user_agent, with_prefix};

const MESSAGES_PATH: &str = "/messages";

static CATALOG_CHAT_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "opencode",
    api_key_env: "",
    base_url: "",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "Opencode (Catalog)",
};

pub struct Opencode {
    client: HttpClient,
    chat_compat: OpenAiCompatProvider,
    auth: Option<Arc<Mutex<ResolvedAuth>>>,
    system_prefix: Option<String>,
    stream_timeout: Duration,
}

impl Opencode {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        Ok(Self {
            client: http_client(timeouts),
            chat_compat: OpenAiCompatProvider::new(&CATALOG_CHAT_CONFIG, timeouts),
            auth: None,
            system_prefix: None,
            stream_timeout: timeouts.stream,
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: super::Timeouts) -> Self {
        Self {
            client: http_client(timeouts),
            chat_compat: OpenAiCompatProvider::new(&CATALOG_CHAT_CONFIG, timeouts),
            auth: Some(auth),
            system_prefix: None,
            stream_timeout: timeouts.stream,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    async fn do_list_models(&self) -> Result<Vec<ModelInfo>, AgentError> {
        let models =
            smol::unblock(move || init_shared_catalog_if_needed().lock().unwrap().all_models())
                .await;
        debug!(
            source = "shared catalog",
            count = models.len(),
            "opencode models listed from local catalog"
        );
        Ok(models)
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_catalog_chat_completions(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        auth: &ResolvedAuth,
        opts: &RequestOptions,
    ) -> Result<StreamResponse, AgentError> {
        let mut body = self.chat_compat.build_body(model, messages, system, tools);
        opts.thinking
            .apply_reasoning_effort(&mut body, &dialect::PREFER_HIGH, model);
        self.chat_compat
            .do_stream(model, &[], &body, event_tx, auth)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_catalog_messages(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        auth: &ResolvedAuth,
        opts: &RequestOptions,
    ) -> Result<StreamResponse, AgentError> {
        let system_blocks = vec![shared::SystemBlock {
            r#type: "text",
            text: system,
            cache_control: Some(shared::EPHEMERAL),
        }];
        let mut body = shared::build_request_body_with_system(
            model,
            messages,
            &system_blocks,
            tools,
            opts.thinking,
        );
        body["model"] = json!(model.id);
        body["stream"] = json!(true);
        let json_body = serde_json::to_vec(&body)?;
        let request = auth
            .configure_request(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "{}{}",
                        auth.base_url.as_deref().unwrap_or(""),
                        MESSAGES_PATH
                    ))
                    .header("user-agent", user_agent())
                    .header("content-type", "application/json")
                    .header("anthropic-version", "2023-06-01"),
            )
            .body(json_body)?;

        debug!(model = %model.id, "sending Anthropic-format request via catalog");

        let response = self.client.send_async(request).await?;
        let status = response.status().as_u16();

        if status == 200 {
            crate::providers::anthropic::parse_sse(response, event_tx, self.stream_timeout).await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }

    async fn lookup(
        &self,
        sub_provider: &str,
        actual_id: &str,
    ) -> Result<(CatalogMeta, EndpointType, ResolvedAuth), AgentError> {
        let sub_provider = sub_provider.to_string();
        let actual_id = actual_id.to_string();
        let auth_override = self.auth.clone();
        smol::unblock(move || {
            let guard = init_shared_catalog_if_needed().lock().unwrap();
            let (meta, provider_data) = guard.lookup(&sub_provider, &actual_id)?;
            let state_dir = &guard.state_dir;
            let auth = provider_data
                .resolve_auth_with_override(auth_override.as_ref(), state_dir)
                .ok_or_else(|| {
                    config_error(format!(
                        "authentication required for provider '{sub_provider}', run `maki auth login {sub_provider}`"
                    ))
                })?;
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
        _session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let model_for_stream = model.clone();

            let model_id = &model_for_stream.id;
            let (sub_provider, actual_id) =
                model_id.split_once('/').unwrap_or(("opencode", model_id));

            let (meta, api_format, auth) = self.lookup(sub_provider, actual_id).await?;

            let mut buf = String::new();
            let system = with_prefix(&self.system_prefix, system, &mut buf);

            let model = Model {
                id: actual_id.to_string(),
                max_output_tokens: Some(meta.output),
                context_window: meta.context,
                ..model_for_stream
            };

            match api_format {
                EndpointType::ChatCompletions => {
                    self.handle_catalog_chat_completions(
                        &model, messages, system, tools, event_tx, &auth, &opts,
                    )
                    .await
                }
                EndpointType::Messages => {
                    self.handle_catalog_messages(
                        &model, messages, system, tools, event_tx, &auth, &opts,
                    )
                    .await
                }
            }
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(self.do_list_models())
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async { Ok(()) })
    }
}
