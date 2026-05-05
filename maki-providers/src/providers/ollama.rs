use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flume::Sender;
use futures_lite::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::model::{Model, ModelEntry};
use crate::provider::{BoxFuture, Provider};
use crate::{
    AgentError, ContentBlock, Message, ProviderEvent, Role, StopReason, StreamResponse,
    ThinkingConfig, TokenUsage,
};

use super::{ResolvedAuth, Timeouts, http_client, next_sse_line};

const HOST_ENV: &str = "OLLAMA_HOST";
const API_KEY_ENV: &str = "OLLAMA_API_KEY";
const CLOUD_BASE_URL: &str = "https://ollama.com";
const DEFAULT_BASE_URL: &str = "http://localhost:11434";
const HOST_NOT_SET: &str = "OLLAMA_HOST not set (or set OLLAMA_API_KEY for cloud)";

pub(crate) fn models() -> &'static [ModelEntry] {
    &[]
}

pub struct Ollama {
    client: HttpClient,
    stream_timeout: Duration,
    auth: Arc<Mutex<ResolvedAuth>>,
    system_prefix: Option<String>,
}

impl Ollama {
    pub fn new(timeouts: Timeouts) -> Result<Self, AgentError> {
        Self::from_env(
            timeouts,
            std::env::var(API_KEY_ENV).ok(),
            std::env::var(HOST_ENV).ok(),
        )
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: Timeouts) -> Self {
        Self {
            client: http_client(timeouts),
            stream_timeout: timeouts.stream,
            auth,
            system_prefix: None,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    fn from_env(
        timeouts: Timeouts,
        api_key: Option<String>,
        host: Option<String>,
    ) -> Result<Self, AgentError> {
        let base_url = match host {
            Some(h) => h.trim_end_matches('/').to_string(),
            None if api_key.is_some() => CLOUD_BASE_URL.into(),
            None => {
                return Err(AgentError::Config {
                    message: HOST_NOT_SET.into(),
                });
            }
        };
        let headers = match api_key {
            Some(key) => vec![("authorization".into(), format!("Bearer {key}"))],
            None => Vec::new(),
        };
        Ok(Self {
            client: http_client(timeouts),
            stream_timeout: timeouts.stream,
            auth: Arc::new(Mutex::new(ResolvedAuth {
                base_url: Some(base_url),
                headers,
            })),
            system_prefix: None,
        })
    }

    fn build_request(
        &self,
        method: &str,
        path: &str,
        auth: &ResolvedAuth,
    ) -> isahc::http::request::Builder {
        let base = auth.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
        let mut builder = Request::builder()
            .method(method)
            .uri(format!("{base}{path}"));
        for (key, value) in &auth.headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        builder
    }
}

fn convert_messages(messages: &[Message], system: &str) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len() + 1);
    if !system.is_empty() {
        out.push(json!({"role": "system", "content": system}));
    }

    for msg in messages {
        match msg.role {
            Role::User => {
                let mut text_parts: Vec<&str> = Vec::new();
                let mut images: Vec<String> = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => text_parts.push(text.as_str()),
                        ContentBlock::Image { source } => images.push(source.data.to_string()),
                        ContentBlock::ToolResult { content, .. } => {
                            out.push(json!({"role": "tool", "content": content}));
                        }
                        ContentBlock::ToolUse { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. } => {}
                    }
                }

                if !text_parts.is_empty() || !images.is_empty() {
                    let mut m = json!({"role": "user", "content": text_parts.join("\n")});
                    if !images.is_empty() {
                        m["images"] = Value::Array(images.into_iter().map(Value::String).collect());
                    }
                    out.push(m);
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut thinking = String::new();
                let mut tool_calls = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text: t } => text.push_str(t),
                        ContentBlock::Thinking { thinking: th, .. } => thinking.push_str(th),
                        ContentBlock::ToolUse { name, input, .. } => {
                            tool_calls.push(json!({
                                "function": {
                                    "name": name,
                                    "arguments": input,
                                }
                            }));
                        }
                        ContentBlock::ToolResult { .. }
                        | ContentBlock::Image { .. }
                        | ContentBlock::RedactedThinking { .. } => {}
                    }
                }

                if !text.is_empty() || !thinking.is_empty() || !tool_calls.is_empty() {
                    let mut m = json!({"role": "assistant", "content": text});
                    if !thinking.is_empty() {
                        m["thinking"] = Value::String(thinking);
                    }
                    if !tool_calls.is_empty() {
                        m["tool_calls"] = Value::Array(tool_calls);
                    }
                    out.push(m);
                }
            }
        }
    }

    out
}

fn convert_tools(anthropic_tools: &Value) -> Value {
    let Some(tools) = anthropic_tools.as_array() else {
        return json!([]);
    };

    Value::Array(
        tools
            .iter()
            .filter_map(|t| {
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name")?,
                        "description": t.get("description")?,
                        "parameters": t.get("input_schema")?,
                    }
                }))
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct ChunkMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<NativeToolCall>>,
}

#[derive(Deserialize)]
struct NativeToolCall {
    function: NativeToolFunction,
}

#[derive(Deserialize)]
struct NativeToolFunction {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Deserialize)]
struct ChatChunk {
    #[serde(default)]
    message: Option<ChunkMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    #[serde(default)]
    eval_count: Option<u32>,
    #[serde(default)]
    error: Option<String>,
}

async fn parse_ndjson(
    reader: impl AsyncBufRead + Unpin,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let mut lines = reader.lines();
    let mut text = String::new();
    let mut thinking_text = String::new();
    let mut tool_blocks: Vec<ContentBlock> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut stop_reason: Option<StopReason> = None;
    let mut is_first_content = true;
    let mut deadline = Instant::now() + stream_timeout;
    let mut next_tool_idx = 0_usize;

    while let Some(line) = next_sse_line(&mut lines, &mut deadline, stream_timeout).await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let chunk: ChatChunk = match serde_json::from_str(trimmed) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "failed to parse Ollama NDJSON chunk");
                continue;
            }
        };

        if let Some(err) = chunk.error {
            warn!(error = %err, "Ollama returned error in stream");
            return Err(AgentError::Api {
                status: 500,
                message: err,
            });
        }

        if let Some(msg) = chunk.message {
            if let Some(t) = msg.thinking
                && !t.is_empty()
            {
                thinking_text.push_str(&t);
                event_tx
                    .send_async(ProviderEvent::ThinkingDelta { text: t })
                    .await?;
            }
            if let Some(c) = msg.content
                && !c.is_empty()
            {
                let piece = if is_first_content {
                    is_first_content = false;
                    c.trim_start().to_string()
                } else {
                    c
                };
                if !piece.is_empty() {
                    text.push_str(&piece);
                    event_tx
                        .send_async(ProviderEvent::TextDelta { text: piece })
                        .await?;
                }
            }
            if let Some(calls) = msg.tool_calls {
                for call in calls {
                    let id = format!("ollama_call_{next_tool_idx}");
                    next_tool_idx += 1;
                    debug!(tool = %call.function.name, args = %call.function.arguments, %id, "tool call from Ollama");
                    event_tx
                        .send_async(ProviderEvent::ToolUseStart {
                            id: id.clone(),
                            name: call.function.name.clone(),
                        })
                        .await?;
                    tool_blocks.push(ContentBlock::ToolUse {
                        id,
                        name: call.function.name,
                        input: call.function.arguments,
                    });
                }
            }
        }

        if chunk.done {
            usage = TokenUsage {
                input: chunk.prompt_eval_count.unwrap_or(0),
                output: chunk.eval_count.unwrap_or(0),
                cache_read: 0,
                cache_creation: 0,
            };
            stop_reason = Some(match chunk.done_reason.as_deref() {
                Some("length") => StopReason::MaxTokens,
                _ => StopReason::EndTurn,
            });
            break;
        }
    }

    if !tool_blocks.is_empty() {
        stop_reason = Some(StopReason::ToolUse);
    }

    let mut content: Vec<ContentBlock> = Vec::new();
    if !thinking_text.is_empty() {
        content.push(ContentBlock::Thinking {
            thinking: thinking_text,
            signature: None,
        });
    }
    if !text.is_empty() {
        content.push(ContentBlock::Text { text });
    }
    content.extend(tool_blocks);

    Ok(StreamResponse {
        message: Message {
            role: Role::Assistant,
            content,
            ..Default::default()
        },
        usage,
        stop_reason,
    })
}

impl Provider for Ollama {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        _thinking: ThinkingConfig,
        _session_id: Option<&str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);

            let wire_messages = convert_messages(messages, system);
            let wire_tools = convert_tools(tools);
            let mut body = json!({
                "model": model.id,
                "messages": wire_messages,
                "stream": true,
                "options": {"num_predict": model.max_output_tokens},
            });
            if wire_tools.as_array().is_some_and(|a| !a.is_empty()) {
                body["tools"] = wire_tools;
            }

            let json_body = serde_json::to_vec(&body)?;
            let request = self
                .build_request("POST", "/api/chat", &auth)
                .header("content-type", "application/json")
                .body(json_body)?;

            debug!(model = %model.id, "Ollama native chat request");
            let response = self.client.send_async(request).await?;
            if response.status().as_u16() != 200 {
                return Err(AgentError::from_response(response).await);
            }
            parse_ndjson(
                BufReader::new(response.into_body()),
                event_tx,
                self.stream_timeout,
            )
            .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let request = self.build_request("GET", "/api/tags", &auth).body(())?;
            let mut response = self.client.send_async(request).await?;
            if response.status().as_u16() != 200 {
                return Err(AgentError::from_response(response).await);
            }
            let body: Value = serde_json::from_str(&response.text().await?)?;
            let mut models: Vec<String> = body["models"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m["name"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            models.sort();
            Ok(models)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::io::Cursor;

    const TEST_TIMEOUTS: Timeouts = Timeouts {
        connect: Duration::from_secs(10),
        low_speed: Duration::from_secs(30),
        stream: Duration::from_secs(300),
    };
    const TEST_STREAM_TIMEOUT: Duration = Duration::from_secs(300);

    #[test]
    fn from_env_without_host_or_api_key_errors() {
        match Ollama::from_env(TEST_TIMEOUTS, None, None) {
            Err(AgentError::Config { message }) => assert_eq!(message, HOST_NOT_SET),
            Err(other) => panic!("expected Config error, got {other:?}"),
            Ok(_) => panic!("expected error when host and api_key are None"),
        }
    }

    #[test]
    fn from_env_with_host_uses_bare_host() {
        let ollama = Ollama::from_env(TEST_TIMEOUTS, None, Some("http://x:1234".into())).unwrap();
        let auth = ollama.auth.lock().unwrap();
        assert_eq!(auth.base_url.as_deref(), Some("http://x:1234"));
        assert!(auth.headers.is_empty());
    }

    #[test]
    fn from_env_strips_trailing_slashes_from_host() {
        let ollama =
            Ollama::from_env(TEST_TIMEOUTS, None, Some("http://x:1234///".into())).unwrap();
        let auth = ollama.auth.lock().unwrap();
        assert_eq!(auth.base_url.as_deref(), Some("http://x:1234"));
    }

    #[test]
    fn from_env_with_api_key_uses_cloud() {
        let ollama = Ollama::from_env(TEST_TIMEOUTS, Some("test-key".into()), None).unwrap();
        let auth = ollama.auth.lock().unwrap();
        assert_eq!(auth.base_url.as_deref(), Some(CLOUD_BASE_URL));
        assert_eq!(auth.headers.len(), 1);
        assert_eq!(auth.headers[0].0, "authorization");
        assert_eq!(auth.headers[0].1, "Bearer test-key");
    }

    #[test]
    fn from_env_both_host_and_api_key_uses_host_with_auth() {
        let ollama = Ollama::from_env(
            TEST_TIMEOUTS,
            Some("test-key".into()),
            Some("http://local:1234".into()),
        )
        .unwrap();
        let auth = ollama.auth.lock().unwrap();
        assert_eq!(auth.base_url.as_deref(), Some("http://local:1234"));
        assert_eq!(auth.headers.len(), 1);
        assert_eq!(auth.headers[0].1, "Bearer test-key");
    }

    #[test]
    fn convert_messages_basic() {
        let messages = vec![
            Message::user("hello".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "hi".into() }],
                ..Default::default()
            },
        ];
        let wire = convert_messages(&messages, "be helpful");
        assert_eq!(wire[0]["role"], "system");
        assert_eq!(wire[0]["content"], "be helpful");
        assert_eq!(wire[1]["role"], "user");
        assert_eq!(wire[1]["content"], "hello");
        assert_eq!(wire[2]["role"], "assistant");
        assert_eq!(wire[2]["content"], "hi");
    }

    #[test]
    fn convert_messages_omits_empty_system() {
        let wire = convert_messages(&[Message::user("hi".into())], "");
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["role"], "user");
    }

    #[test]
    fn convert_messages_tool_round_trip_uses_native_shape() {
        let messages = vec![
            Message::user("list files".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "ollama_call_0".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"}),
                }],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "ollama_call_0".into(),
                    content: "file.txt".into(),
                    is_error: false,
                }],
                ..Default::default()
            },
        ];

        let wire = convert_messages(&messages, "sys");

        let assistant = &wire[2];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"], "");
        let calls = assistant["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].get("id").is_none(),
            "native API has no tool_call id"
        );
        assert_eq!(calls[0]["function"]["name"], "bash");
        assert_eq!(
            calls[0]["function"]["arguments"],
            json!({"command": "ls"}),
            "arguments must be object, not stringified JSON"
        );

        let tool_msg = &wire[3];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["content"], "file.txt");
        assert!(
            tool_msg.get("tool_call_id").is_none(),
            "native API matches tool results positionally"
        );
    }

    #[test]
    fn convert_messages_user_with_image_uses_images_field() {
        use crate::types::{ImageMediaType, ImageSource};
        use std::sync::Arc;
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc123"));
        let msgs = vec![Message::user_with_images("describe".into(), vec![source])];
        let wire = convert_messages(&msgs, "");
        assert_eq!(wire[0]["role"], "user");
        assert_eq!(wire[0]["content"], "describe");
        let images = wire[0]["images"].as_array().unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0], "abc123", "raw base64, no data URL prefix");
    }

    #[test]
    fn convert_tools_native_shape() {
        let anthropic = json!([{
            "name": "bash",
            "description": "Run a command",
            "input_schema": {
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"]
            }
        }]);
        let native = convert_tools(&anthropic);
        let tool = &native[0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "bash");
        assert_eq!(tool["function"]["description"], "Run a command");
        assert_eq!(tool["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn parse_ndjson_text_and_usage() {
        smol::block_on(async {
            let body = "\
{\"message\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"done\":false}
{\"message\":{\"role\":\"assistant\",\"content\":\" world\"},\"done\":false}
{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":12,\"eval_count\":3}
";
            let (tx, rx) = flume::unbounded();
            let resp = parse_ndjson(Cursor::new(body.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert_eq!(resp.usage.input, 12);
            assert_eq!(resp.usage.output, 3);
            assert_eq!(resp.usage.cache_read, 0);
            assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
            assert!(
                matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "Hello world")
            );
            assert!(!resp.message.has_tool_calls());

            let mut deltas = Vec::new();
            while let Ok(e) = rx.try_recv() {
                if let ProviderEvent::TextDelta { text } = e {
                    deltas.push(text);
                }
            }
            assert_eq!(deltas, vec!["Hello", " world"]);
        });
    }

    #[test]
    fn parse_ndjson_tool_calls() {
        smol::block_on(async {
            let body = "\
{\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"bash\",\"arguments\":{\"command\":\"ls\"}}}]},\"done\":false}
{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":7,\"eval_count\":4}
";
            let (tx, rx) = flume::unbounded();
            let resp = parse_ndjson(Cursor::new(body.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].0, "ollama_call_0");
            assert_eq!(tools[0].1, "bash");
            assert_eq!(tools[0].2["command"], "ls");

            let starts: Vec<_> = rx
                .drain()
                .filter_map(|e| match e {
                    ProviderEvent::ToolUseStart { id, name } => Some((id, name)),
                    _ => None,
                })
                .collect();
            assert_eq!(starts, vec![("ollama_call_0".into(), "bash".into())]);
        });
    }

    #[test]
    fn parse_ndjson_thinking() {
        smol::block_on(async {
            let body = "\
{\"message\":{\"role\":\"assistant\",\"thinking\":\"let me think\",\"content\":\"\"},\"done\":false}
{\"message\":{\"role\":\"assistant\",\"content\":\"hello\"},\"done\":false}
{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":2,\"eval_count\":2}
";
            let (tx, _rx) = flume::unbounded();
            let resp = parse_ndjson(Cursor::new(body.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();
            assert!(matches!(
                &resp.message.content[0],
                ContentBlock::Thinking { thinking, .. } if thinking == "let me think"
            ));
            assert!(matches!(
                &resp.message.content[1],
                ContentBlock::Text { text } if text == "hello"
            ));
        });
    }

    #[test]
    fn parse_ndjson_length_maps_to_max_tokens() {
        smol::block_on(async {
            let body = "\
{\"message\":{\"role\":\"assistant\",\"content\":\"x\"},\"done\":false}
{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"length\",\"prompt_eval_count\":1,\"eval_count\":1}
";
            let (tx, _rx) = flume::unbounded();
            let resp = parse_ndjson(Cursor::new(body.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();
            assert_eq!(resp.stop_reason, Some(StopReason::MaxTokens));
        });
    }

    #[test]
    fn parse_ndjson_error_chunk_returns_err() {
        smol::block_on(async {
            let body = "{\"error\":\"model 'foo' not found\"}\n";
            let (tx, _rx) = flume::unbounded();
            let err = parse_ndjson(Cursor::new(body.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap_err();
            match err {
                AgentError::Api { status, message } => {
                    assert_eq!(status, 500);
                    assert!(message.contains("not found"));
                }
                other => panic!("expected Api error, got {other:?}"),
            }
        });
    }
}
