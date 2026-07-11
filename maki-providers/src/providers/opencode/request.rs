//! HTTP request building for catalog models.
//!
//! Each model in the catalog uses one of two API formats:
//! - Chat completions (OpenAI-compatible)
//! - Anthropic messages (only for `@ai-sdk/anthropic` packages)

use std::time::Duration;

use flume::Sender;
use isahc::HttpClient;
use isahc::Request;
use serde_json::{Value, json};
use tracing::debug;

use crate::model::Model;
use crate::providers::ResolvedAuth;
use crate::providers::anthropic::shared;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::providers::user_agent;
use crate::{AgentError, EffortScale, Message, ProviderEvent, RequestOptions, StreamResponse};

const MESSAGES_PATH: &str = "/messages";

pub(super) const ZEN_CHAT: &OpenAiCompatConfig = &OpenAiCompatConfig {
    api_key_env: "",
    base_url: "",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "Opencode Zen",
};

pub(super) const GO_CHAT: &OpenAiCompatConfig = &OpenAiCompatConfig {
    api_key_env: "",
    base_url: "",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "Opencode Go",
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn chat_completions(
    chat: &OpenAiCompatProvider,
    model: &Model,
    messages: &[Message],
    system: &str,
    tools: &Value,
    event_tx: &Sender<ProviderEvent>,
    auth: &ResolvedAuth,
    opts: &RequestOptions,
) -> Result<StreamResponse, AgentError> {
    let mut body = chat.build_body(model, messages, system, tools);
    opts.thinking
        .apply_reasoning_effort(&mut body, EffortScale::PreferHigh);
    chat.do_stream(model, &[], &body, event_tx, auth).await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn anthropic_messages(
    client: &HttpClient,
    stream_timeout: Duration,
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

    let response = client.send_async(request).await?;
    let status = response.status().as_u16();

    if status == 200 {
        crate::providers::anthropic::parse_sse(response, event_tx, stream_timeout).await
    } else {
        Err(AgentError::from_response(response).await)
    }
}
