use std::sync::{Arc, Mutex};
use std::time::Instant;

use flume::Sender;
use futures_lite::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::model::Model;
use crate::provider::{BoxFuture, Provider, ProviderKind};
use crate::providers::{ResolvedAuth, SSE_TIMEOUT, http_client, next_sse_line};
use crate::{
    AgentError, ContentBlock, Message, ProviderEvent, Role, StopReason, StreamResponse,
    ThinkingConfig, TokenUsage,
};

use super::auth_state::OpenAiAuthState;
use super::plan_models;
use super::{effective_system, plan_codex_cli_version};

const MODELS_PATH: &str = "/models";
const RESPONSES_PATH: &str = "/responses";
const HEADER_ORIGINATOR: &str = "originator";
const HEADER_SESSION_ID: &str = "session_id";
const HEADER_CLIENT_REQUEST_ID: &str = "x-client-request-id";
const ORIGINATOR: &str = "maki.sh";
const STREAM_DONE: &str = "[DONE]";
const INCLUDE_REASONING: &str = "reasoning.encrypted_content";
const AUTHORIZATION_ERROR: &str = "OpenAI Coding Plan OAuth token missing authorization header";
const ACCOUNT_ID_ERROR: &str = concat!(
    "OpenAI Coding Plan requires a stored ChatGPT account id. ",
    "Please log in again."
);

// This transport follows the ChatGPT Coding Plan path used by public Codex
// implementations rather than the OpenAI Platform API. The shape here was
// derived from the official Codex auth docs plus the open-source `openai/codex`
// and `anomalyco/opencode` implementations that use ChatGPT OAuth auth and the
// `/backend-api/codex/responses` transport after login.
pub struct OpenAiCodingPlan {
    client: HttpClient,
    auth_state: OpenAiAuthState,
    system_prefix: Option<String>,
}

impl OpenAiCodingPlan {
    pub fn new() -> Result<Self, AgentError> {
        let auth_state = OpenAiAuthState::new_oauth()?;
        validate_auth(&auth_state.current_auth())?;
        Ok(Self {
            client: http_client(),
            auth_state,
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>) -> Result<Self, AgentError> {
        validate_auth(&auth.lock().unwrap())?;
        Ok(Self {
            client: http_client(),
            auth_state: OpenAiAuthState::with_auth(auth),
            system_prefix: None,
        })
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    fn current_auth(&self) -> ResolvedAuth {
        self.auth_state.current_auth()
    }
}

impl Provider for OpenAiCodingPlan {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        thinking: ThinkingConfig,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let session_id = Uuid::new_v4().to_string();
            let request_id = Uuid::new_v4().to_string();
            let effective_system = effective_system(&self.system_prefix, system);
            let body = build_body(model, messages, &effective_system, tools, thinking);
            self.auth_state
                .with_oauth_retry("OpenAI Coding Plan", validate_auth, || async {
                    let auth = self.current_auth();
                    do_stream(
                        &self.client,
                        model,
                        &body,
                        event_tx,
                        &auth,
                        &session_id,
                        &request_id,
                    )
                    .await
                })
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(async {
            self.auth_state
                .with_oauth_retry("OpenAI Coding Plan", validate_auth, || async {
                    let auth = self.current_auth();
                    do_list_models(&self.client, &auth).await
                })
                .await
        })
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        self.auth_state
            .refresh_auth_boxed("OpenAI Coding Plan", validate_auth)
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            self.auth_state.reload_auth().await?;
            validate_auth(&self.current_auth())?;
            Ok(())
        })
    }
}

fn validate_auth(auth: &ResolvedAuth) -> Result<(), AgentError> {
    if !auth.headers.iter().any(|(key, _)| key == "authorization") {
        return Err(AgentError::Config {
            message: AUTHORIZATION_ERROR.into(),
        });
    }
    if !auth
        .headers
        .iter()
        .any(|(key, _)| key == "chatgpt-account-id")
    {
        return Err(AgentError::Config {
            message: ACCOUNT_ID_ERROR.into(),
        });
    }
    Ok(())
}

fn build_body(
    model: &Model,
    messages: &[Message],
    system: &str,
    tools: &Value,
    thinking: ThinkingConfig,
) -> Value {
    let mut body = json!({
        "model": model.id,
        "instructions": system,
        "input": convert_messages(messages),
        "tools": convert_tools(tools),
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "store": false,
        "stream": true,
    });
    if thinking.is_enabled() {
        body["include"] = json!([INCLUDE_REASONING]);
    }
    body
}

fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut input = Vec::new();

    for message in messages {
        match message.role {
            Role::User => {
                let mut content = Vec::new();

                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            content.push(json!({
                                "type": "input_text",
                                "text": text,
                            }));
                        }
                        ContentBlock::Image { source } => {
                            content.push(json!({
                                "type": "input_image",
                                "image_url": source.to_data_url(),
                            }));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            input.push(json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": content,
                            }));
                        }
                        ContentBlock::Text { .. } => {}
                        ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. }
                        | ContentBlock::ToolUse { .. } => {}
                    }
                }

                if !content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": content,
                    }));
                }
            }
            Role::Assistant => {
                let mut content = Vec::new();

                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            content.push(json!({
                                "type": "output_text",
                                "text": text,
                            }));
                        }
                        ContentBlock::ToolUse {
                            id,
                            name,
                            input: args,
                        } => {
                            input.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": args.to_string(),
                            }));
                        }
                        ContentBlock::Text { .. } => {}
                        ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. }
                        | ContentBlock::ToolResult { .. }
                        | ContentBlock::Image { .. } => {}
                    }
                }

                if !content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": content,
                    }));
                }
            }
        }
    }

    input
}

fn convert_tools(tools: &Value) -> Value {
    let Some(tools) = tools.as_array() else {
        return json!([]);
    };

    Value::Array(
        tools
            .iter()
            .filter_map(|tool| {
                Some(json!({
                    "type": "function",
                    "name": tool.get("name")?,
                    "description": tool.get("description")?,
                    "parameters": tool.get("input_schema")?,
                }))
            })
            .collect(),
    )
}

async fn do_stream(
    client: &HttpClient,
    model: &Model,
    body: &Value,
    event_tx: &Sender<ProviderEvent>,
    auth: &ResolvedAuth,
    session_id: &str,
    request_id: &str,
) -> Result<StreamResponse, AgentError> {
    let json_body = serde_json::to_vec(body)?;
    let request = build_request(body, auth, session_id, request_id)?.body(json_body)?;

    debug!(model = %model.id, "sending OpenAI Coding Plan request");

    let response = client.send_async(request).await?;
    if response.status().as_u16() != 200 {
        return Err(AgentError::from_response(response).await);
    }

    parse_sse(BufReader::new(response.into_body()), event_tx).await
}

async fn do_list_models(
    client: &HttpClient,
    auth: &ResolvedAuth,
) -> Result<Vec<String>, AgentError> {
    let base_url = auth
        .base_url
        .as_deref()
        .unwrap_or(ProviderKind::OpenAiCodingPlan.base_url());
    let request = build_models_request(auth)?.body(())?;
    let mut response = client.send_async(request).await?;
    if response.status().as_u16() != 200 {
        return Err(AgentError::from_response(response).await);
    }

    let body = response.text().await?;
    plan_models::list_remote_models(base_url, &body)
}

fn build_request(
    _body: &Value,
    auth: &ResolvedAuth,
    session_id: &str,
    request_id: &str,
) -> Result<isahc::http::request::Builder, AgentError> {
    let base_url = auth
        .base_url
        .as_deref()
        .unwrap_or(ProviderKind::OpenAiCodingPlan.base_url());
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("{base_url}{RESPONSES_PATH}"))
        .header("content-type", "application/json")
        // These header names are part of the Codex/ChatGPT transport shape.
        // `originator` identifies the client implementation, while `session_id`
        // and `x-client-request-id` follow Codex request metadata conventions.
        // The generated UUID values are Maki-specific; the header names are not.
        .header(HEADER_ORIGINATOR, ORIGINATOR)
        .header(HEADER_SESSION_ID, session_id)
        .header(HEADER_CLIENT_REQUEST_ID, request_id);

    for (key, value) in &auth.headers {
        builder = builder.header(key.as_str(), value.as_str());
    }

    Ok(builder)
}

fn build_models_request(auth: &ResolvedAuth) -> Result<isahc::http::request::Builder, AgentError> {
    let base_url = auth
        .base_url
        .as_deref()
        .unwrap_or(ProviderKind::OpenAiCodingPlan.base_url());
    let codex_cli_version = plan_codex_cli_version()?;
    let mut builder = Request::builder().method("GET").uri(format!(
        "{base_url}{MODELS_PATH}?client_version={codex_cli_version}"
    ));

    for (key, value) in &auth.headers {
        builder = builder.header(key.as_str(), value.as_str());
    }

    Ok(builder)
}

#[derive(Deserialize)]
struct ResponseEnvelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    item: Option<Value>,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    response: Option<ResponseCompleted>,
    #[serde(default)]
    summary_index: Option<i64>,
    #[serde(default)]
    content_index: Option<i64>,
}

#[derive(Deserialize)]
struct ResponseCompleted {
    #[serde(default)]
    usage: Option<ResponseUsage>,
}

#[derive(Deserialize)]
struct ResponseUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    input_tokens_details: Option<ResponseInputTokensDetails>,
}

#[derive(Deserialize)]
struct ResponseInputTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

pub async fn parse_sse(
    reader: impl AsyncBufRead + Unpin,
    event_tx: &Sender<ProviderEvent>,
) -> Result<StreamResponse, AgentError> {
    let mut lines = reader.lines();
    let mut deadline = Instant::now() + SSE_TIMEOUT;
    let mut text = String::new();
    let mut thinking = String::new();
    let mut tool_blocks = Vec::new();
    let mut usage = TokenUsage::default();

    while let Some(line) = next_sse_line(&mut lines, &mut deadline).await? {
        let data = match line.strip_prefix("data: ") {
            Some(data) => data.trim(),
            None => continue,
        };

        if data == STREAM_DONE {
            break;
        }

        let event: ResponseEnvelope = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(err) => {
                warn!(error = %err, "failed to parse OpenAI Coding Plan SSE event");
                continue;
            }
        };

        match event.kind.as_str() {
            "response.output_text.delta" => {
                if let Some(delta) = event.delta
                    && !delta.is_empty()
                {
                    text.push_str(&delta);
                    event_tx
                        .send_async(ProviderEvent::TextDelta { text: delta })
                        .await?;
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = event.delta
                    && !delta.is_empty()
                {
                    thinking.push_str(&delta);
                    event_tx
                        .send_async(ProviderEvent::ThinkingDelta { text: delta })
                        .await?;
                }
            }
            "response.output_item.done" => {
                if let Some(item) = event.item {
                    parse_output_item(item, &mut text, &mut tool_blocks, event_tx).await?;
                }
            }
            "response.completed" => {
                if let Some(response) = event.response
                    && let Some(event_usage) = response.usage
                {
                    let cached = event_usage
                        .input_tokens_details
                        .map(|details| details.cached_tokens)
                        .unwrap_or(0);
                    usage = TokenUsage {
                        input: event_usage.input_tokens.saturating_sub(cached),
                        output: event_usage.output_tokens,
                        cache_creation: 0,
                        cache_read: cached,
                    };
                }
            }
            "response.failed" | "response.incomplete" => {
                return Err(AgentError::Api {
                    status: 400,
                    message: data.to_string(),
                });
            }
            _ => {
                let _ = event.summary_index;
                let _ = event.content_index;
            }
        }
    }

    let mut content = Vec::new();
    if !thinking.is_empty() {
        content.push(ContentBlock::Thinking {
            thinking,
            signature: None,
        });
    }
    if !text.is_empty() {
        content.push(ContentBlock::Text { text });
    }
    let has_tool_calls = !tool_blocks.is_empty();
    content.extend(tool_blocks);

    Ok(StreamResponse {
        message: Message {
            role: Role::Assistant,
            content,
            ..Default::default()
        },
        usage,
        stop_reason: Some(if has_tool_calls {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        }),
    })
}

async fn parse_output_item(
    item: Value,
    text: &mut String,
    tool_blocks: &mut Vec<ContentBlock>,
    event_tx: &Sender<ProviderEvent>,
) -> Result<(), AgentError> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => {
            let output_text = item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("output_text"))
                .filter_map(|entry| entry.get("text").and_then(Value::as_str))
                .collect::<String>();
            if text.is_empty() && !output_text.is_empty() {
                text.push_str(&output_text);
            }
        }
        Some("function_call") => {
            let id = item["call_id"]
                .as_str()
                .ok_or_else(|| AgentError::Api {
                    status: 400,
                    message: "Codex function call missing call_id".into(),
                })?
                .to_string();
            let name = item["name"]
                .as_str()
                .ok_or_else(|| AgentError::Api {
                    status: 400,
                    message: "Codex function call missing name".into(),
                })?
                .to_string();
            event_tx
                .send_async(ProviderEvent::ToolUseStart {
                    id: id.clone(),
                    name: name.clone(),
                })
                .await?;
            let input = item["arguments"]
                .as_str()
                .map(serde_json::from_str)
                .transpose()?
                .unwrap_or_else(|| json!({}));
            tool_blocks.push(ContentBlock::ToolUse { id, name, input });
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelFamily, ModelPricing, ModelTier};
    use crate::providers::openai::{
        PLAN_CONFIG_NOT_INITIALIZED_ERROR, plan_test_lock, reset_plan_codex_cli_version,
    };
    use crate::set_openai_plan_codex_cli_version;
    use futures_lite::io::Cursor;
    use serde_json::{Value, json};
    use std::sync::MutexGuard;
    use test_case::test_case;

    const AUTHORIZATION: &str = "Bearer token";
    const ACCOUNT_ID: &str = "acc_123";
    const TEST_CODEX_CLI_VERSION: &str = "0.0.0";

    fn config_lock() -> MutexGuard<'static, ()> {
        plan_test_lock()
    }

    fn init_plan_config() {
        set_openai_plan_codex_cli_version(TEST_CODEX_CLI_VERSION);
    }

    fn sse(events: &[Value]) -> Vec<u8> {
        let mut payload = String::new();
        for event in events {
            payload.push_str("data: ");
            payload.push_str(&serde_json::to_string(event).unwrap());
            payload.push_str("\n\n");
        }
        payload.push_str("data: [DONE]\n");
        payload.into_bytes()
    }

    fn test_auth() -> ResolvedAuth {
        ResolvedAuth {
            base_url: None,
            headers: vec![
                ("authorization".into(), AUTHORIZATION.into()),
                ("chatgpt-account-id".into(), ACCOUNT_ID.into()),
            ],
        }
    }

    #[test]
    fn build_body_uses_responses_shape() {
        let model = Model {
            id: "gpt-5.2-codex".into(),
            provider: crate::provider::ProviderKind::OpenAiCodingPlan,
            dynamic_slug: None,
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            pricing: ModelPricing {
                input: 0.0,
                output: 0.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
            max_output_tokens: 128_000,
            context_window: 272_000,
        };
        let body = build_body(
            &model,
            &[Message::user("hello".into())],
            "system",
            &json!([]),
            ThinkingConfig::Adaptive,
        );

        assert_eq!(body["model"], "gpt-5.2-codex");
        assert_eq!(body["instructions"], "system");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["include"], json!([INCLUDE_REASONING]));
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    }

    #[test]
    fn build_request_targets_codex_responses_with_required_headers() {
        let request = build_request(&json!({}), &test_auth(), "session-1", "request-1")
            .unwrap()
            .body(Vec::<u8>::new())
            .unwrap();

        assert_eq!(
            request.uri().to_string(),
            format!(
                "{}{RESPONSES_PATH}",
                ProviderKind::OpenAiCodingPlan.base_url()
            )
        );
        assert_eq!(request.headers()["authorization"], AUTHORIZATION);
        assert_eq!(request.headers()["chatgpt-account-id"], ACCOUNT_ID);
        assert_eq!(request.headers()[HEADER_ORIGINATOR], ORIGINATOR);
        assert_eq!(request.headers()[HEADER_SESSION_ID], "session-1");
        assert_eq!(request.headers()[HEADER_CLIENT_REQUEST_ID], "request-1");
    }

    #[test]
    fn build_models_request_targets_codex_models() {
        let _lock = config_lock();
        init_plan_config();
        let request = build_models_request(&test_auth())
            .unwrap()
            .body(())
            .unwrap();

        assert_eq!(
            request.uri().to_string(),
            format!(
                "{}{MODELS_PATH}?client_version={}",
                ProviderKind::OpenAiCodingPlan.base_url(),
                plan_codex_cli_version().unwrap()
            )
        );
        assert_eq!(request.headers()["authorization"], AUTHORIZATION);
        assert_eq!(request.headers()["chatgpt-account-id"], ACCOUNT_ID);
    }

    #[test]
    fn build_models_request_requires_initialized_config() {
        let _lock = config_lock();
        reset_plan_codex_cli_version();

        let error = build_models_request(&test_auth()).unwrap_err();

        assert!(matches!(error, AgentError::Config { .. }));
        assert_eq!(error.to_string(), PLAN_CONFIG_NOT_INITIALIZED_ERROR);
    }

    #[test]
    fn parse_sse_text_and_tool_call() {
        smol::block_on(async {
            let sse = sse(&[
                json!({
                    "type": "response.output_text.delta",
                    "delta": "Hello",
                }),
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "glob",
                        "arguments": "{\"pattern\":\"*.rs\"}",
                    },
                }),
                json!({
                    "type": "response.completed",
                    "response": {
                        "usage": {
                            "input_tokens": 10,
                            "input_tokens_details": {
                                "cached_tokens": 3,
                            },
                            "output_tokens": 7,
                        },
                    },
                }),
            ]);

            let (tx, rx) = flume::unbounded();
            let response = parse_sse(Cursor::new(sse), &tx).await.unwrap();

            assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
            assert_eq!(response.usage.input, 7);
            assert_eq!(response.usage.cache_read, 3);
            assert_eq!(response.usage.output, 7);
            assert!(matches!(
                &response.message.content[0],
                ContentBlock::Text { text } if text == "Hello"
            ));
            assert!(matches!(
                &response.message.content[1],
                ContentBlock::ToolUse { id, name, input }
                if id == "call_1" && name == "glob" && input["pattern"] == "*.rs"
            ));

            let events: Vec<_> = rx.try_iter().collect();
            assert!(matches!(
                &events[0],
                ProviderEvent::TextDelta { text } if text == "Hello"
            ));
            assert!(matches!(
                &events[1],
                ProviderEvent::ToolUseStart { id, name } if id == "call_1" && name == "glob"
            ));
        })
    }

    #[test]
    fn parse_sse_final_message_without_deltas() {
        smol::block_on(async {
            let sse = sse(&[
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {
                                "type": "output_text",
                                "text": "Done",
                            }
                        ],
                    },
                }),
                json!({
                    "type": "response.completed",
                    "response": {
                        "usage": {
                            "input_tokens": 1,
                            "output_tokens": 1,
                        },
                    },
                }),
            ]);

            let (tx, _rx) = flume::unbounded();
            let response = parse_sse(Cursor::new(sse), &tx).await.unwrap();
            assert_eq!(response.stop_reason, Some(StopReason::EndTurn));
            assert!(matches!(
                &response.message.content[0],
                ContentBlock::Text { text } if text == "Done"
            ));
        })
    }

    #[test_case(
        vec![("authorization".into(), "Bearer token".into())],
        ACCOUNT_ID_ERROR;
        "missing_account_id"
    )]
    #[test_case(
        vec![("chatgpt-account-id".into(), ACCOUNT_ID.into())],
        AUTHORIZATION_ERROR;
        "missing_authorization"
    )]
    fn validate_auth_rejects_invalid_headers(headers: Vec<(String, String)>, expected: &str) {
        let error = validate_auth(&ResolvedAuth {
            base_url: None,
            headers,
        })
        .unwrap_err();
        assert_eq!(error.to_string(), expected);
    }
}
